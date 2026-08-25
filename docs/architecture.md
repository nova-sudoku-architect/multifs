# MultiFS — System Architecture

> Version 0.3.0 | 2026-08-22 | Author: Nova Claw

## Overview

MultiFS is a multi-cloud storage pool written in Rust that aggregates multiple storage
backends into a **single S3-compatible API endpoint** (port 9000). Objects are distributed
across **47 pCloud OAuth accounts (~178 GB)** plus a **local disk backend (80 GB)** using
**single-blob storage with copy-on-write MVCC versioning**. Every object is stored as one self-contained blob on one account; overwrites create
a new version and atomically flip the file's "current version" pointer, never mutating the live
blob in place.

### Key Features

| Feature | Status | Description |
|---------|--------|-------------|
| S3 API | ✅ Live | ListBuckets, CreateBucket, Put/Get/Head/Delete Object, ListObjectsV2 |
| Single-blob storage | ✅ Live | Each object = one blob on one pCloud account (no chunking) |
| MVCC versioned overwrite | ✅ Live | Copy-on-write: new version + atomic pointer flip, old blob kept for grace period |
| Range streaming | ✅ Live | HTTP Range forwarded to pCloud CDN (start/end) |
| S3 multipart upload | ✅ Live | Initiate / UploadPart / Complete / ListParts / Abort; parts persisted and assembled |
| S3 copy & versioning | ✅ Live | CopyObject, UploadPartCopy, ListMultipartUploads, ListObjectVersions (`versionId`), PutBucketVersioning |
| `vacuum` GC | ✅ Live | Reclaims abandoned `pending` and superseded (orphaned) version blobs |
| `import` command | ✅ Live | Register an existing pCloud file into the DB (metadata only) |
| Content checksums | ✅ Live | SHA-256 stored per blob; `checksum rebuild|verify` detects in-place drift |
| `fsck` health check | ✅ Live | DB integrity + backend presence/size + optional checksum verify |
| Placement | ✅ Live | Tiered: cloud-first, local disk as last resort (per-account `priority`) |
| Read-only web UI | ✅ Live | GET-only browser navigator: list buckets/objects + download (port 9001) |
| Erasure coding | ❌ Stub | Not deployed (single-blob model; each blob lives on one account) |
| NFS | ❌ Stub | Port 2049 not exposed |

---

## Component Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     HTTP Handlers                        │
│  ┌──────────────────────────────────────────────────┐   │
│  │  S3 (axum) port 9000                             │   │
│  │  - list_buckets / create_bucket / delete_bucket  │   │
│  │  - put_object / get_object / head / delete       │   │
│  │  - multipart: initiate / uploadPart / complete   │   │
│  │  - listObjectsV2                                 │   │
│  └────────────────────────┬─────────────────────────┘   │
│                           ▼                              │
│           ┌────────────────────────┐                    │
│           │   StorageEngine        │                    │
│           │   - put_object(_stream)│                    │
│           │   - get_object(_stream)│                    │
│           │   - head_object        │                    │
│           │   - delete_object      │                    │
│           │   - multipart_*        │                    │
│           │   - vacuum / rebalance │                    │
│           └───────────┬────────────┘                    │
│                       │                                  │
│           ┌───────────┴───────────┐                     │
│           ▼                       ▼                      │
│  ┌──────────────┐       ┌──────────────────────────┐    │
│  │  MetadataDb  │       │  StorageBackend (trait)  │    │
│  │  (SQLite WAL)│       └───────────┬──────────────┘    │
│  └──────────────┘                   │                    │
│             ┌───────────────┬───────┴──────┬─────────┐  │
│             ▼               ▼              ▼         │  │
│    ┌──────────────┐ ┌──────────────┐ ┌─────────────┐ │  │
│    │ PCloudBackend│ │LocalDiskBack │ │ MockBackend │ │  │
│    │ (47 accounts) │ │ (80 GB prio1)│ │ (unit tests)│ │  │
│    └──────┬───────┘ └──────────────┘ └─────────────┘ │  │
│           ▼                                           │  │
│    ┌──────────────────────────────┐                   │  │
│    │  pCloud API (EU)             │                   │  │
│    │  uploadfile / getfilelink / deletefile           │  │
│    │  userinfo / createfolder / listfolder            │  │
│    │  CDN: edef*.pcloud.com        │                   │  │
│    └──────────────────────────────┘                   │  │
└─────────────────────────────────────────────────────────┘
```

---

## Data Model (SQLite)

Path: `/var/lib/multifs/meta.db` (WAL mode). Schema version **8** (latest: `buckets.versioning` added in migration 8).

```sql
-- Bucket registry
CREATE TABLE buckets (
    name TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    versioning     TEXT NOT NULL DEFAULT 'Suspended'  -- S3 versioning status
);

-- Logical file pointer: one row per (bucket, key)
CREATE TABLE files (
    bucket_name      TEXT NOT NULL,
    key              TEXT NOT NULL,
    current_version  INTEGER NOT NULL,       -- live version
    size             INTEGER NOT NULL DEFAULT 0,
    etag             TEXT NOT NULL,          -- SHA-256 hex (single blob)
    last_modified    TEXT NOT NULL,
    content_type     TEXT,
    charset          TEXT,                      -- detected charset for text (e.g. utf-8)
    checksum         TEXT NOT NULL DEFAULT '',   -- SHA-256 hex of live content
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
    charset          TEXT,                      -- detected charset for text (e.g. utf-8)
    account_email    TEXT NOT NULL,          -- pCloud account holding the blob
    remote_path      TEXT NOT NULL,          -- the single blob location
    status           TEXT NOT NULL,          -- 'pending' | 'committed'
    checksum         TEXT NOT NULL DEFAULT '',   -- SHA-256 hex of this blob
    created_at       INTEGER NOT NULL,       -- epoch ms (upload start)
    superseded_at    INTEGER,                -- epoch ms; NULL while current
    PRIMARY KEY (bucket_name, key, version)
);

-- In-flight multipart upload session
CREATE TABLE multipart_uploads (
    upload_id TEXT PRIMARY KEY,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    content_type TEXT,
    created INTEGER NOT NULL
);

-- Persisted parts for a multipart upload (kept after Complete for assembly)
CREATE TABLE multipart_parts (
    upload_id TEXT NOT NULL,
    part_number INTEGER NOT NULL,
    size INTEGER NOT NULL,
    part_etag TEXT NOT NULL,                 -- MD5 hex of part
    pcloud_account TEXT NOT NULL,
    pcloud_path TEXT NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);
```

> **Time convention:** `created_at` / `superseded_at` are epoch milliseconds for
> cheap `now - x` arithmetic in vacuum. `last_modified` stays an RFC 3339 string
> for S3 compatibility.

---

## MVCC Write Path (copy-on-write)

Maps to a three-step reserve → stream → commit:

1. **Reserve** — atomically allocate `version = max(version) + 1` for
   `(bucket, key)` and insert a `versions` row with `status='pending'`.
2. **Stream** — upload the body to a fresh blob via `upload_stream`. Existing
   blobs are never touched.
3. **Commit** — in a single SQLite transaction:
   - `UPDATE versions SET status='committed', size, etag, last_modified`
   - `UPDATE files SET current_version=?, size, etag, last_modified, content_type`
   - set the *previous* current version's `superseded_at = now`.

On error/cancel, the version stays `pending` and is swept by `vacuum`.

### Blob Naming

```
{account_mount}/{bucket}/{key}.v{version}.c1
```

- `c1` is always `1` (single-blob per version); the slot is reserved for future chunking.
- Long keys (>255-byte filename) fall back to `{sha256(key)}.v{version}.c1`.
- The DB (`versions.remote_path`) is authoritative — never reconstructed from filenames.

---

## Read Path

- `get_object`: resolve `files.current_version` → `versions.remote_path` →
  `backend.download`. If the remote path carries a multipart marker
  (`__mp__/multipart-<id>`), assemble the object by downloading and concatenating
  persisted parts in order.
- `get_object_stream`: resolve the current version; for multipart composites stream
  each part in order with range slicing across part boundaries; otherwise forward the
  range directly to the pCloud CDN (`download_stream`).
- Readers resolve the version **once at request start**, so an in-flight GET keeps
  streaming the old blob even after a concurrent commit flips the pointer.

---

## Read-Only Web UI

A browser file navigator (port 9001, **disabled by default**) exposing **GET-only** endpoints over
the same `StorageEngine` read path as S3. There is no write/delete/upload route, so the page is
safe to expose to a wider audience (e.g. over Tailscale) without risking data loss.

| Route | Method | Purpose |
|-------|--------|---------|
| `/` | GET | Single-file embedded UI (vanilla HTML+JS, no build step) |
| `/api/buckets` | GET | List buckets (name + created_at) |
| `/api/list?bucket=&prefix=` | GET | List a folder's direct children (prefixes + files + truncated flag) |
| `/api/download?bucket=&key=` | GET | Stream a file; honors HTTP Range for seeking/streaming |

Enabled via `server.enable_web = true` + `server.web_port` (default 9001). Folder grouping and
range slicing reuse `group_objects_by_prefix` and `parse_range` from the shared server module.
Image files (`.jpg`/`.png`/`.webp`/`.gif`) in a folder are rendered inline as a thumbnail gallery
so cover art and frame captures are visible without clicking. Text files are served with
`Content-Type; charset=utf-8` and `Content-Disposition: inline`, so UTF-8 subtitles render
directly in the browser. Charset is detected at upload (BOM / UTF-8 validation) and stored in the
`charset` column; unset values fall back to UTF-8 for text content.

### Folder preview page

In **Preview mode**, a folder that has recorded metadata (`folder_meta`) renders a per-folder
detail page: cover image → preview GIF → summary document. Metadata is recorded via
`multifs folder set-cover / set-summary / set-gif / backfill` (one cover, one GIF, and one
summary key per folder); a missing field or a field whose object no longer exists is skipped
(graceful degrade).

### Summary document schema (JSON)

The summary key may point to a `.json` file describing the video. The UI's `jsonToHtml()`
renders this schema into a structured card (unknown fields are ignored; invalid JSON degrades
to a raw `<pre>`). **Every value is tagged with its source(s)**: a field carries provenance as
`{value, sources: [{url, note}]}` (or `{canonical, sources: [...]}` for actors). If multiple
sources report the *same* value, they are merged into one entry tagged with all of them. A
source is rendered as a small link labelled with its hostname (tooltip = note).

- `title` — object with `zh_hk` (preferred) and/or `ja`; a differing `ja` is shown as 日文片名.
- `code` — 品番 (catalog code).
- `studio` — `{value, sources}` or a plain string (片商).
- `release_date` — `{value, sources}` or a plain string (發售日).
- `duration` — `{value, sources}` or a plain string (片長).
- `rating` — an array of rating objects, each `{value, scale, votes, source: {url, note}}`
  (a bare object is treated as a one-element array). Equal `value/scale` ratings from different
  sources are merged; each distinct rating is one 評分 row with stars + score + vote count and
  its source tag(s).
- `story` (故事) and `description` (簡介).
- `actors` — array of strings or `{canonical|name, sources: [...]}` objects (出演者), each
  tagged with its source(s).
- `comments` — `{summary, count, sources}` → 用家評論 (with count when present), source-tagged.
- `tags` — object mapping category → array of tag strings (標籤).
- `categories` — array of strings (分類).
- `sources` — array of `{url, note}` objects (來源), linked.

For a non-JSON summary (`.md`/`.txt`), the UI renders Markdown via a minimal safe renderer
(headings, tables, lists, code, blockquotes, links).

---

## Delete Path

`DELETE` removes the `files` row (the pointer). The version blob becomes garbage and is
swept by `vacuum` after the grace period. No explicit blob delete at delete time.

---

## Import (register existing files)

`multifs import <email> <remote-path> --bucket <b> [--key <k>]` registers a file that
already exists on pCloud (uploaded outside multifs) into the metadata DB — **metadata only,
no data download or movement**. It fetches size / modified / content-type via pCloud `stat`
and writes one committed version + file pointer at the existing remote path. Idempotent:
re-running on an already-managed path is a no-op, and it auto-creates the target bucket so
the object appears in ListBuckets / ListObjectsV2.

**Bulk scan:** `multifs import <email> --scan [--bucket <b>] [--prefix <p>] [--dry-run]`
recursively lists the account (via non-recursive `listfolder` walks, since recursive mode
omits the per-entry `path`) and imports every file not yet managed. It skips multipart
staging (`__mp__/`) and already-managed paths. Key = the full pCloud path with the leading
slash stripped; bucket defaults to `video`. `--dry-run` reports without writing.

> ⚠️ **Delete-safety:** an imported object's blob is indistinguishable from a multifs-owned
> blob. Deleting the multifs record (and later `vacuum`) will delete the source file from
> pCloud. Don't delete the multifs record for files that still matter elsewhere.

---

## Health Check (`fsck`)

`multifs fsck [--checksums] [--fix]` is a read-mostly integrity checker that runs five phases
and reports them in one pass:

1. **Database integrity** — dangling `files.current_version` pointers (no matching committed
   version), `files`↔`versions` mirror mismatches (size/etag/checksum), committed versions
   with no `files` row, and `pending` versions.
2. **Multipart state** — orphan `multipart_parts` (no upload row *and* unreferenced by any
   committed version) and abandoned `multipart_uploads` older than 24h.
3. **Backend presence + size** — `stat` every committed blob against its backend and compare
   size (cheap; no byte download).
4. **Content checksums** — optional (`--checksums`): recompute SHA-256 via the read path and
   compare to the stored checksum (slow; downloads every blob's bytes).
5. **GC state** — count of superseded versions awaiting `vacuum`.

`--fix` safely reclaims orphan multipart parts (deleting their part blobs) and runs `vacuum`;
it does **not** touch missing or size-mismatched blobs, which require manual re-linking.

---

## Garbage Collection (`vacuum`)

Background (the server runs a vacuum every 10 minutes) or on-demand
(`multifs vacuum [--dry-run]`), idempotent. Three sweeps:

1. **Pending sweep** — `status='pending' AND now - created_at > 1h` → delete blob + row.
2. **Orphan sweep** — `superseded_at IS NOT NULL AND now - superseded_at > 10min` → delete blob + row.
3. **Multipart sweep** — in-progress uploads whose `multipart_uploads.created` (epoch
   seconds) is older than 24h → `abort_multipart_upload` deletes each part blob, then the
   `multipart_uploads` + `multipart_parts` rows.

`superseded_at` protects in-flight readers: an old version becomes vacuumable only after
it has been superseded for the grace window. Completed multipart objects (no
`multipart_uploads` row) are never swept, so their retained parts survive for read assembly.

Multipart part deletion is **per-account**: `upload_part` picks a backend independently for
each part, so a single upload's parts can scatter across 2–4 backends. Reclaim groups parts
by account and deletes each account's `__mp__/multipart-<id>` folder (with a per-file
fallback). A failed delete keeps the DB rows so a later vacuum retries rather than silently
leaking bytes.

---

## S3 Multipart Upload

Each part is stored as one standalone blob under `{mount}/{bucket}/__mp__/{upload_id}/{part_number}`.

- **Initiate** (`POST /key?uploads`) → records `multipart_uploads` row, returns `<UploadId>`.
- **UploadPart** (`PUT /key?partNumber=N&uploadId=X`) → uploads the part blob, records
  `multipart_parts` row (account + path + MD5).
- **Complete** (`POST /key?uploadId=X`) → computes the S3 multipart ETag (MD5 of the
  concatenated part MD5s), stores the upload staging dir as the object's canonical
  `remote_path`, and commits the version. **The `multipart_parts` rows are retained** so
  subsequent GETs can assemble the object from its parts.
- **ListParts** (`GET /key?uploadId=X`) → returns the staged part numbers/sizes for
  rclone resume/verify.
- **Abort** (`DELETE /key?uploadId=X`) → deletes the staged part blobs and the
  `multipart_uploads` + `multipart_parts` rows. Idempotent; no-op on completed uploads.

---

## Backend Interface (StorageBackend Trait)

```rust
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn check_quota(&self) -> anyhow::Result<(i64, i64)>;
    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)>;
    async fn upload_stream(&self, remote_path: &str,
        stream: Box<dyn Stream<Item = Result<Bytes, Error>> + Send + Unpin>)
        -> anyhow::Result<(String, i64, String, i64)>;
    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>>;
    async fn download_stream(&self, remote_path: &str, range_start: Option<u64>,
        range_end: Option<u64>, tx: Sender<Result<Bytes, Error>>) -> anyhow::Result<()>;
    async fn delete(&self, remote_path: &str) -> anyhow::Result<()>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>>;
    async fn stat(&self, remote_path: &str) -> anyhow::Result<Option<i64>>;
    fn clone_box(&self) -> Box<dyn StorageBackend>;
}
```

### Implementations

| Backend | Status | Notes |
|---------|--------|-------|
| `PCloudBackend` | ✅ Complete | 47 accounts, EU API |
| `LocalDiskBackend` | ✅ Complete | Files under a root dir (`path`), used as overflow tier |
| `MockBackend` | ✅ Complete | In-memory HashMap, honors byte ranges, used by unit tests |
| `TrackedBackend` | ✅ Complete | Wraps any backend with call tracking + latency simulation |

### pCloud API Authentication

- **OAuth 2.0 flow**: `authorize` → `oauth2_token` exchange for `access_token`.
- **Token type**: Bearer — passed as `access_token=` form field or `Authorization: Bearer` header.
- **⚠️ NOT `auth=`** — that param is for login session tokens, not OAuth tokens.
- EU region endpoint: `eapi.pcloud.com`.

---

## Placement Strategy

Placement is driven by `placement_strategy` and a per-account `priority` field (lower =
preferred). Cloud backends default to `priority = 0`, local disk to `priority = 1`.

- **Utilization (default, tiered)** — distinct priority levels are filled ascending (lowest
  number first). Within a level, the least-full backend wins (`fill_ratio = used / total`).
  A level only spills to the next (higher) priority when every backend in the preferred
  level is full (`fill_ratio ≥ 1.0`, i.e. no free space). Net effect: pCloud absorbs all
  writes while it has any free space; local disk only receives overflow.
- **Round-Robin** — `AtomicUsize` counter, `idx = counter++ % backends.len()`, ignoring
  priority.

---

## CLI Interface

```
multifs serve [--config <path>]     Start the daemon
multifs init                        Initialize config + database
multifs check                       Validate config, test all accounts
multifs config show                 Print current config
multifs account list|add|check       Manage pCloud accounts
multifs bucket create|list|info      Manage buckets
multifs object cp|ls|rm|info         Manage objects
multifs shard status                 Show account fill levels
multifs audit scan|list-files        Find files not managed by MultiFS
multifs audit reconcile|cleanup      Diff/delete orphaned pCloud files vs the DB
multifs import <email> <path> \      Register an existing pCloud file (metadata only)
    --bucket <b> [--key <k>]
multifs import <email> --scan \      Bulk-import every unmanaged file in the account
    [--bucket <b>] [--prefix <p>] [--dry-run]
multifs vacuum [--dry-run]           GC superseded + abandoned version blobs + abandoned multipart uploads
multifs fsck [--checksums] [--fix]   Verify DB integrity + backend presence/size (+ optional checksums)
```

---

## Known Issues

### High
1. **No erasure coding** — each blob lives on a single account; an account failure
   loses those blobs. (Single-blob model is a deliberate simplification.)
2. **S3 body buffering** — uploads buffer the body in memory before streaming to pCloud.

### Medium
3. **No upload retry on pCloud errors** — quota-full (2008), rate limit (429), auth
   failure (2094) all fail immediately.

### Low
4. **Config still references `cache_chunks` / `cache_size_mb`** — legacy from the chunked
   architecture; harmless but unused.

---

## Test Coverage

All 127 lib tests pass (`cargo test --lib`). Highlights:
- `test_s3_multipart_part_body_is_consumed_and_stored` — multipart round-trip + assembly.
- `test_s3_multipart_roundtrip_stores_object` — total size + ETag correctness.
- `test_concurrent_streaming` — concurrent range streams (range-aware mock).
- `test_streaming_range_download` — range slicing (inclusive start / exclusive end).
- `test_abort_multipart_deletes_parts_from_all_backends` — abort reclaims parts
  scattered across backends (per-account delete, no cross-backend leak).
- `stream_hasher` — incremental SHA-256 ETag hashing.

---

## Configuration File

Path: `/etc/multifs.toml`

```toml
[server]
bind = "0.0.0.0"
s3_port = 9000
enable_s3 = true
enable_web = false        # read-only web UI (GET-only browser)
web_port = 9001

[storage]
meta_db_path = "/var/lib/multifs/meta.db"
placement_strategy = "Utilization"

[[storage.accounts]]
email = "nova-video-01@agentmail.to"
backend_type = "pcloud"
token_env = "PCLOUD_TOKEN_VIDEO_01"
mount_prefix = "/multifs/01"
quota_gb = 10
# ... 46 more pCloud accounts ...

[[storage.accounts]]
email = "local-disk"
backend_type = "local"
mount_prefix = "/multifs/local"
path = "/var/lib/multifs/disk"
quota_gb = 80
priority = 1
```

### Deploy

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

---

## References

| Resource | URL |
|----------|-----|
| pCloud API Documentation | https://docs.pcloud.com/ |
| pCloud OAuth 2.0 | https://docs.pcloud.com/methods/oauth_2.0/oauth2_token.html |
| S3 API Reference | https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html |
| HTTP Range (RFC 7233) | https://datatracker.ietf.org/doc/html/rfc7233 |
