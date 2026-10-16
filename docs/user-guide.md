# MultiFS — User Guide

For anyone **using** the S3 endpoint or the web UI: terminology, S3 clients, and folder features.

## Concepts & terminology

| Term | Meaning |
|------|---------|
| **Bucket** | S3 namespace for objects. Registry table `buckets`. |
| **Key** | Object path inside a bucket (may contain `/`, e.g. `video/blor-116/cover.jpg`). |
| **Object** | A `(bucket, key)` pair plus its metadata; the thing S3 clients see. |
| **Blob** | The **physical** stored file on a backend. In the single-blob model, one object version = exactly one blob. |
| **Version** | An immutable snapshot of an object's content. MultiFS keeps history (MVCC). |
| **MVCC** | Multi-Version Concurrency Control — writers create new versions instead of overwriting; readers resolve the live version once at request start. |
| **Copy-on-write** | An overwrite writes a *new* blob and atomically repoints the live pointer; the old blob is left intact for readers/rollback until GC. |
| **`files` table** | Logical pointer: **one row per `(bucket, key)`** holding `current_version` and the mirrored size/etag/checksum. |
| **`versions` table** | One row per version — `account_email` + `remote_path` say *where the blob lives*. |
| **`pending`** | A reserved-but-not-yet-committed version (upload in flight). |
| **`committed`** | A version that completed; the live one is pointed at by `files.current_version`. |
| **Superseded / `superseded_at`** | A previously-live version after a newer one committed. Kept for a **grace period**, then GC-eligible. |
| **Backend** | A physical storage destination — a pCloud account or the local disk. |
| **Account** | Config entry describing a backend (`[[storage.accounts]]`): email, type, quota, `mount_prefix`, `priority`. |
| **`mount_prefix`** | Per-backend base path (e.g. `/multifs/01`) that blobs are stored under. |
| **Placement** | Choosing which backend receives a new upload. |
| **`priority`** | Lower = more preferred. Cloud backends default `0`; local disk `1`. |
| **Tier** | A group of backends at the same `priority`; tiers are filled in ascending order. |
| **`fill_ratio`** | `used / quota` for a backend; drives "least-full wins" within a tier. |
| **Shard** | Legacy alias for the `backend` subcommand — `multifs backend status` shows per-backend fill levels, `backend rebalance` moves data off over-full backends. The underlying concept is a *backend* (a pCloud account or local disk); `shard` is accepted as a compatibility alias. |
| **ETag** | For a single-blob object, the **SHA-256 hex** of the blob (also stored as the checksum). For multipart objects, the S3 multipart ETag (MD5-of-part-MD5s `-N`). |
| **Checksum** | Stored SHA-256 of the live content; `checksum rebuild|verify` detects in-place drift. |
| **Charset** | Detected text encoding (BOM / UTF-8 validation) stored per version for correct serving. |
| **Multipart upload** | S3 large-file protocol: `Initiate → UploadPart* → Complete`. Parts are staged under a `__mp__/` prefix. |
| **Part** | One uploaded chunk of a multipart upload; each part is its own blob and may land on a different backend. |
| **Vacuum / GC** | Background reclaimer of blob bytes no live object references (see [GC](operator-guide.md#garbage-collection)). |
| **Symlink (tag folder)** | A CLI-created alias: a folder prefix that re-presents another folder's contents under a second path (no bytes duplicated). |
| **Folder metadata** | Per-prefix `cover` / `summary` / `preview_gif` keys, shown on the web UI preview page. |
| **`last_modified` (folder)** | Epoch-ms stamp of the most recent add/remove/update anywhere in a folder's subtree. |
| **Schema version** | SQLite migration level (`meta.db`); currently **9**. |
| **`token_env`** | Name of the env var holding a backend's OAuth token — secrets never live in code/config. |


## S3 usage

```bash
aws configure set endpoint_url http://localhost:9000
aws configure set aws_access_key_id multifs
aws configure set aws_secret_access_key multifs

aws s3 mb s3://my-bucket
aws s3 cp file.txt s3://my-bucket/
aws s3 ls s3://my-bucket/
aws s3 cp s3://my-bucket/file.txt ./downloaded.txt
```

S3 multipart upload is fully implemented, so large-file copies (e.g.
`rclone copy` of files > 64 MB) work end-to-end. Any S3-compatible client works;
credentials are static (`multifs` / `multifs`) and are not enforced by the server.


## Web UI

The optional read-only web UI (port `9001`, `server.enable_web = true`) is a GET-only
browser: browse buckets/folders, sort columns, view the image gallery + per-folder
preview page, and download (Range supported for streaming). Three view modes — **List**,
**Preview** (rows + folder cards), and **Names** — with large-folder virtualization
(`content-visibility`) and lazily-hydrated cover thumbnails. It never writes.

## Folder metadata & symlinks

- **Folder metadata** — a folder can record a `cover` image, a `summary` document, and a
  `preview_gif`; the preview page shows them (missing fields degrade gracefully). Set via
  `multifs folder set-cover|set-summary|set-gif|backfill`.
- **Symlinks (tag folders)** — present one folder under multiple paths without duplicating
  bytes (`multifs link`).
- **Folder last-modified** — every folder shows the most recent add/remove/update in its subtree.

Full command surface: [CLI reference](cli-reference.md).
