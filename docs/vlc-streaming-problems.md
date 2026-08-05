# VLC Streaming Problems — Problem Catalog

> Compiled 2026-07-27. No solutions. Just identification.

## Problem 1: `parse_range("bytes=0-", total)` → `(0, total)`

**File:** `src/server/mod.rs`, `parse_range()`

When VLC sends `Range: bytes=0-` (no end bound — it wants from byte 0 to EOF),
the parser maps this to `(0, total_len)`. For BLOR-085, that's `(0, 678457386)`.

This routes through `get_object_stream(Some((0, 678457386)))` → `stream_chunked_file_range(0, 678457386)`,
which downloads all 21 chunks. VLC only needs the first chunk (~32MB of header + Cues).

**Consequences:**
- Content-Length: 678,457,386 in response headers
- Content-Range: bytes 0-678457385/678457386
- All 21 chunk downloads initiated
- VLC waits for 678MB when it only needs header

**Test:** `test_range_bytes_0_dash_triggers_full_file_download` (written)

## Problem 2: HTTP Response Built Before Data Ready

**File:** `src/server/webdav/mod.rs`, `handle_get()`

```rust
let mut response = Response::builder()
    .header("Content-Length", content_length.to_string())  // computed BEFORE any data
    .header("Content-Range", ...)
    .status(206)
    .body(Body::from_stream(stream))  // stream reads from channel, currently empty
```

Axum returns this Response object immediately. Nginx receives the headers and
forwards them to VLC instantly. But `get_object_stream` runs in a **spawned tokio task**
separate from the HTTP handler. The spawned task hasn't started downloading from
pCloud yet when the headers are already at VLC.

For cold pCloud access: headers arrive at T+20ms, body data at T+4000ms.
VLC's timeout is ~3 seconds. VLC receives headers but no body, disconnects.

**Nginx log evidence:** `"GET /video/BLOR-085/blor-085.mkv HTTP/1.1" 206 0`

**Test:** `test_flaw_cold_first_byte_blocked_by_backend_latency` (not yet compilable)

## Problem 3: Sequential Chunk Processing in `stream_chunked_file_full`

**File:** `src/storage/engine.rs`, `stream_chunked_file_full()` (original code in deployed binary)

The original code iterates all chunks in a sequential `for` loop:
```rust
for ci in &chunks_info {
    backend.backend.download(&owned_path).await  // blocks on full 32MB per chunk
}
```

Each chunk takes ~4s from cold pCloud. 21 chunks = ~84s. No parallelism.

**Consequence:** Even after the first chunk's data arrives at the HTTP body stream,
chunks 1-20 still need to be downloaded sequentially before the response completes.
The response takes 84 seconds to finish, all with Content-Length: 678MB.

**Test:** `test_flaw_full_file_downloads_all_chunks_sequentially` (written)

## Problem 4: Optimized `stream_chunked_file_range` Truncation Bug

**File:** `src/storage/engine.rs`, `stream_chunked_file_range()` (sub-agent version)

When called with range `(0, 160MB)` on a 5-chunk file, only **393KB** were received
instead of 160MB. The page-buffering/slice-extraction logic in the optimized path
has a bug that truncates the output. This was discovered by running
`test_range_bytes_0_dash_triggers_full_file_download` — the test expected the full
file but got 0.2% of it.

**Consequence:** Even when the range path is used, it delivers incorrect data.
A VLC client receiving 393KB instead of 678MB would see corrupted video.

**Test:** `test_range_bytes_0_dash_triggers_full_file_download` (written — reveals the bug)

## Problem 5: Two Different Code Paths with Inconsistent Behavior

**File:** `src/storage/engine.rs`

The deployed binary has two streaming implementations with different capabilities and bugs:

| Path | Function | Method | Bug |
|------|----------|--------|-----|
| No Range | `stream_chunked_file_full` | Sequential `download()` | Downloads ALL chunks, 84s |
| Has Range | `stream_chunked_file_range` | Optimized `download_stream()` | Truncates output |

When VLC sends `Range: bytes=0-` (no end), `parse_range` maps to `(0, total)` →
goes through the Range path. When VLC sends no Range at all → goes through
the full-file path. Same VLC intent (probe header), different code paths,
different bugs.

**Consequence:** Behavior depends on exactly how VLC formats its request
(Range header presence, whitespace), not on what VLC actually needs.

**No direct unit test** — this is an architectural design issue, not a function bug.

## Problem 6: Content-Length Mismatch with Async Body

**File:** `src/server/webdav/mod.rs`, `handle_get()`

```rust
// Content-Length MUST match bytes produced by body stream exactly
// But body stream is asynchronous — bytes arrive later, possibly fewer
let content_length = match parsed_range {
    Some((start, end)) => end - start,  // computed from range, not from actual data
    None => total_len,
};
```

The Content-Length header is set to the **requested** byte count, not the
**delivered** byte count. If the stream produces fewer bytes (e.g., because
a pCloud chunk fetch fails, or the stream is truncated by the bug in Problem 4),
the HTTP client will wait for more bytes that never arrive.

**Consequence:** HTTP clients (VLC, Safari, curl) trust Content-Length.
If the body produces fewer bytes, the connection hangs until the client
times out. There's no mechanism for the server to say "I'm done" without
matching the promised Content-Length exactly.

**No direct unit test** — this is a protocol-level constraint violation.

## Problem 7: Channel(16) Couples HTTP Body to Backend Latency

**File:** `src/server/webdav/mod.rs`, `handle_get()`

```rust
let (tx, rx) = mpsc::channel::<Result<Bytes, anyhow::Error>>(16);

tokio::task::spawn(async move {
    engine_clone.get_object_stream(&b, &k, parsed_range, tx).await;
});
// Response body reads from rx via ReceiverStream
```

The channel connects the spawned task (producer) to the HTTP response body
(consumer). When the producer is blocked waiting for pCloud, the channel is
empty and the consumer produces nothing. The HTTP body's data flow is a
direct mirror of pCloud's responsiveness.

With capacity 16 (~256KB for 16KB pages), the channel acts as a small buffer.
But this doesn't help when no data is available for 4 seconds.

**Consequence:** The HTTP response body produces 0 bytes for 4 seconds.
VLC sees a response with headers but no data, times out at 3s.

**No direct unit test** — this is an integration concern between the HTTP
layer and the storage layer.

## Problem 8: No Client-Timeout Awareness

**File:** All layers (nginx → axum → MultiFS → pCloud)

No layer in the stack knows or cares about the client's timeout:
- MultiFS doesn't know VLC's HTTP timeout is ~3 seconds
- Nginx doesn't buffer anything on the video path (`proxy_buffering off`)
- The HTTP handler doesn't send interim/keepalive data during the cold-start gap
- pCloud's cold CDN connection has no deadline

Every layer assumes the next layer responds fast enough. When pCloud takes 4s
and VLC times out at 3s, every layer between them succeeds at its individual job
while the system as a whole fails.

**Consequence:** A 1-second gap between VLC's timeout (3s) and pCloud's
cold start (4s) causes the entire streaming session to fail. No layer
mitigates this gap.

## Problem 9: Content-Range Mismatch for Full-File Responses

**File:** `src/server/webdav/mod.rs`, `handle_get()`

When VLC sends `Range: bytes=0-`, the response includes:
```
HTTP 206 Partial Content
Content-Range: bytes 0-678457385/678457386
Content-Length: 678457386
```

This tells VLC: "I am sending you a 206 partial response, and the data
will be the entire 678MB file." VLC expects 678MB to arrive. But the
actual stream may:
- Only send the first 32MB (if the parse_range behavior changes)
- Send corrupted data (Problem 4 truncation bug)
- Send nothing for 4 seconds (Problem 2 cold start)
- Take 84 seconds to finish (Problem 3 sequential loop)

**Consequence:** VLC's assumptions about what's arriving are set by
the Content-Range header, but the actual data delivery doesn't match.

## Problem 10: System Treats Video Streaming as File Transfer

**File:** Architecture-wide

The entire stack (nginx → axum → MultiFS → pCloud) is designed for file transfer:
- Static Content-Length computed from file size
- HTTP 206 responses with exact byte counts
- No concept of "send enough data for header, then expect re-requests"
- No streaming metadata signalling (HLS, DASH manifests)

VLC's video streaming model is different:
- Probe: send a range-less GET to get header info
- Parse: read Cues to understand seekable positions
- Seek: send Range requests for specific byte ranges at I-frame boundaries
- Stream: request sequential ranges to buffer playback

MultiFS treats the probe as a full-file download. There's no distinction
between "file download" and "video header probe" in the architecture.

**Consequence:** Every VLC session starts with an unintended full-file
download. Even if all other bugs were fixed, the architecture would still
send ~32MB of header data when VLC only needs ~1MB to show metadata.

---

## Test Coverage vs Problems

| Problem | Test Written | Status |
|---------|-------------|--------|
| 1: parse_range(0-) → full file | `test_range_bytes_0_dash_triggers_full_file_download` | EXISTS — reveals Problem 4 bug |
| 2: Response built before data | `test_flaw_cold_first_byte_blocked_by_backend_latency` | NOT COMPILABLE — needs latency simulation |
| 3: Sequential chunk loop | `test_flaw_full_file_downloads_all_chunks_sequentially` | EXISTS — written |
| 4: Range path truncation | `test_range_bytes_0_dash_triggers_full_file_download` | EXISTS — reveals this as a side effect |
| 5: Two code paths | No unit test | Architectural — needs integration test |
| 6: Content-Length mismatch | No unit test | Protocol-level — needs HTTP-level test |
| 7: Channel(16) coupling | No unit test | Integration concern |
| 8: No timeout awareness | No unit test | E2E integration test needed |
| 9: Content-Range mismatch | No unit test | Protocol-level |
| 10: File transfer vs streaming | No unit test | Architectural |

## Known Test Issues

- `test_range_bytes_0_dash_triggers_full_file_download`: expected 160MB but got 393KB.
  This reveals Problem 4 (truncation bug) instead of Problem 1 (full file download).
  The test assertion needs to be adjusted: it should first test that the full
  range path works (sends correct number of bytes), and separately test that
  `parse_range` maps `bytes=0-` to full file.

- Most tests need the original `stream_chunked_file_full` code to be deployed.
  The optimized version (from the sub-agent) has different behavior and
  different bugs. Tests should cover both code paths.

- HTTP-level integration tests are needed for Problems 6, 7, 8, 9, 10.
  These require spinning up the server and making real HTTP requests.
