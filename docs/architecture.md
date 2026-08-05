# MultiFS — System Architecture

> Version 0.1.0 | 2026-08-03 | Author: Nova Claw

## Overview

MultiFS is a multi-cloud storage pool written in Rust that aggregates multiple cloud storage
backends into a **single S3-compatible API endpoint** (port 9000) and **WebDAV endpoint** (port 8080).
Files are distributed across 8 pCloud OAuth accounts (~39 GB total), with automatic chunking
(32 MB split), parallel download/upload, and a RAM-backed page cache for streaming.

### Key Features

| Feature | Status | Description |
|---------|--------|-------------|
| S3 API | ✅ Live | ListBuckets, CreateBucket, Put/Get/Head/Delete Object, ListObjectsV2 |
| WebDAV | ✅ Live | GET, PUT, DELETE, PROPFIND, MKCOL, OPTIONS |
| Chunked storage (>32 MB) | ✅ Live | Split into 32 MB chunks, distributed across accounts |
| Parallel chunk download | ✅ Live | All chunks downloaded concurrently via tokio::spawn |
| Range streaming | ✅ Live | HTTP Range with page-level forwarding (<500ms TTFB) |
| Page cache | ✅ Live | 16 KB pages in `/dev/shm`, LRU eviction, 10 chunks max |
| Download deduplication | ✅ Live | Concurrent Range requests share chunk downloads |
| S3 multipart upload | ❌ Stub | Returns fake ETags, body not consumed → TCP stalls |
| Erasure coding | ❌ Stub | 5+2 XOR planned (not deployed) |
| WebDAV COPY/MOVE | ❌ Stub | COPY appends `_copy`, MOVE appends `_moved` |
| WebDAV LOCK/UNLOCK | ❌ Stub | Returns HTTP 200 with no state |
| NFS | ❌ Stub | Port 2049 not exposed |

---

## Component Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     HTTP Handlers                        │
│  ┌──────────────────┐  ┌──────────────────────────────┐ │
│  │  S3 (axum)       │  │  WebDAV (axum)               │ │
│  │  port 9000        │  │  port 8080                   │ │
│  │  - list_buckets   │  │  - webdav_root_handler      │ │
│  │  - put_object     │  │  - webdav_handler            │ │
│  │  - get_object     │  │    GET PUT DELETE PROPFIND   │ │
│  │  - head_bucket    │  │    MKCOL COPY MOVE OPTIONS   │ │
│  │  - create_bucket  │  │                              │ │
│  │  - delete_object  │  │                              │ │
│  └────────┬─────────┘  └──────────────┬───────────────┘ │
│           │                           │                  │
│           └───────────┬───────────────┘                  │
│                       ▼                                  │
│           ┌────────────────────────┐                    │
│           │   StorageEngine        │                    │
│           │   - put_object         │                    │
│           │   - get_object         │                    │
│           │   - get_object_stream  │                    │
│           │   - head_object        │                    │
│           │   - delete_object      │                    │
│           │   - put_chunked_file   │                    │
│           │   - get_chunked_file   │                    │
│           │   - stream_chunked_*   │                    │
│           │   - rebalance          │                    │
│           └───────────┬────────────┘                    │
│                       │                                  │
│     ┌─────────────────┼──────────────────┐              │
│     ▼                 ▼                  ▼              │
│ ┌─────────┐  ┌──────────────┐  ┌──────────────┐       │
│ │PageCache│  │DownloadTrack │  │  MetadataDb  │       │
│ │(/dev/shm│  │(dedup in-    │  │  (SQLite,    │       │
│ │ 16KB pgs│  │ flight chunk │  │   WAL mode)  │       │
│ │ LRU-10) │  │ downloads)   │  │              │       │
│ └─────────┘  └──────────────┘  └──────┬───────┘       │
└───────────────────────────────────────┼───────────────┘
                                        │
            ┌───────────────────────────┼───────────────┐
            │                           ▼                │
            │  ┌──────────────────────────────┐         │
            │  │   StorageBackend trait        │         │
            │  │   upload / download / delete  │         │
            │  │   download_stream / check_qta │         │
            │  └──────────────┬───────────────┘         │
            │                 │                          │
            │     ┌───────────┴───────────┐              │
            │     ▼                       ▼              │
            │ ┌──────────────┐  ┌──────────────────┐   │
            │ │ PCloudBackend│  │  MockBackend     │   │
            │ │ (8 accounts) │  │  (unit tests)    │   │
            │ └──────┬───────┘  └──────────────────┘   │
            │        │                                   │
            └────────┼───────────────────────────────────┘
                     │
                     ▼
        ┌────────────────────────┐
        │  pCloud API (EU)       │
        │  POST /uploadfile      │
        │  POST /getfilelink      │
        │  POST /userinfo        │
        │  POST /deletefile      │
        │  POST /createfolder    │
        │  POST /listfolder      │
        │  POST /copyfile        │
        │  GET /oauth2_token      │
        │  CDN: edef*.pcloud.com │
        └────────────────────────┘
```

---

## Data Model (SQLite)

Path: `/var/lib/multifs/meta.db` (WAL mode)

```sql
-- Bucket registry
CREATE TABLE buckets (
    name TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Whole-file objects (≤32 MB)
CREATE TABLE objects (
    bucket_name TEXT NOT NULL,
    key TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    etag TEXT NOT NULL,              -- SHA-256 hex
    last_modified TEXT NOT NULL,
    content_type TEXT,
    account_email TEXT NOT NULL,
    remote_path TEXT NOT NULL,
    PRIMARY KEY (bucket_name, key),
    FOREIGN KEY (bucket_name) REFERENCES buckets(name)
);

-- File metadata for chunked storage (>32 MB)
CREATE TABLE files (
    bucket_name TEXT NOT NULL,
    key TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    etag TEXT NOT NULL,              -- SHA-256 of full file
    last_modified TEXT NOT NULL,
    content_type TEXT,
    storage_type TEXT NOT NULL DEFAULT 'whole',  -- 'chunked' or 'whole'
    PRIMARY KEY (bucket_name, key)
);

-- Individual chunks of a chunked file
CREATE TABLE chunks (
    bucket_name TEXT NOT NULL,
    key TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    checksum TEXT NOT NULL,          -- SHA-256 of chunk data
    is_parity INTEGER NOT NULL DEFAULT 0,
    account_email TEXT NOT NULL,     -- Which pCloud account
    remote_path TEXT NOT NULL,       -- Path on pCloud
    PRIMARY KEY (bucket_name, key, chunk_index)
);
```

---

## Flow 1: Whole-File Upload (≤32 MB)

Used when body size ≤ CHUNK_SIZE (33,554,432 bytes = 32 MiB).

### Step-by-Step

| # | Step | Location | Input | Output | Side Effect |
|---|------|----------|-------|--------|-------------|
| 1 | Buffer HTTP body | `server/s3/mod.rs:put_object` | `Body` stream | `Bytes` (full buffer) | Memory alloc (size = file size) |
| 2 | Resolve content type | `server/mod.rs:resolve_content_type` | `key`, `Content-Type` header | `Option<String>` | — |
| 3 | `put_object_with_content_type` | `storage/engine.rs` | bucket, key, data, content_type | `Result<ObjectInfo>` | — |
| 4 | `ensure_bucket` | `storage/engine.rs` | bucket name | — | `INSERT INTO buckets` if new |
| 5 | Compute ETag | `storage/engine.rs` | `data` bytes | `String` (SHA-256 hex) | — |
| 6 | `put_whole_file` | `storage/engine.rs` | bucket, key, data, ct, etag, now | `ObjectInfo` | — |
| 7 | `pick_backend` | `storage/engine.rs` | backends list | backend index | `next_backend_idx++` (RR) or quota cache read (Util) |
| 8 | `backend.upload` | `storage/backends/pcloud.rs` | `remote_path`, `data` | `(actual_path, file_id)` | File on pCloud |
| 9 | `meta.put_object` | `storage/metadata.rs` | all fields | — | Row in `objects` table |

### pCloud API Calls (Step 8)

| Call | Endpoint | Purpose |
|------|----------|---------|
| `ensure_path()` | `POST /createfolder` | Create parent directories (idempotent, ignores 2004/2005) |
| Upload file | `POST /uploadfile` | Multipart form with `access_token`, `path`, `filename`, `nopartial=1` + file bytes |

### Assumptions (with pCloud API doc references)

1. **OAuth tokens never expire**: pCloud OAuth 2.0 tokens have no documented expiry. Multiple tests over weeks confirm they remain valid. Ref: [pCloud OAuth 2.0 docs](https://docs.pcloud.com/methods/oauth_2.0/oauth2_token.html)
2. **EU region API endpoint**: All accounts use `eapi.pcloud.com`. Must use `access_token` query param or `Authorization: Bearer` header, NOT `auth` (login session token). Ref: [pCloud API docs](https://docs.pcloud.com/)
3. **`nopartial=1` for uploads**: Prevents pCloud from creating partial-upload records. Ensures atomic upload — file either fully exists or doesn't. Ref: [uploadfile method](https://docs.pcloud.com/methods/file/uploadfile.html)
4. **Rate limits are bandwidth-based**: 50 sequential, 200 parallel, 1000 parallel calls all succeeded without 429s. No documented request-count limit. Ref: Tested empirically 2026-07-24.
5. **Quota is accurate at query time**: `userinfo` returns `usedquota` and `quota` fields that reflect current usage. May have slight propagation delay. Ref: [userinfo method](https://docs.pcloud.com/methods/general/userinfo.html)
6. **`stat` vs `getfilelink` consistency**: `stat` returns immediate metadata; `getfilelink` generates a temporary CDN link (valid ~1 hour). Both should reflect the same file. File creation may have sub-second propagation. Ref: [stat](https://docs.pcloud.com/methods/file/stat.html), [getfilelink](https://docs.pcloud.com/methods/file/getfilelink.html)

### Errors Handled

| Error Code | Meaning | Handler Behavior |
|-----------|---------|-----------------|
| 2004 | Folder already exists | OK (ignored, `ensure_path`) |
| 2005 | Parent folder missing | Continue recursion |
| 2009 | File not found (on download) | Propagate to client as 404 |
| 2008 | Over quota | Return error, account skipped |

### Unit Test Coverage

- `test_engine_put_get_object` — roundtrip with MockBackend
- `test_engine_round_robin_across_backends` — distribution
- `test_put_get_object` — MetadataDb operation

---

## Flow 2: Chunked Upload (>32 MB)

Used when body size > CHUNK_SIZE (33,554,432 bytes = 32 MiB).

### Step-by-Step

| # | Step | Location | Input | Output | Side Effect |
|---|------|----------|-------|--------|-------------|
| 1-6 | Same as whole-file upload | — | — | — | — |
| 7 | `chunk_manager::split` | `storage/chunk_manager.rs` | data, CHUNK_SIZE | `Vec<Chunk>` (index, data, checksum) | Each chunk has SHA-256 |
| 8 | `put_chunked_file` | `storage/engine.rs` | bucket, key, data, ct, etag, now | `ObjectInfo` | — |
| 9 | For each chunk: `pick_backend` | `storage/engine.rs` | backends | index | Round-robin/wrap across accounts |
| 10 | `backend.upload(chunk)` | `storage/backends/pcloud.rs` | chunk_path, chunk.data | `(actual_path, file_id)` | Chunk on pCloud |
| 11 | `meta.with_conn` (transaction) | `storage/metadata.rs` | — | — | `INSERT INTO files` + `INSERT INTO chunks` |

### ⚠️ KNOWN BUG: Silent Chunk Upload Failure

In Step 10, if `backend.upload()` fails for any chunk, the error is logged but the loop continues:

```rust
// engine.rs, put_chunked_file
Err(e) => {
    tracing::error!("Failed to upload chunk {}: {}", global_idx, e);
    // ← continues, does NOT abort, does NOT clean up
}
```

**Consequence**: The `files` table records the full file size, but `chunks` only has the successful ones. Download will fail with missing chunks. No cleanup of orphaned pCloud chunks.

**Fix needed**: On any chunk failure, abort, delete already-uploaded chunks from pCloud, and return error to client.

### Assumptions

1. **Chunks fit on individual accounts**: A 32 MB chunk fits on any account (minimum account quota: 4 GB). Max file size: ~8 accounts × 4 GB = limited by smallest account.
2. **Sequential upload is acceptable**: Chunks are uploaded one-by-one. For 150 MB (5 chunks × ~5s each ≈ 25s total). Parallel upload would speed this up.
3. **Chunk indices never collide**: `{bucket}/{key}.ck.{index}` path format, with UUID-v4 bucket names or timestamp-based suffixes.

### Unit Test Coverage

- `test_chunk_manager_split_*` / `_roundtrip_*` — chunk split/assemble
- `test_engine_delete_chunked_file` — chunked roundtrip + deletion
- `test_list_objects_includes_chunked` — mixed whole + chunked listing

---

## Flow 3: Whole-File Download

Used for objects stored as "whole" (≤32 MB).

### Step-by-Step

| # | Step | Location | Input | Output | Side Effect |
|---|------|----------|-------|--------|-------------|
| 1 | `get_object` | `storage/engine.rs` | bucket, key | `Vec<u8>` | — |
| 2 | Query `storage_type` | `storage/engine.rs` | bucket, key | `"whole"` or `"chunked"` | — |
| 3 | `meta.get_object` | `storage/metadata.rs` | bucket, key | `ObjectRecord` (account, path, size, etag) | — |
| 4 | `find_backend` | `storage/engine.rs` | account_email | `&BackendHandle` | — |
| 5 | `backend.download` | `storage/backends/pcloud.rs` | remote_path | `Vec<u8>` | — |
| 6 | `getfilelink` → CDN | `storage/pcloud/client.rs` | path | Temporary CDN URL | — |
| 7 | `GET CDN_URL` | reqwest | — | `Vec<u8>` (streamed then collected) | — |

### pCloud API Calls

| Call | Endpoint | Purpose |
|------|----------|---------|
| Get file link | `POST /getfilelink` | OAuth → temporary CDN URL valid ~1 hour |
| Download | `GET <CDN host>/<path>` | No auth needed (signed URL) |

### Assumptions

1. **CDN links are valid for ~1 hour**: pCloud's `getfilelink` returns a signed path on a CDN host. The link is valid for approximately 1 hour. No documented exact TTL. Ref: [getfilelink method](https://docs.pcloud.com/methods/file/getfilelink.html)
2. **CDN host may vary**: Response includes `hosts` array — first element is preferred. EU accounts typically return `edef*.pcloud.com`.
3. **No Range support needed for whole files**: Full download uses `getfilelink` → CDN GET without Range headers.

### Unit Test Coverage

- `test_engine_put_get_object` — roundtrip
- `test_engine_non_existent_object` — error path

---

## Flow 4: Chunked Download (Full File)

Used when `storage_type = "chunked"` and no Range header.

### Step-by-Step

| # | Step | Location | Input | Output | Side Effect |
|---|------|----------|-------|--------|-------------|
| 1 | `get_object_stream(None)` | `storage/engine.rs` | bucket, key | streamed bytes through `tx` | — |
| 2 | Query `storage_type` | `storage/engine.rs` | bucket, key | `"chunked"` | — |
| 3 | `stream_chunked_file_full` | `storage/engine.rs` | bucket, key, tx | streamed bytes | — |
| 4 | Query file size | `storage/engine.rs` | bucket, key | `file_size: i64` | — |
| 5 | Delegates to `stream_chunked_file_range(0, file_size)` | `storage/engine.rs` | — | — | Unified code path |
| 6 | Query `chunks_info` | `storage/engine.rs` | bucket, key | `Vec<ChunkInfo>` (index, account, path) | — |
| 7 | Spawn parallel download tasks | `storage/engine.rs` | chunk info | N spawned tasks | Tasks run concurrently |
| 8 | `stream_chunk_paged` per chunk | `storage/engine.rs` | chunk info, channels | Pages + sentinel through `page_tx` | Page cache populated |
| 9 | Assembly loop: collect pages per chunk | `storage/engine.rs` | `page_rx` stream | Sorted chunks through `tx` | `chunk_pages` HashMap drained |
| 10 | `send_chunk_data` | `storage/engine.rs` | Pages, index, range, tx | 64KB pages through `tx` | — |
| 11 | HTTP response body stream | `server/webdav/mod.rs` | `tx → ReceiverStream → Body` | HTTP response | — |

### ✅ FIXED: Sentinel Race Condition (2026-08-03)

**Original bug**: When a later chunk's sentinel arrived before an earlier chunk finished downloading, the drain loop `while let Some(pages) = chunk_pages.remove(&next)` would send incomplete page data for the earlier chunk.

**Fix**: Changed drain condition from `while let Some(pages) = chunk_pages.remove(&next)` to `while sentinel_rcvd.contains(&next)`. Chunks are only sent after their own sentinel has been received.

### Assumptions

1. **Sentinel always arrives after all pages**: In `stream_chunk_paged`, the empty Bytes sentinel is sent after the `while let Some(res) = dl_rx.recv().await` loop completes, ensuring all pages arrive before the sentinel.
2. **pCloud CDN delivers full chunks**: If `getfilelink` succeeds and CDN GET returns 200, the full chunk data is assumed to arrive. Partial downloads (connection drops) are not retried.
3. **Page cache in `/dev/shm`**: The server has 7.8 GB RAM, with ~3.9 GB allocated to `/dev/shm`. Each 32 MB chunk = 2048 pages × 16 KB. Max 10 cached chunks ≈ 320 MB.
4. **Channel backpressure**: `tx` channel has capacity 16 items (each ≤ 64 KB). Senders block when full; receiver (HTTP body stream) drains as client reads.

### Unit Test Coverage

- `test_full_chunked_file_md5_match` — 96 MB roundtrip integrity
- `test_streaming_full_file_via_get_object` — non-streaming path
- `test_streaming_full_file_pages_immediately` — immediate TTFB
- `test_no_range_serves_full_file_not_just_header` — full delivery verified

---

## Flow 5: Chunked Streaming Download (Range Request)

Used when `storage_type = "chunked"` and a Range header is present.

### Step-by-Step

| # | Step | Location | Input | Output | Side Effect |
|---|------|----------|-------|--------|-------------|
| 1 | Parse Range header | `server/mod.rs:parse_range` | `"bytes=N-M"`, file_size | `Option<(start, end)>` | — |
| 2 | `get_object_stream(Some(range))` | `storage/engine.rs` | bucket, key, range, tx | streamed bytes | — |
| 3 | `stream_chunked_file_range` | `storage/engine.rs` | range, tx | streamed bytes | — |
| 4 | Compute `first_chunk`, `last_chunk` | `storage/engine.rs` | range, CHUNK_SIZE | chunk indices | Filters chunks_info |
| 5 | Spawn parallel downloads for needed chunks only | `storage/engine.rs` | filtered chunks | spawned tasks | Only affected chunks downloaded |
| 6 | `stream_chunk_paged` per chunk | `storage/engine.rs` | — | Pages + sentinel | Page cache populated |
| 7 | Assembly with range slicing | `storage/engine.rs:send_chunk_data` | Pages, index, range | Correctly sliced bytes | Chunk-level offset math |
| 8 | HTTP 206 Partial Content | `server/*/mod.rs` | bytes sent, range | Response with Content-Range | — |

### Range Slicing Math (send_chunk_data)

```
chunk_offset = chunk_idx × CHUNK_SIZE
slice_begin = max(0, req_start - chunk_offset)
slice_end   = min(chunk_data.len(), req_end - chunk_offset - slice_begin)
```

Example: Request bytes 40 MB – 45 MB (chunk_size = 32 MB):
- `first_chunk = 40MB / 32MB = 1` (second chunk)
- `chunk_offset = 1 × 32MB = 32MB`
- `slice_begin = 40MB - 32MB = 8MB`
- `slice_end = min(chunk_len, 45MB - 32MB - 8MB) = min(chunk_len, 5MB)`

### Assumptions

1. **Only affected chunks are downloaded**: The `stream_chunked_file_range` function skips chunks outside `[first_chunk, last_chunk]`. Verified by `test_range_skip_does_not_fetch_all_chunks`.
2. **Parallel downloads share the page cache**: Concurrent Range requests for the same file share chunk downloads via `DownloadTracker`. All tasks write to the same page cache.

### Unit Test Coverage

- `test_streaming_ttfb_within_500ms` — chunk-0-first optimisation
- `test_streaming_mid_chunk_ttfb_within_500ms` — page-level forwarding
- `test_streaming_range_partial_last_chunk` — partial last chunk
- `test_range_skip_does_not_fetch_all_chunks` — selective chunk download
- `test_concurrent_streaming_ttfb` — parallel requests
- `test_streaming_prefetch_adjacent_chunks` — pre-fetch verification

---

## Flow 6: S3 Multipart Upload (STUB — Broken)

### Step-by-Step (Current Stub Behavior)

| # | Step | Handler | Input | Output | Status |
|---|------|---------|-------|--------|--------|
| 1 | Initiate | `POST /key?uploads` | — | Fake `<UploadId>` XML | ❌ No state stored |
| 2 | Upload part | `PUT /key?partNumber=N&uploadId=X` + body | Body bytes | Fake ETag | ❌ **Body NOT consumed → TCP stall** |
| 3 | Complete | `POST /key?uploadId=X` | — | Fake completion XML | ❌ No file created |

### Known Issue: Body Not Consumed

The handler for step 2 returns an HTTP response **without consuming the request body**:

```rust
// server/s3/mod.rs:put_object
if has_part_number {
    return Response::builder()
        .header("ETag", ...)
        .body(Body::empty())        // ← body NOT read
        .unwrap();
}
```

**Consequence**: When rclone (or any S3 client) sends data for the multipart part, the server sends HTTP 200 and closes the connection while the client is still transmitting. The client gets a TCP RST → connection hang → timeout. This blocks all S3 writes that use multipart upload.

### Assumption FAILED

The assumption that the body can be safely ignored is **invalid** for HTTP. A conforming server must drain the request body before responding. Reference: [RFC 7230 §3.3](https://datatracker.ietf.org/doc/html/rfc7230#section-3.3) — "A server that receives a request message with a Transfer-Encoding of chunked MUST process the chunked encoding."

---

## Flow 7: Bucket Operations

| Operation | HTTP Method | S3 Endpoint | WebDAV Endpoint |
|-----------|------------|-------------|-----------------|
| Create bucket | PUT | `/bucket` → 200 | `MKCOL /bucket` → 201 |
| List buckets | GET | `/` → XML | `PROPFIND /` → XML |
| Check bucket | HEAD | `/bucket` → 200 + x-amz-bucket-region | — |
| Delete bucket | DELETE | `/bucket` → 204 | — |

### Implementation Notes

- Buckets are purely SQLite records — no pCloud containers.
- `DELETE bucket` iterates all objects and deletes each one (individual pCloud `deletefile` calls).
- `HEAD bucket` returns `x-amz-bucket-region: us-east-1` for rclone compatibility.
- `GET bucket?location` returns `<LocationConstraint>us-east-1</LocationConstraint>`.
- `GET bucket?versioning` returns `<Status>Suspended</Status>`.

---

## Flow 8: Object Deletion

### Step-by-Step

| # | Step | Input | Output | Side Effect |
|---|------|-------|--------|-------------|
| 1 | Query `storage_type` | bucket, key | `"chunked"` or `"whole"` | — |
| 2a | **Chunked**: Query all chunk records | bucket, key | `Vec<(account, path)>` | — |
| 2b | For each chunk: `backend.delete(path)` | pCloud path | — | File removed from pCloud |
| 3a | **Whole**: `meta.get_object` then `backend.delete` | object record | — | File removed from pCloud |
| 4 | `meta.delete_object` | bucket, key | — | Rows removed from `objects`, `chunks`, `files` tables |

### Assumptions

1. **pCloud deletion is best-effort**: If `backend.delete()` fails for a chunk, the loop continues (deleting remaining chunks). The SQLite records are always removed. Orphaned pCloud files may remain.
2. **Delete is idempotent**: S3 DELETE returns 204 even if the object doesn't exist.

---

## Backend Interface (StorageBackend Trait)

```rust
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn check_quota(&self) -> anyhow::Result<(i64, i64)>;
    async fn upload(&self, remote_path: &str, data: &[u8]) -> anyhow::Result<(String, i64)>;
    async fn download(&self, remote_path: &str) -> anyhow::Result<Vec<u8>>;
    async fn download_stream(&self, remote_path: &str, range_start: Option<u64>,
        range_end: Option<u64>, tx: Sender<Result<Bytes, Error>>) -> anyhow::Result<()>;
    async fn delete(&self, remote_path: &str) -> anyhow::Result<()>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<StorageFile>>;
    async fn server_side_copy(&self, source_path: &str, dest_path: &str)
        -> anyhow::Result<Option<String>>;
    fn clone_box(&self) -> Box<dyn StorageBackend>;
}
```

### Implementations

| Backend | Status | Notes |
|---------|--------|-------|
| `PCloudBackend` | ✅ Complete | 8 accounts, EU API |
| `MockBackend` | ✅ Complete | In-memory HashMap, used by unit tests |
| `TrackedBackend` | ✅ Complete | Wraps any backend with call tracking + latency simulation |

### pCloud Backend Implementation

| Method | pCloud API Call | Auth |
|--------|----------------|------|
| `check_quota` | `POST /userinfo` | `access_token` form field |
| `upload` | `POST /uploadfile` | multipart form with `access_token`, `path`, `filename`, `nopartial=1`, file |
| `download` | `POST /getfilelink` → `GET <CDN>` | `access_token` then signed URL |
| `download_stream` | `POST /getfilelink` → CDN stream | Same as download but via `bytes_stream()` |
| `delete` | `POST /deletefile` | `access_token` + `path` form fields |
| `ensure_path` | `POST /createfolder` (recursive) | `access_token` + `path` |
| `copy_file` | `POST /copyfile` | `access_token`, `path`, `topath`, `toname` |

### pCloud API Authentication

- **OAuth 2.0 flow**: User authorizes at `https://my.pcloud.com/oauth2/authorize?client_id=...&response_type=code` → redirected with `code` → exchange at `https://eapi.pcloud.com/oauth2_token` for `access_token`.
- **Token storage**: Environment variables (`PCLOUD_TOKEN_VIDEO_01`, etc.) per account.
- **Token type**: Bearer token — passed as `access_token=<value>` form field or `Authorization: Bearer <value>` header.
- **⚠️ NOT `auth=`**: The `auth` param is for login session tokens (email+password login), not OAuth tokens.

Ref: [pCloud OAuth 2.0 documentation](https://docs.pcloud.com/methods/oauth_2.0/oauth2_token.html)

---

## Page Cache Design

| Property | Value |
|----------|-------|
| Location | `/var/cache/multifs/chunks` |
| Page size | 16 KB |
| Max chunks | 10 (configurable: `cache_chunks` in config) |
| Eviction | LRU (per-chunk access counter) |
| Tracking | Per-page bitmap (bit i = page i is cached) |
| Merge | Consecutive missing pages merged into single range |
| Notification | `tokio::sync::watch` channel per chunk (wake waiters) |

### Design Decision Notes

- **Page-level (not chunk-level)**: Pages are forwarded immediately as pCloud delivers them. Don't wait for the full 32 MB chunk.
- **Missing page merging**: Reduces `getfilelink` calls — one range per contiguous gap.
- **Sparse file support**: `write_partial` uses `seek` + `write` to create sparse files; `read_partial` checks for zero-fill at end.

---

## Download Tracker (Chunk Deduplication)

Prevents duplicate pCloud downloads when multiple concurrent Range requests hit the same chunk.

| API | Behavior |
|-----|----------|
| `try_register(bucket, key, chunk_idx)` | Returns `Ok(rx)` if first registration; `Err(rx)` if already pending |
| `complete(bucket, key, chunk_idx, result)` | Notifies all waiters via `watch::Sender` |
| `cancel(bucket, key, chunk_idx)` | Removes registration |

Each waiter gets a `watch::Receiver` and calls `.changed().await` to be notified when the download completes.

---

## Placement Strategy

Two strategies implemented:

### Round-Robin
- Debug mode (config: `placement_strategy = "round-robin"`)
- `AtomicUsize` counter incremented per upload
- `idx = counter++ % backends.len()`
- Even distribution guaranteed over time

### Utilization (Default)
- Production mode (`placement_strategy = "utilization"`)
- Cached fill ratios refreshed on demand
- Picks backend with lowest `fill_ratio = used_bytes / total_bytes`
- Fill cache updated in-memory after successful uploads (avoids extra API call)

---

## CLI Interface

```
multifs serve [--config <path>]     Start the daemon
multifs init                        Initialize config + database
multifs check                       Validate config, test all accounts
multifs status                      Show daemon health + account stats
multifs config show                 Print current config
multifs account list|add|check       Manage pCloud accounts
multifs bucket create|list|info      Manage buckets
multifs object cp|ls|rm|info         Manage objects
multifs shard status                 Show account fill levels
multifs audit                        Find orphaned pCloud files
```

---

## Known Issues

### Critical
1. **S3 multipart upload stubs** — Body not consumed, causing TCP stalls. Blocks all S3 clients using multipart upload (rclone default, aws-cli for >64 MB).
2. **Silent chunk upload failure** — `put_chunked_file` continues on error, leaving orphaned chunks and incomplete file records.

### High
3. **No erasure coding** — Single chunk failure makes the entire file unrecoverable. XOR-based 5+2 was tested but not deployed.
4. **S3 body buffering** — `axum::body::to_bytes()` buffers entire upload body in memory. 756 MB file requires 756 MB RAM allocation.
5. **No transactional safety in upload** — Chunk metadata and file metadata are not in a SQLite transaction. Crash mid-upload leaves inconsistent state.

### Medium
6. **WebDAV COPY/MOVE** — COPY appends `_copy`, MOVE appends `_moved`. Need Destination header parsing.
7. **WebDAV PROPPATCH/LOCK/UNLOCK** — Stub implementations returning HTTP 200.
8. **No upload retry on pCloud errors** — Quota-full (2008), rate limit (429), auth failure (2094) all fail immediately.

### Low
9. **README still references `pcloudfs`** (old binary name).
10. **Config docs say 3 accounts** — currently 8 configured.

---

## Test Coverage Summary

| Module | Unit Tests | Integration Tests | Coverage |
|--------|-----------|-------------------|----------|
| `chunk_manager` | 8 | — | ✅ Full |
| `placement` | 9 | — | ✅ Full |
| `MetadataDb` | 10+ | — | ✅ Full |
| `StorageEngine` (small) | 5 | — | ✅ Full |
| `StorageEngine` (chunked) | 4 | — | ✅ Full |
| `StorageEngine` (streaming) | 8 | — | ✅ Full |
| `PageCache` | 14 | — | ✅ Full |
| `DownloadTracker` | 6 | — | ✅ Full |
| S3 handler (non-multipart) | — | 1 (@ignore) | ⚠️ Manual only |
| WebDAV handler | 14 (format only) | 1 (@ignore) | ⚠️ Format only |
| S3 multipart handler | 2 (XML format) | — | ❌ No body consumption test |
| `put_chunked_file` error path | — | — | ❌ No test |

### Running Tests

```bash
# All unit tests
cargo test

# Specific test file
cargo test --test integration_test

# With output
cargo test -- --nocapture
```

---

## Configuration File

Path: `/etc/multifs.toml`

```toml
[server]
bind = "0.0.0.0"
s3_port = 9000
webdav_port = 8080
enable_nfs = false
enable_webdav = true
enable_s3 = true

[tls]
enabled = true
cert_path = "/etc/pcloudfs/ssl/vmi3137694.tailb9bfd3.ts.net.crt"
key_path = "/etc/pcloudfs/ssl/vmi3137694.tailb9bfd3.ts.net.key"
domain = "vmi3137694.tailb9bfd3.ts.net"

[storage]
meta_db_path = "/var/lib/multifs/meta.db"
cache_path = "/var/cache/multifs"
cache_size_mb = 512
cache_chunks = 10
placement_strategy = "Utilization"

[[storage.accounts]]
email = "nova-video-01@agentmail.to"
backend_type = "pcloud"
token_env = "PCLOUD_TOKEN_VIDEO_01"
mount_prefix = "/multifs/01"
quota_gb = 10
# ... 7 more accounts
```

### pCloud Token Environment Variables

All tokens stored in `~/.openclaw/.env`:
- `PCLOUD_TOKEN_VIDEO_01` through `PCLOUD_TOKEN_VIDEO_22`
- `PCLOUD_APP_CLIENT_ID` / `PCLOUD_APP_CLIENT_SECRET` (OAuth app)

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
| pCloud uploadfile | https://docs.pcloud.com/methods/file/uploadfile.html |
| pCloud getfilelink | https://docs.pcloud.com/methods/file/getfilelink.html |
| pCloud userinfo (quota) | https://docs.pcloud.com/methods/general/userinfo.html |
| pCloud stat | https://docs.pcloud.com/methods/file/stat.html |
| pCloud deletefile | https://docs.pcloud.com/methods/file/deletefile.html |
| pCloud createfolder | https://docs.pcloud.com/methods/folder/createfolder.html |
| pCloud copyfile | https://docs.pcloud.com/methods/file/copyfile.html |
| HTTP Range (RFC 7233) | https://datatracker.ietf.org/doc/html/rfc7233 |
| HTTP Chunked Transfer (RFC 7230) | https://datatracker.ietf.org/doc/html/rfc7230#section-3.3 |
| S3 API Reference | https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html |
| WebDAV (RFC 4918) | https://datatracker.ietf.org/doc/html/rfc4918 |
