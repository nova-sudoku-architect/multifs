# MultiFS — Multi-Cloud Storage Pool

A MinIO/S3-compatible object storage service written in Rust that aggregates multiple
storage backends — **8 pCloud OAuth accounts (~49 GB)** plus a **local disk (80 GB)** — into
a single S3 endpoint (port 9000). Objects are stored as **single self-contained blobs** with
**copy-on-write MVCC versioning**: overwrites create a new version and atomically flip the
live pointer, never mutating a blob in place.

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the full system architecture:
SQLite data model, MVCC write/read/delete paths, S3 multipart upload, garbage collection,
placement strategy, backend interface, and known issues.

## Quick Start

```bash
# Initialize config and database
multifs init

# Check connectivity to all backends
multifs check

# Start the daemon
multifs serve
```

## Features

| Feature | Status |
|---------|--------|
| **S3-compatible API** — ListBuckets, Create/DeleteBucket, Put/Get/Head/Delete Object, ListObjectsV2 | ✅ |
| **S3 multipart upload** — Initiate / UploadPart / Complete / ListParts / Abort (rclone >64 MB) | ✅ |
| **Single-blob MVCC versioning** — copy-on-write overwrite, old version kept for a grace period | ✅ |
| **HTTP Range streaming** — Range forwarded to the pCloud CDN (start/end) | ✅ |
| **Tiered placement** — cloud-first, local disk as last resort (per-account `priority`) | ✅ |
| **`vacuum` garbage collection** — reclaims superseded + abandoned versions + abandoned multipart uploads | ✅ |
| **`import` command** — register an existing pCloud file (metadata only, no data movement) | ✅ |
| **CLI management** — accounts, buckets, objects, shards, audit | ✅ |
| WebDAV | ❌ Removed |
| NFS | ❌ Stub (port not exposed) |
| Erasure coding | ❌ Stub (single-blob model; each blob lives on one account) |

## Backends

| Type | Count | Notes |
|------|-------|-------|
| pCloud (EU) | 8 accounts | OAuth tokens from env vars, ~49 GB combined |
| Local disk | 1 | `/var/lib/multifs/disk`, 80 GB, `priority = 1` (last resort) |

## S3 Usage

```bash
# With aws-cli
aws configure set endpoint_url http://localhost:9000
aws configure set aws_access_key_id multifs
aws configure set aws_secret_access_key multifs

aws s3 mb s3://my-bucket
aws s3 cp file.txt s3://my-bucket/
aws s3 ls s3://my-bucket/
aws s3 cp s3://my-bucket/file.txt ./downloaded.txt
```

S3 multipart upload is fully implemented, so large-file copies (e.g. `rclone copy` > 64 MB)
work end-to-end.

## Configuration

Config lives at `/etc/multifs.toml` (or `/etc/multifs/config.toml` symlink). See
[`config.example.toml`](config.example.toml) for a commented example and
[`config.deploy.toml`](config.deploy.toml) for the current deployed configuration.

Key fields:

```toml
[server]
bind = "0.0.0.0"
s3_port = 9000
enable_s3 = true
enable_nfs = false        # NFS is a stub

[storage]
meta_db_path = "/var/lib/multifs/meta.db"
placement_strategy = "utilization"   # or "round-robin"

# pCloud account (cloud backends default to priority 0 = preferred)
[[storage.accounts]]
email = "nova-video-01@agentmail.to"
backend_type = "pcloud"
token_env = "PCLOUD_TOKEN_VIDEO_01"
mount_prefix = "/multifs/01"
quota_gb = 10

# Local disk backend (priority 1 = last resort; only absorbs overflow)
[[storage.accounts]]
email = "local-disk"
backend_type = "local"
mount_prefix = "/multifs/local"
path = "/var/lib/multifs/disk"
quota_gb = 80
priority = 1
```

### Placement

- **`utilization` (default)** — tiered: fill the lowest `priority` tier first (cloud = 0),
  spilling to the next tier only when the preferred tier is full. Local disk (`priority = 1`)
  only receives writes when every cloud account is out of free space.
- **`round-robin`** — cycles across all backends, ignoring priority.

### pCloud OAuth Tokens

Tokens are read from environment variables at startup. Stored in `~/.openclaw/.env`:

- `PCLOUD_TOKEN_VIDEO_01` … `PCLOUD_TOKEN_VIDEO_22` (account tokens)
- `PCLOUD_APP_CLIENT_ID` / `PCLOUD_APP_CLIENT_SECRET` (OAuth app credentials)

To add a pCloud account:

1. Get an OAuth token via `multifs account add <email>`
2. Add an `[[storage.accounts]]` block to the config
3. Restart: `sudo systemctl restart multifs.service`

## CLI Reference

```
multifs serve [--config <path>]   Start the daemon
multifs init                      Initialize config + database
multifs check                     Validate config, test all accounts
multifs status                    Daemon health + account stats
multifs config show|set           Show / edit configuration

multifs account list              List accounts
multifs account add <email>       OAuth flow for a new account
multifs account remove <email>    Remove an account from rotation
multifs account check <email>     Test token + show quota
multifs account refresh <email>   Refresh OAuth token

multifs bucket list               List buckets
multifs bucket create <name>      Create a bucket
multifs bucket delete <name>      Delete a bucket (--force to skip empty check)
multifs bucket info <name>        Bucket stats

multifs object list <bucket>      List objects
multifs object cp <src> <dst>     Copy object (local ↔ remote)
multifs object rm <bucket>/<key>  Delete object
multifs object info <bucket>/<key>  Object metadata

multifs shard status              Backend fill levels
multifs shard rebalance           Rebalance over-full → under-full backends

multifs audit scan <email>        Find pCloud files NOT managed by MultiFS
multifs audit list-files <email>  List all files (managed + orphaned)

multifs import <email> <path> --bucket <b> [--key <k>]
                                  Register an existing pCloud file (metadata only)

multifs vacuum [--dry-run]        GC superseded + abandoned version blobs
```

## Garbage Collection

`multifs vacuum [--dry-run]` reclaims blobs no live file references (the server also runs
it automatically every 10 minutes):

1. **Abandoned uploads** — `pending` versions older than 1 hour (reserved but never committed).
2. **Superseded versions** — committed versions whose `superseded_at` is older than 10 minutes.
3. **Abandoned multipart uploads** — in-progress uploads initiated more than 24 hours ago:
   their staged part blobs, `multipart_uploads` and `multipart_parts` rows are all reclaimed.

Completed multipart objects keep their part rows for read assembly and are left untouched.
In-progress uploads can also be cancelled explicitly via S3 AbortMultipartUpload
(`DELETE /bucket/key?uploadId=...`).

## Deploy

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

## Running Tests

```bash
cargo test                    # All unit tests
cargo test -- --nocapture     # With output
```

## License

MIT
