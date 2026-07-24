# Failure Behavior — MultiFS Dependency Failures

## pCloud Backend Failures

### Network / Connection Drop Mid-Operation

| Operation | Current Behavior | Desired Behavior |
|-----------|-----------------|-----------------|
| `upload()` drops mid-stream | Error propagates; partial file on pCloud, metadata not updated | ✅ Should discard partial pCloud file, return upload error to client |
| `download()` drops mid-stream | Error propagates; HTTP 500 returned | ✅ Acceptable. Should retry on connection reset |
| `copyfile()` times out | Error propagates | ✅ Acceptable |

### Authentication Failure

| Operation | Current Behavior | Desired Behavior |
|-----------|-----------------|-----------------|
| Invalid token on startup | Daemon fails to start with auth error | ✅ Acceptable — fail fast |
| Token expires mid-session | Next API call fails with 2094 error | ⚠️ Should log error and surface to user |
| Token revoked | 2094 on all operations | ⚠️ Should log and stop operations |

### Quota / Rate Limit

| Operation | Current Behavior | Desired Behavior |
|-----------|-----------------|-----------------|
| Upload exceeds account quota | pCloud returns error (2003) | ⚠️ Should select next account in rotation, not fail |
| pCloud returns 429 (too many requests) | Returns error to client | ⚠️ Should add retry with backoff |
| Account full but others have space | Upload fails | ⚠️ Should check next account automatically |

### Data Integrity

| Operation | Current Behavior | Desired Behavior |
|-----------|-----------------|-----------------|
| Chunk checksum mismatch on download | Currently NOT checked in download path | ❌ Must verify checksums and use erasure coding to reconstruct |
| Corrupted chunk returned by pCloud | Surfaces as garbage data to client | ❌ Must detect via SHA-256 checksum, fall back to erasure reconstruction |
| Partial file on pCloud (upload aborted) | Metadata not written, stale pCloud file | ⚠️ Orphaned files should be cleaned up |

## SQLite Metadata Database Failures

### Access Failures

| Failure | Current Behavior | Desired Behavior |
|---------|-----------------|-----------------|
| DB file is read-only | `with_conn` returns error, operation fails | ⚠️ Log detailed error, surface meaningful message |
| DB file locked by another process | SQLITE_BUSY returned after timeout | ⚠️ Add retry with backoff (WAL mode mitigates this) |
| DB file missing on startup | `MetadataDb::open` creates new DB | ✅ Acceptable for fresh setup |
| Disk full during write | SQLITE_FULL error propagates | ⚠️ Should log and return 507 Insufficient Storage |

### Corruption

| Failure | Current Behavior | Desired Behavior |
|---------|-----------------|-----------------|
| DB file corruption | Depends on corruption location — may crash or return garbage | ⚠️ Should run integrity check on startup |
| WAL corruption | WAL mode is resilient, but extreme cases fail | ⚠️ Should fall back to replay WAL |
| Concurrent write from multiple processes | WAL mode handles this, but busy timeout may fire | ⚠️ WAL handles concurrent readers/writers well |

### Data Consistency

| Failure | Current Behavior | Desired Behavior |
|---------|-----------------|-----------------|
| Chunk metadata written but file metadata write fails | Orphaned chunk records in DB | ❌ Must use transaction to write both atomically |
| File metadata written but backend upload fails | Orphaned metadata entries | ⚠️ Should clean up metadata on upload failure |
| Partial chunk list written | Incomplete file, download fails | ❌ Must use transaction for chunk list writes |

## Current Gap: No Transactional Safety

The upload path currently writes chunks to pCloud first, then registers metadata. If the metadata write fails after some chunks were uploaded, we get orphaned chunks on pCloud and/or orphaned metadata records.

**Fix needed:** Wrap the entire upload operation in a SQLite transaction. If anything fails, roll back the metadata and (for upload failures) record the orphaned chunks for cleanup.

## Current Gap: Checksum Verification

The download path does NOT currently verify chunk checksums against what's stored in the metadata. If pCloud returns corrupted data, it passes through to the client.

**Fix needed:** Before returning reassembled data, verify each chunk's SHA-256 checksum. If mismatch found, try reconstructing from parity chunks.
