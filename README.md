# MultiFS — Multi-Cloud Storage Pool

A MinIO-compatible object storage service written in Rust that aggregates multiple
pCloud accounts into a single S3 + WebDAV endpoint. Files are automatically split into
32 MB chunks, distributed across 8 accounts (~39 GB total), with parallel download,
page-level streaming, and a RAM-backed page cache.

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the full system architecture,
including detailed flow diagrams, step-by-step operation breakdowns, pCloud API
assumptions with doc references, data model, and known issues.

## Quick Start

```bash
# Initialize config and database
multifs init

# Check connectivity to all pCloud accounts
multifs check

# Start the daemon
multifs serve
```

## Features

| Feature | Status |
|---------|--------|
| **Multi-account pCloud storage** — 8 accounts, 39 GB total | ✅ |
| **S3-compatible API** — ListBuckets, Put/Get/Head/Delete, ListObjectsV2 | ✅ |
| **WebDAV** — GET, PUT, DELETE, PROPFIND, MKCOL, OPTIONS | ✅ |
| **Chunked storage (>32 MB)** — Split across accounts with SHA-256 checksums | ✅ |
| **Parallel chunk download** — All chunks downloaded concurrently | ✅ |
| **HTTP Range streaming** — Page-level forwarding, <500ms TTFB | ✅ |
| **Page cache** — 16 KB pages, LRU eviction, up to 10 chunks in `/var/cache/multifs` | ✅ |
| **Download deduplication** — Concurrent requests share chunk downloads | ✅ |
| **Utilization-based placement** — Uploads go to least-full account | ✅ |
| **CLI management** — Accounts, buckets, objects, shard status | ✅ |
| S3 multipart upload | ❌ Stub (blocks rclone >64MB) |
| Erasure coding (5+2) | ❌ Stub |
| WebDAV COPY / MOVE / LOCK / UNLOCK | ❌ Stub |
| NFS | ❌ Stub |

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

## WebDAV Usage

```bash
# Mount with davfs2
sudo mount -t davfs http://localhost:8080 /mnt/multifs

# Or access directly
curl -X PROPFIND http://localhost:8080/
curl -X PUT --data-binary @file.bin http://localhost:8080/bucket/file.bin
curl http://localhost:8080/bucket/file.bin
```

## Deploy

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

## Configuration

See [`config.example.toml`](config.example.toml) or `/etc/multifs.toml`.

### pCloud OAuth Tokens

Stored in `~/.openclaw/.env`:
- `PCLOUD_TOKEN_VIDEO_01` through `PCLOUD_TOKEN_VIDEO_22` (account tokens)
- `PCLOUD_APP_CLIENT_ID` / `PCLOUD_APP_CLIENT_SECRET` (OAuth app credentials)

To add a new pCloud account:
1. Get OAuth token via `multifs account add <email>`
2. Add to `[[storage.accounts]]` in config
3. Restart: `sudo systemctl restart multifs.service`

## CLI Reference

```
multifs serve                 Start daemon
multifs init                  Initialize config + database
multifs check                 Validate config and accounts
multifs status                Daemon health + account stats

multifs account list          List accounts
multifs account add <email>   OAuth flow for new account
multifs account check <email> Test token + show quota

multifs bucket create <name>  Create bucket
multifs bucket list           List buckets
multifs bucket info <name>    Bucket stats

multifs object cp src dst     Copy object (local ↔ remote)
multifs object ls <bucket>    List objects
multifs object rm <bucket>/<key>  Delete object
multifs object info <bucket>/<key>  Object metadata

multifs shard status          Account fill levels
multifs audit                 Find orphaned pCloud files
```

## Running Tests

```bash
cargo test                    # All unit tests
cargo test -- --nocapture     # With output
```

## License

MIT
