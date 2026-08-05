# MultiFS VLC Streaming — Fix Plan

> Based on 10 problems catalogued in `vlc-streaming-problems.md`.
> Broken into 4 independent phases, each a self-contained goal.

---

## Phase 1: Unify Streaming — Single Code Path

**Problem addressed:** #3 (sequential loop), #4 (range truncation), #5 (two code paths)

**What's broken:**
- `stream_chunked_file_full` uses sequential `for` loop + `backend.download()` per chunk
- `stream_chunked_file_range` uses sub-agent's page-level streaming but truncates output
- Same intent (stream a file) goes through different code depending on Range header presence

**Fix:**
1. Rewrite `stream_chunked_file_full` as a thin wrapper that calls `stream_chunked_file_range(0, total_len)` with a clean delegation
2. Fix the truncation bug in `stream_chunked_file_range` (393KB for 160MB request)
3. Add tests: range path produces exactly the right number of bytes for both exact-range and full-file requests

**Tests needed:**
- `test_range_path_exact_match`: request bytes 0-50000, verify 50000 bytes received, content matches
- `test_range_path_mid_chunk`: verify range starting at offset 40MB produces correct slice
- `test_delegation_same_output`: verify stream_chunked_file_full and stream_chunked_file_range(0, total) produce identical output for same file

**Dependencies:** None — foundation phase

---

## Phase 2: Serve Header-Only for No-Range / Open-Ended Range

**Problem addressed:** #1 (parse_range maps bytes=0- to full file), #10 (file transfer vs streaming)

**What's broken:**
- VLC sends `Range: bytes=0-` or no Range to probe the header
- MultiFS interprets this as "download the entire 678MB file"
- VLC only needs ~1-2 chunks (header + Cues) to show metadata

**Fix:**
1. In `handle_get()`, detect "header probe" requests (no Range, or `bytes=0-` with no end)
2. Cap the response to first 32MB (1 chunk), or 2-3 chunks if Cues data spans beyond chunk 0
3. Add `Transfer-Encoding: chunked` or cap `Content-Length` to actual bytes served
4. Use `Content-Range: bytes 0-<sent>/<total>` (206) with actual byte count

**Tests needed:**
- `test_no_range_serves_at_most_first_chunk`: 5-chunk file, no Range → only 32MB received
- `test_range_bytes_0_dash_serves_at_most_first_chunk`: `Range: bytes=0-` on 5-chunk file → at most first chunk
- `test_explicit_range_still_works`: `Range: bytes=0-33554431` on 5-chunk file → exactly 32MB
- `test_content_range_matches_actual_bytes`: the Content-Range header bytes match what the body delivers

**Dependencies:** Phase 1 complete

---

## Phase 3: Eliminate Cold-Start Gap (First Byte Before VLC Timeout)

**Problem addressed:** #2 (response built before data), #6 (Content-Length mismatch), #7 (channel coupling), #8 (no timeout awareness), #9 (Content-Range mismatch)

**What's broken:**
- HTTP response headers sent immediately, body has 0 bytes for 4s
- pCloud CDN cold start: 4s delay before first data arrives
- VLC timeout: 3s w/o data → disconnects
- Nginx logs "206 0"

**Fix:**
1. Pre-warm first chunk: before building the HTTP response, start downloading chunk 0 from pCloud and wait for at least the first 16KB page to arrive in the page cache
2. Only then build and return the HTTP response
3. The body stream now has data immediately (from cache) while the rest of chunk 0 downloads
4. Alternative: use `Transfer-Encoding: chunked` so no Content-Length promise is made — each 16KB page can be sent as it arrives, with no advance guarantee of total size

**Tests needed:**
- `test_ttfb_within_500ms_with_cold_backend`: HTTP GET with simulated 3s backend latency → headers + data arrive within 500ms
- `test_response_returns_200_not_zero_when_streaming`: full integration test ensuring body has > 0 bytes
- `test_chunked_transfer_encoding`: verify Transfer-Encoding header is chunked for streaming responses

**Dependencies:** Phase 1, Phase 2

---

## Phase 4: Integration & VLC Verification

**Problem addressed:** End-to-end validation

**What's broken:**
- All previous phases fixed individual bugs; need to verify the whole system works

**Fix:**
1. Spin up MultiFS server in test with mock backends
2. Simulate VLC-like request patterns: HEAD for subtitles, GET with `bytes=0-`, GET with specific ranges
3. Deploy to production, test with real VLC client on BLOR-085
4. Verify seek bar works, playback starts within 2s

**Tests needed:**
- `test_vlc_pattern_head_then_get`: HEAD → GET `bytes=0-` pattern
- `test_seek_to_midpoint`: GET with range at 50% of file, verify data starts from correct offset
- `test_concurrent_streams`: two simultaneous requests for different ranges, verify both complete

**Dependencies:** Phase 1, 2, 3

---

## Dependency Graph

```
Phase 1 (Unify path, fix truncation)
    │
    ├──→ Phase 2 (Header-only serving)
    │        │
    │        ├──→ Phase 3 (Cold-start gap)
    │        │        │
    │        │        └──→ Phase 4 (Integration + VLC test)
    │        │
    │        └──→ Phase 4 (can verify header behavior independently)
    │
    └──→ (Phase 3 also depends on Phase 1 independently)
```

## Timeline Estimate

| Phase | Scope | Est. Duration |
|-------|-------|---------------|
| 1 | Unify code paths, fix truncation, 3 tests | ~1 session |
| 2 | Header-only serving, 4 tests | ~1 session |
| 3 | Cold-start elimination, 3 tests | ~1-2 sessions |
| 4 | Integration tests + VLC verification | ~1 session |
| **Total** | | **~4-5 sessions** |

## Decision Points (William to decide)

1. **Transfer-Encoding: chunked vs Content-Length**: Chunked means no advance size promise — VLC gets data as it arrives but no seek bar until enough data is buffered. Content-Length gives VLC an immediate total but requires knowing the exact bytes. Which approach?

2. **Header size**: How many chunks should a "header probe" serve? 1 chunk (32MB) is generous — a typical MKV header + Cues is <1MB. 1 chunk covers the worst case.

3. **Cues position**: All files were remuxed with `--cues 0:iframes` (Cues at start). So 1 chunk should always contain Cues. Confirm this is true for all remuxed files.

4. **Phase boundaries**: Are these dependencies and ordering right? Phase 2 doesn't strictly depend on Phase 1 — they could be parallel if we want to separate concerns.

5. **Two code paths vs one**: Phase 1 proposes delegating `stream_chunked_file_full` to `stream_chunked_file_range`. Alternative: keep them separate but apply Phase 2 caps to both. Which approach?

6. **Pre-warm vs Transfer-Encoding**: Phase 3 proposes pre-warming chunk 0 before building the response. Alternative: switch to chunked transfer encoding without pre-warming. The combination of both would be most robust. Priority?
