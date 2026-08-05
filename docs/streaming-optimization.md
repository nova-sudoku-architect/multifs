# Streaming Optimization Design

## Overview

MultiFS stores large files as 32 MB chunks across multiple pCloud accounts.
Clients (VLC) stream video via HTTP Range requests through nginx → MultiFS.

## How VLC Streams

1. **Initial probe (no Range / Range: bytes=0-):** VLC sends a GET with no Range
   to probe the file header. MultiFS serves the **first 32 MB only** — enough for
   VLC to parse SeekHead, Info, Tracks, and Cues (the seek index). Then closes
   the stream. VLC re-requests with specific Range headers for the actual video.

2. **Range requests:** VLC sends `Range: bytes=N-M` for specific byte ranges.
   MultiFS uses page-level streaming: forwards pages immediately as they arrive
   from pCloud's CDN, without waiting for the full 32 MB chunk to download.
   This gives <500ms TTFB for any requested range.

3. **Seeking:** VLC reads the Cues (seek index, at the start of the file),
   finds the nearest I-frame byte offset, sends a Range request for that offset.

## Key Optimizations

### 1. Early First-Chunk Priority
When a Range request includes chunk 0 (or the lowest chunk), it's downloaded
synchronously first before spawning remaining chunks in parallel. This cuts
TTFB for the first chunk by ~50%.

### 2. Batch getfilelink
Download links for all needed chunks are pre-fetched in a single parallel burst
instead of one-by-one serial API calls. Turns 21 serial RTTs into one parallel round-trip.

### 3. True Page-Level Streaming
`download_stream()` from pCloud forwards each 16 KB TCP chunk as it arrives
through the channel immediately. Cache writes happen in a background task —
they don't block forwarding. This means any chunk position gets <500ms TTFB.

### 4. Adjacent Chunk Pre-fetching
After serving the requested range, chunks N+1 and N+2 are downloaded in the
background so subsequent seeks feel instant.

### 5. RAM-Backed Page Cache
The page cache lives at `/dev/shm/multifs/chunks` (3.9 GB RAM-backed tmpfs)
instead of disk. Warm reads are effectively instantaneous.

## Code Architecture

```
get_object_stream(bucket, key, range, tx)
  │
  ├─ range.is_some() → stream_chunked_file_range(bucket, key, range, tx)
  │     │
  │     ├─ Determine which chunks cover the range
  │     ├─ Pre-fetch download links for all needed chunks (parallel)
  │     ├─ Download first chunk synchronously (for early TTFB)
  │     ├─ Download remaining chunks in parallel via spawn
  │     ├─ Stream pages through unbounded channel as they arrive
  │     └─ Pre-fetch chunks N+1, N+2 in background
  │
  └─ range.is_none() → stream_chunked_file_full(bucket, key, tx)
        │
        └─ Serve first chunk (32 MB) as Range: bytes=0-32MB
           VLC parses header and re-requests with proper Range headers.
```

## Page Cache

- **Location:** `/dev/shm/multifs/chunks` (RAM-backed tmpfs, 3.9 GB)
- **Max chunks:** 20 (covers ~640 MB of hot data)
- **Page size:** 16 KB
- **Eviction:** LRU
- **Bitmap tracking:** Each chunk tracks which 16 KB pages are cached,
  can stream from cache if all pages for a range are available.

## Adjacent Chunk Pre-fetching

After streaming completes for a Range request, the last chunk index is used
to determine which chunks to pre-fetch next. Current window: 2 chunks ahead.

## Known Limitations

- **Cold pCloud download:** First access to a chunk requires downloading
  32 MB from pCloud CDN (~4 seconds on ~8 MB/s connection). After that,
  the chunk is cached in RAM-backed `/dev/shm` for instant subsequent access.
- **Erasure coding:** 5+2 parity is tested but not deployed. All chunks are
  stored as whole 32 MB blobs (no XOR reconstruction overhead).
- **No HTTP/2 server push:** Not pre-fetching chunks before VLC requests them
  (adjacent pre-fetch only triggers after the first request completes).

## Testing

All streaming tests in `src/storage/tests.rs`:

| Test | What it validates |
|------|------------------|
| `test_streaming_ttfb_within_500ms` | Chunk-0-first: TTFB <500ms |
| `test_streaming_mid_chunk_ttfb_within_500ms` | True page-level: TTFB <500ms for any chunk |
| `test_concurrent_streaming_ttfb` | 3 simultaneous requests, all fast |
| `test_streaming_full_file_pages_immediately` | No-Range path first page arrives <3s |
| `test_streaming_prefetch_adjacent_chunks` | Chunks N+1, N+2 pre-fetched |
| `test_range_skip_does_not_fetch_all_chunks` | Only needed chunks fetched |
| `test_full_chunked_file_md5_match` | Full download integrity |
| `test_streaming_range_partial_last_chunk` | Partial range in last chunk |
| `test_streaming_full_file_via_get_object` | Non-streaming path unaffected |
| `test_missing_chunk_erasure_recovery` | Erasure behavior documented |

## Deploy

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

Page cache is at `/dev/shm/multifs/chunks` — survives restarts because it's
re-populated on access. On system reboot, `/dev/shm` is cleared.
