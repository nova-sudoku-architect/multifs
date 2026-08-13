# Versioned File Update (MVCC Overwrite) — Target Design

**Status:** Proposed (not yet implemented)
**Date:** 2026-08-13
**Related:** `docs/architecture.md`, `src/storage/metadata.rs`, `src/storage/engine.rs`

---

## 1. Problem

Today, overwriting an existing object (PUT to an existing `(bucket, key)`) is
destructive and leaks storage:

- `put_object` / `put_object_stream` never looks up the old blob's location.
- `pick_backend()` is placement-driven (round-robin / utilization), **not**
  key-affine, so a re-upload frequently lands on a *different* pCloud account.
- `meta.put_object()` is `INSERT OR REPLACE`, so the metadata row is overwritten
  but the old pCloud blob is **never deleted** → orphaned forever (a silent
  storage leak), unless cleaned manually via `multifs audit`.

There is no S3 versioning either — `?versioning` returns a hardcoded
`<Status>Suspended</Status>` compat stub, and there are no version IDs.

## 2. Design Overview

Adopt **copy-on-write with explicit versions** (Postgres MVCC style):

1. Write the new data to **fresh** blob locations — never touch the existing blob.
2. Persist the new blob's metadata under a **pending** version as it lands.
3. Once all data is received, **atomically** flip the file's "current version"
   pointer to the new version.

A background **vacuum** job deletes versions nobody references anymore.

Because the pointer only moves at step 3, a reader always sees a complete old
version or a complete new version — never a torn one. Because we upload *before*
we commit, a crash mid-upload leaves orphaned blobs but no broken reference.

## 3. Naming Convention

Blobs are self-describing on pCloud so audit/vacuum can identify them by
filename alone (no DB join required):

```
{account_mount}/{bucket}/{key}.v{version}.c{chunk}
```

Example — `bucket=video`, `key=subtitle/foo.mkv`, version 3, chunk 1:

```
/multifs/video-00/video/subtitle/foo.mkv.v3.c1
```

- `c{chunk}` is always `1` for now (single-blob per version); chunking is a
  future add-on and the slot is reserved so no renames are needed later.
- **Long keys** (>255-byte filename after suffix): fall back to
  `{sha256(key)}.v{version}.c{chunk}`. The DB is authoritative for the key, so
  the pCloud name is cosmetic.
- **Parsing:** `v` / `c` prefixes make version/chunk unambiguous even when the
  key contains dots.

## 4. Data Model (Target Schema)

```sql
-- Logical file pointer: one row per (bucket, key)
CREATE TABLE files (
    bucket_name      TEXT NOT NULL,
    key              TEXT NOT NULL,
    current_version  INTEGER NOT NULL,       -- live version
    size             INTEGER NOT NULL DEFAULT 0,   -- denormalized current view
    etag             TEXT NOT NULL,
    last_modified    TEXT NOT NULL,
    content_type     TEXT,
    PRIMARY KEY (bucket_name, key)
);

-- One row per version. One version = one blob (single-blob model).
CREATE TABLE versions (
    bucket_name      TEXT NOT NULL,
    key              TEXT NOT NULL,
    version          INTEGER NOT NULL,       -- per-file version number
    size             INTEGER NOT NULL DEFAULT 0,
    etag             TEXT NOT NULL,
    last_modified    TEXT NOT NULL,
    content_type     TEXT,
    account_email    TEXT NOT NULL,          -- pCloud account holding the blob
    remote_path      TEXT NOT NULL,          -- the single blob location
    status           TEXT NOT NULL,          -- 'pending' | 'committed'
    created_at       INTEGER NOT NULL,       -- epoch ms (creation / upload start)
    superseded_at    INTEGER,                -- epoch ms; NULL while current
    PRIMARY KEY (bucket_name, key, version)
);
```

`chunks` table is **deferred** until chunking is implemented (see §11).

> **Time convention:** `created_at` / `superseded_at` are epoch milliseconds
> (INTEGER) for cheap `now - x` arithmetic in vacuum. `last_modified` stays an
> ISO-8601 / RFC 3339 string for S3 compatibility.

## 5. Write Path

Maps to the three-step proposal:

1. **Reserve** — atomically allocate `version = max(version) + 1` for
   `(bucket, key)` and insert a `versions` row with `status='pending'`,
   `remote_path = "{key}.v{version}.c1"`.
   *(Version allocated up-front, like a Postgres xid; aborted uploads just leave
   a gap in the numbering.)*
2. **Stream** — upload the body to that path via the pCloud backend
   (`upload_stream`). Existing blobs are never touched.
3. **Commit** — in a **single SQLite transaction**:
   - `UPDATE versions SET status='committed', size=?, etag=?, last_modified=?`
   - `UPDATE files SET current_version=?, size=?, etag=?, last_modified=?, content_type=?`
   - set the *previous* current version's `superseded_at = now`.

On error/cancel, the version stays `pending` and is swept by vacuum.

## 6. Read Path

- Resolve `files.current_version` → fetch that version's `remote_path` → stream
  from pCloud.
- Readers resolve the version **once at request start**, so an in-flight GET keeps
  streaming the old blob even after a concurrent commit flips the pointer.

## 7. Delete Path

- `DELETE` removes the `files` row (the pointer). The version's blob then becomes
  garbage and is swept by vacuum.
- No explicit blob delete at delete time; vacuum handles it.

## 8. Garbage Collection (`vacuum`)

Background job, idempotent, rate-limited against the pCloud API. Two sweeps:

1. **Pending sweep** — `status='pending' AND now - created_at > 1h`
   → delete blob + row (failed/abandoned uploads).
2. **Orphan sweep** — `superseded_at IS NOT NULL AND now - superseded_at > grace`
   → delete blob + row.

`superseded_at` is what protects in-flight readers: an old version becomes
vacuumable only after it has been superseded for the grace window.

## 9. Migration (from current schema)

Pure **metadata** migration — no pCloud blob is moved, renamed, re-uploaded, or
deleted. Existing blobs keep their current `remote_path` and simply become
`version = 1` of their file.

Run with the service stopped (offline) so no in-flight write races the copy.
Bump `schema_version` from `1` to `2` in `metadata.rs::run_migrations`.

```sql
-- 1. Create new tables alongside the existing `objects` (leave it intact)
CREATE TABLE files (...);      -- see §4
CREATE TABLE versions (...);   -- see §4

-- 2. Copy, in ONE transaction:
BEGIN;

INSERT INTO files (bucket_name, key, current_version, size, etag, last_modified, content_type)
  SELECT bucket_name, key, 1, size, etag, last_modified, content_type FROM objects;

INSERT INTO versions (bucket_name, key, version, size, etag, last_modified,
                      content_type, account_email, remote_path, status, created_at, superseded_at)
  SELECT bucket_name, key, 1, size, etag, last_modified,
         content_type, account_email, remote_path, 'committed', <now_epoch_ms>, NULL
  FROM objects;

COMMIT;

-- 3. Verify
SELECT (SELECT COUNT(*) FROM objects) AS old_rows, (SELECT COUNT(*) FROM versions) AS new_rows;
-- must be equal
```

**Naming mismatch (acceptable):** legacy version-1 blobs keep their old path
(`foo.mkv`, no suffix); only new writes get `.v{version}.c1`. The DB is
authoritative — `remote_path` is read from `versions`, never reconstructed from
the filename.

**Reversible:** keep `objects` (rename to `objects_legacy`) until the new path is
verified in production, then drop it in a later migration. Rollback = drop
`files`/`versions` + revert `schema_version`; blobs are untouched.

## 10. Defaults & Decisions

| Knob | Decision |
|------|----------|
| Version scope | **Per-file** (1, 2, 3…), not global |
| Chunking | **Single-blob** (Option A); enhance later |
| Concurrent PUT to same key | Last-writer-wins (atomic commit) |
| S3 versioning | Stays `Suspended` (safe overwrite, not exposed history) |
| Grace period | 10 min |
| Pending timeout | 1 h |

## 11. Future: Chunking

When chunking is introduced, add a `chunks` table keyed by
`(bucket_name, key, version, part_number)`, move `account_email`/`remote_path`
out of `versions`, and start writing `c1`, `c2`, … — no rename of existing blobs.
