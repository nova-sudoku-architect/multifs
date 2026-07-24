# WebDAV Interface Documentation

**Base URL**: `https://vmi3137694.tailb9bfd3.ts.net:8080/`
**DAV Compliance**: Class 1 + Class 2 (partial)

---

## Supported Operations

### ✅ Fully Supported

| Operation | HTTP Method | Description |
|-----------|-------------|-------------|
| **List buckets** | `PROPFIND /` with `Depth: 1` | Returns all buckets as DAV:collection entries |
| **List objects** | `PROPFIND /bucket` with `Depth: 1` | Returns files with ETag, size, last-modified |
| **Download** | `GET /bucket/key` | Downloads file content with auto-detected Content-Type |
| **Upload** | `PUT /bucket/key` | Uploads file; auto-creates bucket if missing (returns 201) |
| **Create bucket** | `MKCOL /bucket` | Creates a top-level bucket (returns 201) |
| **Delete object** | `DELETE /bucket/key` | Deletes object; idempotent (always returns 204) |
| **Delete bucket** | `DELETE /bucket` | Deletes entire bucket and all objects (returns 204) |
| **Head** | `HEAD /bucket/key` | Returns metadata headers only |
| **Options** | `OPTIONS /` | Returns DAV: 1, 2 and allowed methods |

### ⚠️ Partially Supported (Caveats)

| Operation | Method | Status | Limitation |
|-----------|--------|--------|------------|
| **COPY** | `COPY /source` | ⚠️ Partial | `Destination` header is **ignored**. Always appends `_copy` to the source filename in the same bucket. Cannot copy across buckets. |
| **MOVE** | `MOVE /source` | ⚠️ Partial | Same limitation as COPY — appends `_moved` suffix. Acts as rename-in-place within the same bucket. |

### ❌ Not Supported

- `LOCK` / `UNLOCK` (WebDAV class 2 locking)
- `PROPPATCH` (property modification)
- Partial/delta uploads (`PATCH`)
- Range requests (`Range` header on GET)
- Directory nesting (only flat `bucket/key` structure)
- `quota-available-bytes` / `quota-used-bytes` properties
- Basic/Digest authentication

---

## Client Setup

### macOS Finder
```
Go → Connect to Server → https://vmi3137694.tailb9bfd3.ts.net:8080/
```
Finder presents the buckets as folders and objects as files. Upload via drag-and-drop.

### Windows
```
Add a network location → https://vmi3137694.tailb9bfd3.ts.net:8080/
```

### Linux (davfs2)
```bash
sudo apt install davfs2
sudo mount -t davfs https://vmi3137694.tailb9bfd3.ts.net:8080/ /mnt/multifs
```

### rclone
```bash
rclone config
# Choose "webdav" type
# URL: https://vmi3137694.tailb9bfd3.ts.net:8080/
# Vendor: Other
```

### curl (for scripting)
```bash
# List buckets
curl -X PROPFIND "https://vmi3137694.tailb9bfd3.ts.net:8080/" -H "Depth: 1"

# List bucket contents
curl -X PROPFIND "https://vmi3137694.tailb9bfd3.ts.net:8080/my-bucket" -H "Depth: 1"

# Upload
echo "content" | curl -X PUT --data-binary @- "https://vmi3137694.tailb9bfd3.ts.net:8080/bucket/file.txt"

# Download
curl "https://vmi3137694.tailb9bfd3.ts.net:8080/bucket/file.txt"

# Create bucket
curl -X MKCOL "https://vmi3137694.tailb9bfd3.ts.net:8080/new-bucket"

# Delete
curl -X DELETE "https://vmi3137694.tailb9bfd3.ts.net:8080/bucket/file.txt"
```

---

## Limitations & Known Issues

### Performance

| Operation | Speed | Notes |
|-----------|-------|-------|
| LIST (PROPFIND) | 🟢 Fast | Metadata only — served from local SQLite, no pCloud API call |
| DELETE | 🟢 Fast | Single pCloud API call |
| UPLOAD (PUT) | 🟡 Moderate | ~100-300ms per file + pCloud EU network latency |
| DOWNLOAD (GET) | 🟡 Moderate | Two pCloud API calls: `getfilelink` redirect + fetch from CDN |
| COPY | 🔴 Slow | Downloads full object from pCloud to RAM, then re-uploads |
| MOVE | 🔴 Slow | Same as COPY — download + upload + delete |

### Memory

- **All operations load the full file into RAM.** There is no streaming to/from pCloud.
- A 2 GB upload consumes ~2 GB of RAM on the server.
- Practical file size limit: ~500 MB before memory becomes a concern on this server.

### COPY / MOVE are expensive

Both COPY and MOVE:
1. Download the entire object from pCloud into server RAM
2. Upload the entire object back to pCloud
3. MOVE additionally deletes the original

For a 100 MB file: 100 MB download + 100 MB upload = ~200 MB of data transfer per operation.

### No Authentication

Currently the WebDAV endpoint has no authentication configured. Anyone on your Tailscale network can read/write to any bucket. **Do not expose to the public internet.** This server is bound to the Tailscale interface only (`100.100.30.59`).

### No Directory Nesting

S3 has a flat bucket→key structure. WebDAV maps this as:
```
/                       → List all buckets
/my-bucket              → List files in bucket
/my-bucket/file.txt     → Download file
```
You cannot create `/my-bucket/subdir/file.txt` — only one level of nesting (bucket/key) is supported.

### Property Support

Only basic DAV properties are returned:
- `displayname`
- `getcontenttype`
- `getcontentlength`
- `getlastmodified`
- `resourcetype`

Custom properties, quota information, and locking information are not returned.

---

## Under the Hood

- Backend: pCloud (EU region, `eapi.pcloud.com`)
- Metadata: SQLite (local)
- Objects are distributed across 3 pCloud accounts using fill-level sharding
- All operations are synchronous HTTP calls to pCloud (blocking the request until complete)
- TLS certificate is managed by Tailscale (auto-renewed)
