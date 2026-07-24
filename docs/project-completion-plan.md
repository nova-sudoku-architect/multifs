# MultiFS — Project Completion & Enhancement Plan

## Current Status (as of 2026-07-24 16:38)

| Component | Status |
|-----------|--------|
| **Project** | Renamed from `pcloudfs` → `multifs` |
| **GitHub** | https://github.com/nova-sudoku-architect/multifs ✅ |
| **StorageBackend trait** | `src/storage/backends/mod.rs` — abstracted ✅ |
| **pCloud backend** | Wrapped behind trait ✅ |
| **Engine** | Uses `Vec<Box<dyn StorageBackend>>` ✅ |
| **Binary** | `/usr/local/bin/multifs` installed ✅ |
| **Service** | Running via `nohup` on Tailscale IP ✅ |
| **Binding** | `100.100.30.59:9000` (S3) + `:8080` (WebDAV) — NOT public ✅ |
| **Upload works** | S3 PUT + WebDAV PUT both HTTP 200/201 ✅ |
| **Download works** | S3 GET + WebDAV GET both return content ✅ |
| **List works** | S3 ListObjects + WebDAV PROPFIND on bucket ✅ |
| **Delete works** | S3 DELETE + WebDAV DELETE both 204 ✅ |
| **TLS** | Tailscale cert exists, not yet wired into server code |
| **NFS** | Disabled in config, port not exposed |
| **Tests** | ❌ No test files written yet |
| **Gap analysis** | ❌ Not yet performed |

### Verified Working Endpoints

**S3 API** — `http://100.100.30.59:9000/`
- ListBuckets, CreateBucket, DeleteBucket
- PutObject, GetObject, HeadObject, DeleteObject
- ListObjectsV2 (with prefix, max-keys)

**WebDAV** — `http://100.100.30.59:8080/`
- OPTIONS, PROPFIND (per-bucket), MKCOL
- GET, PUT, DELETE
- ⚠️ PROPFIND on root `/` returns empty (minor bug)
- ⚠️ COPY/MOVE are stubs (no Destination header parsing)

**Backends**: 3 × pCloud accounts (nova-video-10/11/12, ~4 GB each)

---

## Phase 1 — Harden & Polish (1-2 hours)

### 1a. Stop old services, clean up

| Action | Status |
|--------|--------|
| Kill old `pcloudfs` server | ✅ Done |
| Remove old binary | ✅ Done |
| Close NFS port 2049 | ✅ Done |
| Free ports 9000, 8080 | ✅ Done |

*All complete.*

### 1b. Deploy multifs with Tailscale isolation

| Action | Status |
|--------|--------|
| Update engine to use `Vec<Box<dyn StorageBackend>>` | ✅ Done |
| Install multifs binary | ✅ Done |
| Bind to Tailscale IP `100.100.30.59` only | ✅ Done |
| Verify no `0.0.0.0` binding | ✅ Done |
| Fix HTTP 500 on upload (ensure_path 2004 tolerance) | ✅ Done |
| End-to-end S3 test pass | ✅ Done |
| End-to-end WebDAV test pass | ✅ Done |

*All complete.*

### 1c. TLS via Tailscale certificate

| Action | Estimate |
|--------|----------|
| Wire `rustls` + `tokio-rustls` into server module | 30 min |
| Configure HTTPS on S3 port 9443 + WebDAV 8443 | 15 min |
| Verify TLS via Tailscale domain | 5 min |

### 1d. Push to GitHub

| Action | Estimate |
|--------|----------|
| Final README update with new name | 10 min |
| Architecture doc (`docs/architecture.md`) | 20 min |
| Git commit + push to `main` | 5 min |

---

## Phase 2 — Tests (2-3 hours)

### 2a. Unit Tests

| File | Scope | Estimate |
|------|-------|----------|
| `tests/storage_tests.rs` | StorageBackend trait (mock), MetadataDb, shard logic | 45 min |
| `tests/backends_test.rs` | PCloudBackend wrapper (mocked HTTP) | 30 min |

### 2b. Integration Tests

| File | Scope | Estimate |
|------|-------|----------|
| `tests/s3_tests.rs` | Create bucket, upload, download, list, delete, head | 30 min |
| `tests/webdav_tests.rs` | PROPFIND, GET, PUT, DELETE, MKCOL, COPY, MOVE | 30 min |

### 2c. Manual Smoke Tests

| Test | Scope | Estimate |
|------|-------|----------|
| Upload file → verify on pCloud | 5 min |
| Download → verify content matches | 5 min |
| Delete → verify gone | 5 min |
| Create bucket, list buckets | 5 min |
| WebDAV PROPFIND root + per-bucket | 5 min |
| WebDAV upload → S3 download (cross-protocol) | 5 min |

---

## Phase 3 — MinIO Gap Analysis (1-2 hours)

### 3a. Compatibility Testing

| Client | Test | Estimate |
|--------|------|----------|
| `aws s3 cp` (AWS CLI) | upload, download, list, delete | 15 min |
| `mc` (MinIO Client) | `mb`, `cp`, `ls`, `cat`, `rm` | 15 min |
| `rclone` → S3 | sync, copy, ls | 15 min |
| `rclone` → WebDAV | sync, copy, ls | 15 min |
| macOS Finder → WebDAV | mount, browse, drag-drop | 10 min |
| Python `boto3` | list_buckets, put/get/delete object | 10 min |

### 3b. Gap Table (output)

Each gap gets: name, impact, frequency, effort estimate, priority.

Example:

| Gap | Impact | Effort | Priority |
|-----|--------|--------|----------|
| SigV4 auth | All S3 clients fail | 2 days | 🔴 Critical |
| Multipart upload | Files >5GB fail | 1 day | 🟡 Medium |
| Range requests | No video streaming | 4 hours | 🟡 Medium |
| Content-Type detection | Wrong mime for uploads | 1 hour | 🟢 Easy |
| MD5 ETags | Some clients warn | 30 min | 🟢 Easy |
| Bucket policy/ACL | No multi-user | 3 days | 🟢 Future |
| PROPFIND on root | WebDAV root listing broken | 30 min | 🟢 Easy |

### 3c. Roadmap Decision

Based on gap table, produce a ranked enhancement plan for the next 1-2 weeks.

---

## Execution Order

```
Day 1 (now):
  ├── Phase 1c: TLS via Tailscale cert
  ├── Phase 1d: Push to GitHub with docs
  └── Phase 2a: Unit tests (StorageBackend, MetadataDb)

Day 2:
  ├── Phase 2b: Integration tests (S3 + WebDAV)
  ├── Phase 2c: Manual smoke tests with video-20/21/22
  └── Phase 3a: Client compatibility tests

Day 3:
  ├── Phase 3b: Gap table
  └── Phase 3c: Roadmap decision → next development sprint
```
