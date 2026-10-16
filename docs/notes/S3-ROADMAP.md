# MultiFS — S3 Operations Roadmap

> ## Status (2026-08-22)
> **Shipped & deployed (live-verified):**
> - ✅ CopyObject (metadata-level copy, zero data movement + reference-aware vacuum)
> - ✅ UploadPartCopy (`x-amz-copy-source-range`, streamed MD5 part ETag)
> - ✅ ListMultipartUploads (`GET /bucket?uploads`)
> - ✅ ListObjectVersions (`GET /bucket?versions`) + `versionId` on GET/HEAD
> - ✅ PutBucketVersioning (`PUT /bucket?versioning`, persisted `buckets.versioning`)
>
> **Skipped — not meaningful for a private single-tenant pool (per William):**
> - ⏭️ Object/bucket tagging, ACLs, bucket policy, bucket CORS API (Phase 4)

Remaining S3 API surface to implement, ordered by practical value for the
video-subtitle / rclone use-case. Each item is independent and can be shipped
and deployed on its own.

> Status of each item: `[ ]` pending — tick it when merged + deployed.

---

## Phase 1 — Large-object copy (highest value)

### 1. UploadPartCopy
**Why:** `CopyObject` (just shipped) handles objects of *any* size at the
metadata level, but real S3 caps CopyObject at 5 GB and clients (rclone, `aws s3`)
switch to `CreateMultipartUpload → UploadPartCopy → CompleteMultipartUpload` for
larger objects. Without UploadPartCopy, those clients fall back to
download+re-upload for >5 GB files.

**Scope:**
- S3 handler: on `PUT /{bucket}/{key}?partNumber=N&uploadId=...` with
  `x-amz-copy-source` (and optional `x-amz-copy-source-range`), copy the
  requested byte range of the source into a new multipart part.
- Engine: a `copy_part_range(src_bucket, src_key, range, dst_bucket, upload_id, part_no)`
  that streams `get_object_stream` of the source (range-sliced) into
  `upload_part`-style storage for the target part.
- ETag for a copied part = MD5 of the copied range bytes (S3 part ETag semantics).
- Tests: copy a >5 GB object via rclone/`aws s3 cp` and assert integrity.

**Notes:** `x-amz-copy-source-range: bytes=start-end` is inclusive (S3) and
required when copying a sub-range of a multipart source.

---

## Phase 2 — Multipart visibility / hygiene

### 2. ListMultipartUploads (`GET /bucket?uploads`)
**Why:** Currently the `?uploads` query on `GET /bucket` returns a bogus
`InitiateMultipartUploadResult` with `Key=unknown`. Correct behaviour is a
`ListMultipartUploadsResult` listing in-progress uploads. rclone uses this to
list/resume stalled uploads.

**Scope:**
- Engine/metadata: `list_multipart_uploads(bucket, prefix, key_marker, upload_id_marker, max)` over `multipart_uploads` + `multipart_parts`.
- S3 handler: proper `ListMultipartUploadsResult` XML (Uploads + CommonPrefixes).
- Remove the placeholder `?uploads` branch in `list_objects`.
- Tests: initiate 2 uploads, list, assert keys/ids; respect pagination markers.

---

## Phase 3 — Expose the existing MVCC as S3 versioning

### 3. ListObjectVersions (`GET /bucket?versions`) + `versionId`
**Why:** MultiFS already keeps full MVCC history in the `versions` table. This
just exposes it over the S3 versioning API.

**Scope:**
- Metadata: `list_object_versions(bucket, prefix, ...)` returning versions + delete
  markers; `get_object_version(bucket, key, version_id)`.
- S3 handler: `ListVersionsResult` XML; accept `versionId` on GET/HEAD/DELETE.
- Tests: put twice, list versions (both), fetch an old version by id.

### 4. PutBucketVersioning (`PUT /bucket?versioning`)
**Why:** Let clients turn versioning on/off. The store already version-internally,
so this is mostly signalling + gating delete-marker behaviour.

**Scope:**
- Persist a per-bucket versioning flag (new column on `buckets` or a `bucket_meta` table).
- S3 handler: parse `<VersioningConfiguration>`; return it on GET.
- Decide: keep `Suspended` default for now (delete still soft-deletes via MVCC).

---

## Phase 4 — Metadata & access-control (lower value, single-tenant)

### 5. Object tagging — Get/Put/DeleteObjectTagging
- New `object_tags(bucket, key, tag_key, tag_value)` table; parse `<Tagging>` XML;
  return `<Tagging>` / empty set. Mirror tags onto copy (S3 default: copy carries tags).

### 6. Bucket tagging — Get/PutBucketTagging
- `bucket_tags` table; same XML shape at bucket level.

### 7. ACLs — Get/PutObjectACL, Get/PutBucketACL
- Likely stub returning `private` owner ACL (single-tenant). No real enforcement.

### 8. Bucket policy — Get/Put/DeleteBucketPolicy
- Store JSON policy in a `bucket_meta` column; GET/PUT/DELETE with `?policy`.
  No enforcement unless a use-case demands it.

### 9. Bucket CORS API — Get/PutBucketCors
- Persist CORS rules; return on GET. (HTTP `CorsLayer` is already permissive at the
  transport layer; this is just the S3 config API surface.)

---

## Explicitly out of scope (no plan)
- **RestoreObject** (Glacier) — no archival tier.
- **GetObjectAttributes** — newer API; add only if a client needs it.
- **Lifecycle rules, replication, object lock, encryption keys** — not applicable
  to a private storage pool.

---

## Process notes
- One item at a time (matches William's one-at-a-time preference).
- Build: `cargo build --release` + `cargo test`.
- Deploy: `sudo cp target/release/multifs /usr/local/bin/multifs.new && sudo mv /usr/local/bin/multifs.new /usr/local/bin/multifs && sudo systemctl restart multifs`
  (the temp+`mv` avoids `Text file busy` on a running binary).
- Smoke-test each item against the live endpoint before closing it out.
