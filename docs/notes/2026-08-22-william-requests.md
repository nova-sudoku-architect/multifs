# William's Requests — 2026-08-22

A dated checklist of what William asked me to do this session, so I can check
status later. Items are marked ✅ done / ⏳ pending.

---

## 1. Implement remaining S3 operations (MultiFS)

| # | Operation | Status | Notes |
|---|---|---|---|
| 1 | CopyObject | ✅ done + live-verified | metadata-level copy (zero data movement) + reference-aware vacuum |
| 2 | UploadPartCopy | ✅ done + live-verified | streamed multipart copy, MD5 part ETag, `x-amz-copy-source-range` |
| 3 | ListMultipartUploads | ✅ done | proper `ListMultipartUploadsResult` (replaced placeholder) |
| 4 | ListObjectVersions + `versionId` | ✅ done + live-verified | `GET /bucket?versions`, `versionId` on GET/HEAD |
| 5 | PutBucketVersioning | ✅ done + live-verified | persisted `buckets.versioning` column (migration 8) |
| 6 | Object + bucket tagging | ⏭️ skipped (William) | cosmetic for private single-tenant pool |
| 7 | ACLs (object + bucket) | ⏭️ skipped (William) | cosmetic |
| 8 | Bucket policy + CORS API | ⏭️ skipped (William) | cosmetic |

Smoke-tested each shipped op against the live endpoint. Versioning flow tested
end-to-end (PutBucketVersioning Enabled → GetBucketVersioning → ListObjectVersions
→ GET ?versionId=1 byte-identical), then reset to Suspended (default).

Roadmap updated at `docs/S3-ROADMAP.md`.

---

## 2. Folder preview page (web UI) fixes

| # | Request | Status |
|---|---|---|
| 1 | Summary section expands fully (no nested scroll on iPhone) | ✅ done + deployed |
| 2 | Folder preview page only activated in **Preview mode** | ✅ done + deployed |

Change #2: in `index.html` `loadList()`, `renderFolderDetail()` is now gated on
`state.mode === 'preview'`; `loadSummaryText()` also skipped outside preview mode.

---

*Generated 2026-08-22 ~09:45 GMT+8. Revisit this file to confirm current state.*
