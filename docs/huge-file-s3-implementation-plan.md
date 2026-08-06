# Implementation Plan — Support rclone copy of huge files over S3

Status: DRAFT
Date: 2026-08-06
Repo: `~/projects/multifs`
Branch target: `main`
Owner: Nova Video

## 1. Problem summary

rclone copying a huge file over S3 currently:

1. Buffers the **entire** object/part into RAM (`axum::body::to_bytes`) — no streaming write.
2. Triggers **many unnecessary serialized pCloud calls per write**:
   - `refresh_quotas()` → 8 × POST `/userinfo` on **every** file/part write (via `put_whole_file`, `put_chunked_file`, `upload_part_as_chunks`).
   - `ensure_path()` → POST `/listfolder` (+ `/createfolder`) on **every chunk** upload.
3. Writes each 32 MiB chunk as a **separate pCloud file** (`.ck.N` / `.mp.N`), so bandwidth is split into many tiny transfers, each with its own `listfolder + uploadfile` overhead → replicates slowly / hangs on a slow backend.

Observed symptom: rclone `SerializationError: empty response payload / EOF`, ~0 B/s, endless retries, no S3 response completing while the daemon churns through the pCloud call cascade.

## 2. Goals

- G1. Make huge-file S3 writes work reliably and at S3-expected throughput.
- G2. Eliminate unnecessary per-write pCloud calls (quota refresh, path ensure).
- G3. Preserve existing S3 conformance where already correct (Range GET/HEAD, ETag, multipart).
- G4. Add tests proving the new behavior without real pCloud accounts.

## 3. Non-goals / out of scope

- WebDAV / NFS.
- Changing the chunk size (32 MiB) or the chunked-on-pCloud storage model (a larger redesign; tracked separately in `architecture.md`).
- Download/GET streaming rework (already functional).

## 4. Proposed changes

### Change A — Cache quota-based placement on a timer (highest value)

Currently `refresh_quotas()` is called from `put_whole_file`, `put_chunked_file`, and `upload_part_as_chunks` on **every** write. It holds `cached_quotas` lock while doing 8 sequential network `/userinfo` calls.

**Design**
- Add a time-based cache: refresh quotas at most every `QUOTA_REFRESH_SECS` (default e.g. 60 s), and once on startup.
- `refresh_quotas()` becomes: if now - last_refresh < TTL → return immediately (no pCloud calls); else refresh (all accounts) and store timestamp.
- **Do not hold the `cached_quotas` lock across network awaits.** Take a snapshot/clone of the cached values to compute placement; hold the lock only to read/update in-memory values. Refreshing must not block writers.
- `pick_backend()` reads the cache (already does), but must not await a refresh while holding the lock. Ensure refresh and pick use the same lock discipline, or use a separate `tokio::sync::RwLock` / `OnceCell` for the cache.
- Add `cached_quotas` timestamp field. Consider `tokio::sync::Mutex` replaced by `RwLock` so concurrent reads don't serialize behind a writer.

**Files:** `src/storage/engine.rs`
**Risk:** low. Quota freshness degrades slightly (~TTL), placement stays near-best.

### Change B — Cache `ensure_path` so mkdir isn't done per chunk

`PCloudClient::ensure_path` runs POST `/listfolder` per call. `upload()` calls it before every chunk upload → hundreds of `/listfolder` per multi-GB file.

**Design**
- Add an in-memory "directory known to exist" cache keyed by `(account, parent_path)`.
- `ensure_path`: if `parent` in cache → return immediately (no pCloud call). Else `listfolder`; if the folder exists, record it and return; else `createfolder` recursively and record.
- Cache must be bounded (LRU or TTL) and per-account (paths differ per `mount_prefix`/account).
- Cleared on a `delete_bucket` / `delete_object` only if relevant.

**Files:** `src/storage/pcloud/client.rs` (+ maybe a small cache struct)
**Risk:** low. Only removes redundant round-trips.

### Change C — Stream the write path instead of full-body buffering (biggest architectural item)

This is the S3-faithfulness fix and is higher effort. Do it in phases.

**Phase C1 (minimal, low risk) — gate/chunk the buffered body:**
- Keep the buffered body (no rework of transport), but ensure the pCloud `uploadfile` uses a **chunked/spooled multipart stream** rather than one giant in-memory `Vec` copy per chunk (avoid duplicate RAM). Currently `uploadfile` builds `Part::bytes(data.to_vec())` → an extra copy.
- Also enforce that large PUTs funnel through the chunked/multipart path and never attempt a single >2 GiB `to_bytes`.

**Phase C2 (target) — true streaming:**
- Replace `axum::body::to_bytes(...)` in `put_object` with streaming: read the request `Body` as a byte stream, spool into 32 MiB chunks, and hand each chunk to the storage engine as it fills — no full-object buffer.
- For multipart (`upload_part_as_chunks`): stream the part body into chunks too, instead of receiving `&[u8]` of the whole part.

**What "implemented correctly" looks like for rclone:**
```
rclone (multipart)                    multifs S3                    pCloud
  POST ?uploads            → create_multipart_upload (DB)
  PUT ?partNumber=N&id=..  → stream body → chunks
                              [for each 32MiB chunk]
                                pick_backend (cache) 
                                ensure_path (cached) 
                                uploadfile (chunked stream)
                                DB record chunk meta
  POST ?uploadId=.. (Done) → stitch_multipart (DB-only re-map) → 200 + ETag
```
All pCloud calls are **only** the unavoidable `uploadfile` per chunk; `/userinfo` and `/listfolder` are amortized by Change A + B.

**Files:** `src/server/s3/mod.rs`, `src/storage/engine.rs`, `src/storage/pcloud/client.rs`

## 5. S3 conformance notes (record for the reviewer)

- ✅ Already correct: `GET`/`HEAD` with Range (`get_object`), ETag on put/parts, multipart initiate/upload/complete.
- ⚠️ Gap: **no true streaming** — whole-body buffering is not S3-faithful and is the main throughput/memory problem.
- ⚠️ Gap: `ListParts` may be missing/limited — rclone uses it; verify it's served.
- ⚠️ Gap: ensure complete returns correct multipart ETag (v2) and correct XML — already implemented; keep tests.
- ⚠️ Consider: rclone sets `X-Amz-Content-Sha256 UNSIGNED-PAYLOAD` / sends `Expect: 100-continue`; ensure the HTTP server handles those.

## 6. Ordering & effort

| # | Change | Effort | Depends | Value |
|---|--------|--------|---------|-------|
| A | Quota timer + lock fix | S | — | Very high |
| B | ensure_path cache | S | — | High |
| C1 | chunked pCloud upload, avoid extra copy | M | — | Medium |
| C2 | true streaming PUT | L | C1 | High (S3-faithfulness) |
| — | ListParts / S3 conformance checks | M | — | Medium |

Suggested sequence: **A → B → C1 → tests → C2** (C2 last, biggest).

## 7. Acceptance criteria

1. `refresh_quotas` runs at most once per TTL; a write never triggers >2 pCloud calls that would be avoidable (quota = 0 on a warm cache; one `listfolder` per new parent).
2. `put_object` of a >32 MiB body does **not** hold the full body in RAM for S3 conformance tests.
3. Huge-file rclone copy completes without `empty response payload / EOF` under the slow-backend condition.
4. Existing unit tests + new tests pass:
   - quota TTL (quota not re-fetched within window),
   - ensure_path not called twice for same parent,
   - streaming path produces byte-identical object to buffered path,
   - multipart complete still returns correct ETag/XML.
5. No regression in `cargo test --lib` (currently green at `7c8bab5`).

## 8. Test strategy

- Unit tests in `src/storage/tests.rs`, `src/*
  *hand*ler_tests.rs`, and `src/storage/pcloud/` using the existing `MockBackend` + a new mock that counts per-call pCloud HTTP activity (assert call counts per write).
- Integration: a `tokio::test` that does a large multipart upload through `build_router` (router tests already exist in `handler_tests.rs`) and asserts the object round-trips byte-identical and ETag correct.

## 9. Risks / open questions

- Quota TTL value: pick default, make configurable via `multifs.toml` (`placement.quota_refresh_secs`).
- Streaming body → the S3 handler currently also handles the no-Content-Length case with a 2 GiB cap; streaming changes that path.
- Ensure rollback safety: keep the old buffered paths behind the new streaming code; can toggle by env during rollout.

## 10. References

- `src/server/s3/mod.rs` — `put_object` (lines ~365-595).
- `src/storage/engine.rs` — `refresh_quotas` (~165), `put_whole_file` (~190), `put_chunked_file` (~215), `upload_part_as_chunks` (~346), `stitch_multipart` (~413).
- `src/storage/pcloud/client.rs` — `check_quota` (~29), `ensure_path` (~58), `upload` (~108), `download_stream` (~200).
- `docs/architecture.md`, `docs/chunking-plan.md`, `docs/streaming-optimization.md` (context).
- Design review (2026-08-06) — sequence of actions, unnecessary pCloud calls, S3 conformance.
