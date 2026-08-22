# MultiFS Audit Findings — 2026-08-22

Author: Nova (main agent) · Scope: `multifs audit`, storage reconciliation, orphan detection
Fix target: another agent. Everything below is read-only evidence gathered 2026-08-22 ~09:00–09:30 HKT.

---

## TL;DR

1. **`multifs audit scan` is broken** — returns a phantom `1 orphaned file, blank path, 0 B` for every account. Root cause: pCloud listing quirk (`folderid=0` / folder traversal). See Finding 1.
2. **`multifs shard status` "Objects" column is misleading** — it does not count multipart parts, so accounts that store only large `.mkv` objects show "0 objects" despite multi-GiB usage. See Finding 2.
3. **~52 GiB of pCloud usage is not accounted for by any DB table.** Most likely pCloud Trash (30-day retention) but unconfirmed. Needs a full reconciliation tool. See Finding 3.

---

## Environment facts (for the fixing agent)

| Thing | Value |
|---|---|
| Config | `/etc/multifs.toml` (also reachable via symlink `/etc/multifs/config.toml`) |
| Tokens | `/etc/multifs.env` (env vars `PCLOUD_TOKEN_*`, root-only, sourced by systemd + CLI) |
| DB | `/var/lib/multifs/meta.db` (SQLite, WAL mode, `wal_autocheckpoint=1000`) |
| Accounts | 47 total = 46 pCloud + 1 `local-disk` (80 GiB, empty) |
| Service | systemd `multifs.service`, runs `serve --config /etc/multifs.toml` |
| CLI | `/usr/local/bin/multifs` (subcommands: serve, init, check, config, account, bucket, object, shard, status, audit, import, checksum, fsck, vacuum, folder, link) |

CLI subcommands read config from the **default path** `/etc/multifs/config.toml` (they do NOT accept `--config`; only `serve` does). They require the token env vars — run as:

```bash
sudo bash -c 'set -a; source /etc/multifs.env; set +a; multifs <cmd>'
```

---

## Finding 1 — `audit scan` returns phantom "1 orphaned file"

### Symptom
For **every** account, `multifs audit scan <email>` reports:

```
Total files on pCloud: 1
Files managed by MultiFS: <0 or N>
Orphaned (not in MultiFS): 1
📄 Orphaned Files:
--------------------------------------------------------------------------------
                       <- blank path, 0 bytes
Total: 1 orphaned files
```

This is wrong. Accounts actually contain a `multifs/` tree (hundreds of files, GiB of data) plus pCloud default folders (`My Music/`, `My Pictures/`, `My Videos/`, `Getting started with pCloud.pdf`).

Also `multifs audit list-files <email>` returns the same phantom blank entry (path = empty, size = 0 B).

### Root cause (hypothesis, high confidence)
The listing is hitting the pCloud API quirk already documented in the workspace `TOOLS.md`:

> `listfolder` with `folderid: 0` returns empty content (use `path` instead)
> `listfolder` with `path: /` returns contents in `metadata.contents`

The scan appears to start from `folderid=0` (or otherwise mishandles the root), get an empty/phantom entry, and treat that single empty entry as "one orphaned file."

### Additional pCloud API notes (verified today)
- `listfolder?path=/multifs` returns `result: 0` with `metadata.contents` (top-level: `004`, `video`, …).
- `listfolder?path=/multifs&recursive=1` returns the **full tree NESTED** — folder entries carry their own `contents[]` arrays. It is **not** flat. (A naive single-level iteration over `metadata.contents` sees only directories and misses all files.)
- `listfolder?path=/&recursive=1` returns `result: 1101 "Invalid request"` (root path + recursive is rejected — use `path=/multifs` or a non-root path).
- `listtrash` returns **HTTP 404** on both `eapi.pcloud.com` and `api.pcloud.com` from this host. Could not measure Trash via API. (Either the method name differs, or pCloud changed it — please verify against current pCloud docs before relying on it.)

### Suggested fix
1. List from `path=/` (or `path=/multifs`) — never `folderid=0`.
2. Recurse properly: either walk `folderid` children one level at a time, or parse the **nested** `recursive=1` result recursively (recurse into folder entries' `contents[]`).
3. Ignore pCloud default folders (`My Music`, `My Pictures`, `My Videos`) and the `Getting started with pCloud.pdf` when reporting orphans — or at least flag them separately as "pCloud default, not MultiFS-managed."
4. The orphan definition should be: **pCloud file (under `/multifs/`) whose path is not referenced by any `versions.remote_path` NOR any `multipart_parts.pcloud_path`.** Note `multipart_parts` stores individual part files, so a multipart object is a *folder* of numbered parts, not a single file.

---

## Finding 2 — `shard status` "Objects" undercounts multipart objects

### Symptom
`multifs shard status` shows e.g.:

```
nova-video-004@atomicmail.ai   0 objects   3.1 GiB used
nova-video-001@atomicmail.ai   0 objects   1.8 GiB used
```

These accounts are not empty — they hold large `.mkv` objects stored as multipart part-folders. "Objects" appears to count only whole/single-file objects, not multipart parts.

### Evidence
`versions` table: 1601 rows = 75.85 GiB total, of which **104 rows are multipart objects** (`remote_path` contains `__mp__/multipart-…`, 67.99 GiB) and 1497 are single-file objects (7.86 GiB).

`multipart_parts` table: **8919 parts = 69.27 GiB across 104 distinct `upload_id`s.** Every one of the 104 `upload_id`s is referenced by a committed `versions` row (verified with a `NOT EXISTS` query → **0 orphaned multipart uploads**).

### Suggested fix
Make "Objects" (and any per-account counts) consistent: either count each committed `version` regardless of storage mode, or split into "objects" + "multipart parts". A "0 objects" row while GiB of data is present is what caused a false orphan alarm.

---

## Finding 3 — ~52 GiB of pCloud usage unaccounted by the DB

### Numbers (all measured 2026-08-22)
| Metric | Value |
|---|---|
| pCloud total `usedquota` (46 accounts, via `userinfo`) | **130.58 GiB** (208.00 GiB quota) |
| `multipart_parts` (physical part bytes) | 69.27 GiB |
| single-file `versions` (non-multipart committed) | ~7.86 GiB |
| `objects_legacy` legacy-only rows (311 rows, keys not in `files`) | ~0.72 GiB |
| **DB-accounted total** | **~77.9 GiB** |
| **Unaccounted on pCloud** | **~52 GiB** |

### DB table census
```
buckets           1
files          1591   (logical files: bucket,key -> current_version,size,etag)
versions       1601   (bucket,key,version -> account_email,remote_path,status)
objects_legacy  426   (old model: bucket,key -> account_email,remote_path)
files_legacy      0
chunks            0   (chunked/erasure storage NOT in use)
multipart_uploads 0
multipart_parts 8919  (upload_id,part_number,size,part_etag,pcloud_account,pcloud_path)
folder_meta     143
symlinks          0
```

`objects_legacy`: 426 rows total (10.41 GiB), but only **311 rows (0.72 GiB)** have keys NOT present in `files` (i.e. truly legacy-only, not migrated). The rest overlap with `files`/`versions` and should not be double-counted.

### Hypotheses for the ~52 GiB gap
1. **pCloud Trash (most likely).** pCloud keeps deleted files for 30 days (`userinfo.trashrevretentiondays = 30`) and they still count toward `usedquota`. The pipeline has been reprocessing/deleting many videos (e.g. dedup was removed 2026-07-15; reprocessing on 2026-08-22). If this is the cause, it self-clears within 30 days.
2. **True orphans.** Files uploaded to pCloud but never registered in the DB, or DB rows deleted without pCloud cleanup (would be permanent until cleaned).

### What's needed to resolve
A full reconciliation: list every file (recursively) under `/multifs/` on all 46 pCloud accounts, collect paths+sizes, and diff against the union of `versions.remote_path` + `multipart_parts.pcloud_path` (and `objects_legacy.remote_path`). Whatever is on pCloud but in no DB table = true orphan (excludes Trash, which `listfolder` does not return).

For the Trash portion, confirm the correct pCloud trash-listing API method (it is **not** `listtrash` on this host — 404) and measure Trash per account.

### Suggested tooling (for the fixing agent)
1. Fix `audit scan` (Finding 1) so it produces a real orphan report.
2. Add a `--reconcile` / `audit reconcile` mode that diffs full pCloud listing vs DB paths and reports true orphans per account with byte totals.
3. Consider reporting Trash usage per account (once the correct API is identified) so `shard status` / `status` can show "used = objects + trash + other."

---

## Related notes (not bugs, but context)

- **`vacuum --dry-run`** reports 0 pending / 0 superseded / 0 abandoned multipart — correct; there are none to reclaim. Do not expect `vacuum`/`fsck --fix` to recover the ~52 GiB gap (it is not DB-tracked).
- **Rebalance**: there is real skew (accounts `003/007/009/010/022` at 85–95%, new batch `031–036` at 10–23%), but `placement_strategy = "Utilization"` already routes new uploads to least-full accounts, so it self-corrects. Rebalancing is optional and should not run during active pipeline uploads.
- **SQLite integrity**: `PRAGMA quick_check` = `ok`, `freelist_count = 0`, 1006 pages. DB is healthy and compact; no SQLite `VACUUM` needed.

---

## Resolution (Nova, main agent — 2026-08-22 ~10:10 HKT)

All three findings are **fixed, deployed, and live-verified**. Details:

### Finding 1 — `audit scan` phantom orphan → FIXED
- Root cause confirmed: `isfolder` is a JSON **boolean**, but the scan read it
  with `.as_i64()` (always `None` → 0), so every folder was misclassified as a
  file; in `recursive=1` mode folders carry no `path`/`size` → blank-path 0 B phantom.
- Rewrote `src/cli/audit_cmd.rs`: correct recursive walk (bool `isfolder`,
  path reconstruction), plus the managed set now includes **both**
  `versions.remote_path` (all committed) **and** `multipart_parts.pcloud_path`.
- Verified: `nova-video-01` now reports 816 files / 226 matched / 590 orphaned (4.57 GiB) — no blanks.

### Finding 2 — `shard status` Objects undercount → FIXED
- Added `part_count` to `ShardStatus` + `count_parts_for_account()` and a
  **Parts** column. e.g. `nova-video-01` = 17 objects + 212 parts.
- Note: `nova-video-001`/`004` show 0 objects **and** 0 parts — genuinely empty
  in the DB (see Finding 3).

### Finding 3 — ~52 GiB gap → RESOLVED (true orphans, not Trash)
- New `multifs audit reconcile [--account <email>]` subcommand: lists every file
  under each mount prefix (single `recursive=1` call/account → ~46 calls, 29 s
  for the full run) and diffs vs the managed-path union.
- Full run result:
  - **16,746 files on pCloud · 10,001 matched · 6,745 orphans · 52.04 GiB**
  - The gap is **true orphaned files**, almost entirely `__mp__/multipart-*/N`
    part files from abandoned/failed uploads (leftover from reprocessing churn).
  - `nova-video-037` has no `/multifs/037` folder yet (new/empty account) — expected.
- Safety check: sampled 72 orphaned upload IDs across 5 accounts — **every one has
  `versions_ref=0` and `parts_ref=0`** (no committed object references them).
  → Reclaimable without data loss.

### Recommended next step (needs William's approval)
Build `multifs audit cleanup [--dry-run]` that, per account, deletes orphaned
`__mp__/multipart-<id>` dirs **only after** re-verifying the upload_id has zero
`versions` + `multipart_parts` references, using `deletefolderrecursive`. ~52 GiB
reclaimable. NOT auto-run — destructive.

---

*End of findings.*
