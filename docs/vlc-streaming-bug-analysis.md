# Streaming Bug Analysis: VLC on MultiFS

## VLC Connection Trace

```
20:32:48-52  VLC probes 21 subtitle extensions (serial HEAD) — 6s wasted
20:33:04     GET /video/BLOR-085/blor-085.mkv → HTTP 206, 0 bytes sent
20:33:08     GET /video/BLOR-085/blor-085.mkv → HTTP 206, 65536 bytes
20:33:08     GET /video/BLOR-085/blor-085.mkv → HTTP 206, 0 bytes
20:33:14     GET /video/BLOR-085/blor-085.mkv → HTTP 206, 0 bytes
20:34:05     GET /video/BLOR-085/blor-085.mkv → HTTP 206, 90112 bytes
```

## Root Cause Chain

### 1. `parse_range("bytes=0-", total_len)` → `Some((0, total_len))`

VLC sends `Range: bytes=0-` to probe the file. The parser maps "no end bound"
to `end = total_len` (678,457,386). This passes through `get_object_stream` as
`Some((0, 678457386))`, routing to `stream_chunked_file_range(0, 678457386)`.

Result: ALL 21 chunks are downloaded, not just the header.

### 2. HTTP Response Built Before Data Ready

```rust
// handle_get() builds the response synchronously:
Response::builder()
    .header("Content-Length", "678457386")  // full file
    .header("Content-Range", "bytes 0-678457385/678457386")
    .status(206)
    .body(Body::from_stream(stream))  // stream reads from channel(16)
```

The response headers go to VLC immediately. But `get_object_stream` runs in a
spawned task. For cold pCloud access, the first page takes ~4s to arrive.
During that time, the response body produces **0 bytes**.

VLC receives headers but no body for 3+ seconds → disconnects → nginx logs "206 0".

### 3. Original Code's Sequential Chunk Processing

The DEPLOYED binary has the original `stream_chunked_file_full` (git checkout
reverted all optimizations). It uses a sequential `for ci in &chunks_info`
loop with `backend.download()` — blocks on each 32MB chunk. For 21 chunks
at ~4s each, that's ~84s before the full response completes.

### 4. Channel Capacity (16) Bottleneck

The channel between `get_object_stream` (spawned task) and the HTTP response
stream has capacity 16. With `backend.download()` returning 32MB at once,
this is OK for the in-flight chunk, but blocks the spawned task if the
receiver (HTTP body stream) isn't consuming fast enough.

### 5. Content-Length: 678MB in Response Header

VLC sees Content-Length: 678,457,386 and waits for all that data. It doesn't
know what fraction has arrived. With chunked transfer encoding, VLC would see
partial data arrival. With Content-Length, it expects the full file.

## Tests Needed

Each test MUST fail with the current (original) code and pass after the fix.

### Test: `test_no_range_serves_header_only_not_full_file`
- Upload 5-chunk file (160MB), request with no Range
- Assert: only first chunk's data is received through the channel (at most ~32MB)
- Current behavior: receives all 160MB (FAILS — proves the flaw)

### Test: `test_range_bytes_0_open_end_serves_header_not_full_file`
- Upload 5-chunk file, request with Range: bytes=0-
- Assert: at most first chunk's data (~32MB) is served through the stream
- Current behavior: parse_range maps to full file, all chunks sent (FAILS)

### Test: `test_first_page_arrives_before_full_chunk_download`
- Upload 3-chunk file, request with no Range
- Measure time from stream start to first page arrival
- Assert: TTFB < 500ms (with mock backend)
- Current behavior: original sequential `for` loop + `download()` means
  each chunk completes fully before the next, first page = first chunk's
  full data. Mock backend is instant, so this test may still pass.
  REAL pCloud: 4s per chunk → TTFB ~4s.

### Test: `test_response_headers_not_blocked_by_chunk_download`
- HTTP-level integration test: start server, make GET request
- Assert: HTTP status line arrives within 1s of connection
- Current behavior: response built before data ready, but server
  sends headers immediately — this test passes.
  REAL problem: headers arrive, then 4s gap until body data.

## Actual Production Measurement

| Stage | Time | What Happens |
|-------|------|-------------|
| VLC connects | T+0s | GET /video/... Range: bytes=0- |
| nginx proxies | T+0s | Passes to MultiFS on port 8080 |
| MultiFS responds | T+0.02s | HTTP 206 + Content-Length: 678MB ✅ |
| pCloud getfilelink | T+0-0.2s | API call to get download URL |
| pCloud CDN connect | T+0.2-0.4s | TLS + TCP to CDN host |
| pCloud first TCP chunk | T+0.4-4s | 16KB arrives from CDN |
| First page forwarded | T+4s | First data reaches VLC |
| VLC timeout | T+3s | VLC disconnected at 3s (never saw data) |

The gap: **MultiFS sends headers at T+0.02s but body data at T+4s.**
VLC times out at T+3s. Never sees any data.
