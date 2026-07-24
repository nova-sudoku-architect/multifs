# Chunking Architecture Plan

## Decisions (2026-07-24)
- Chunk size: **32 MB**
- Erasure: **5+2 Reed-Solomon** (5 data + 2 parity per stripe)
- No LRU cache in initial simplified version
- Tests before implementation
- Backward compat: existing whole-file objects remain readable

## Milestones

### Milestone A: Metadata Schema (2 days)
- [ ] Redesign SQLite schema: `files`, `chunks` tables
- [ ] Migration script: existing objects → `files` table as "whole-file" entries
- [ ] `MetadataDb` Rust API: create_file, get_file, add_chunk, get_chunks

### Milestone B: Chunk Manager — Tests First (5 days)
- [ ] Write tests for ChunkManager (split, assemble, checksum)
- [ ] Implement `ChunkManager::split(file, 32MB) → Vec<Chunk>`
- [ ] Implement `ChunkManager::assemble(chunks) → Vec<u8>`
- [ ] Implement `ChunkManager::checksum(data) → String`

### Milestone C: Erasure Coding — Tests First (3 days)
- [ ] Write tests for Reed-Solomon encode/decode
- [ ] Implement RS encode: 5 data → 5 data + 2 parity
- [ ] Implement RS decode: any 5 of 7 → original data
- [ ] Integrate with ChunkManager

### Milestone D: Placement Strategy — Tests First (1 day)
- [ ] Write tests for placement
- [ ] Implement round-robin across accounts
- [ ] Reconstruct from N-of-M survivors

### Milestone E: Engine Integration (2 days)
- [ ] Wire chunked path into Engine::put_object
- [ ] Wire chunked path into Engine::get_object
- [ ] Backward compat: whole-file objects still readable
- [ ] Update server-side copy for chunked files

### Milestone F: CLI Operations (1 day)
- [ ] `chunk status <bucket/key>` — show chunk distribution
- [ ] `chunk repair <bucket/key>` — reconstruct missing chunks
- [ ] `chunk migrate <bucket/key>` — convert whole-file to chunked

### Milestone G: Validation (1 day)
- [ ] Upload 200 MB test file → verify chunking
- [ ] Delete one account's chunk → verify reconstruction
- [ ] Benchmark parallel reads vs single-account reads

Total: ~15 working days
