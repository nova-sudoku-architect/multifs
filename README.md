# pCloudFS — Multi-Cloud Object Storage

An object storage service (like MinIO) written in Rust. Objects are stored on **pCloud** accounts accessed via OAuth tokens, distributed across multiple accounts for capacity. The service exposes **S3-compatible**, **WebDAV**, and **NFS** interfaces.

## Quick Start

```bash
# Initialize config and database
pcloudfs init

# Add your pCloud accounts
pcloudfs account add nova-video@agentmail.to

# Check connectivity
pcloudfs check

# Start the daemon
pcloudfs serve
```

## Features

- **Multi-account pCloud storage** — distribute objects across multiple pCloud accounts
- **S3-compatible API** — works with `aws-cli`, `boto3`, `MinIO SDKs`, `rclone`
- **WebDAV** — mount via `davfs2`, access from macOS Finder
- **NFS v3** — mount as a regular filesystem (in progress)
- **Automatic sharding** — objects are distributed based on account fill levels
- **Local caching** — LRU disk cache for frequently accessed objects
- **CLI management** — manage accounts, buckets, objects, and shards

## CLI Usage

```bash
pcloudfs serve                 # Start the daemon
pcloudfs init                  # Initialize config + database
pcloudfs check                 # Validate config and accounts

pcloudfs config show           # Print current config

pcloudfs account list          # List configured accounts
pcloudfs account add <email>   # Add account (OAuth flow)
pcloudfs account check <email> # Test token and show quota

pcloudfs bucket create <name>  # Create a bucket
pcloudfs bucket list           # List all buckets
pcloudfs bucket info <name>    # Show bucket stats

pcloudfs object cp <local> <bucket>/<key>  # Upload
pcloudfs object cp <bucket>/<key> <local>  # Download
pcloudfs object list <bucket>              # List objects
pcloudfs object rm <bucket>/<key>          # Delete object
pcloudfs object info <bucket>/<key>        # Show metadata

pcloudfs shard status          # Show account fill levels
pcloudfs status                # Daemon health and stats
```

## S3 Usage (with aws-cli)

```bash
# Configure AWS CLI to use pcloudfs
aws configure set endpoint_url http://localhost:9000
aws configure set aws_access_key_id pcloudfs
aws configure set aws_secret_access_key pcloudfs

# Use it like any S3 service
aws s3 mb s3://my-bucket
aws s3 cp file.txt s3://my-bucket/
aws s3 ls s3://my-bucket/
aws s3 cp s3://my-bucket/file.txt ./downloaded.txt
```

## Architecture

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│  NFS Client  │  │   S3 SDK    │  │  WebDAV     │
│  (mount -t   │  │   (boto3/   │  │  (cadaver/  │
│   nfs)       │  │   minio-py) │  │   davfs2)   │
└──────┬───────┘  └──────┬──────┘  └──────┬──────┘
       │                 │                 │
       ▼                 ▼                 ▼
┌─────────────────────────────────────────────┐
│           pCloudFS Daemon (rust)            │
│  ┌────────┐ ┌────────┐ ┌──────────────────┐│
│  │  NFS   │ │  S3    │ │  WebDAV          ││
│  │Server  │ │Gateway │ │  Server           ││
│  └───┬────┘ └───┬────┘ └───────┬──────────┘│
│      │          │               │           │
│      └──────────┴───────────────┘           │
│                      │                      │
│             ┌────────▼────────┐             │
│             │  Storage Engine │             │
│             │  + SQLite Meta  │             │
│             └────────┬────────┘             │
└──────────────────────┼──────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────┐
│        pCloud Accounts (OAuth 2.0)          │
│  ┌────────┐ ┌────────┐ ┌────────┐ ┌──────┐│
│  │Acct #0 │ │Acct #1 │ │Acct #2 │ │...   ││
│  │ 10 GB  │ │ 10 GB  │ │ 6 GB   │ │      ││
│  └───┬────┘ └───┬────┘ └───┬────┘ └──┬───┘│
└──────┼──────────┼──────────┼──────────┼────┘
       │          │          │          │
       ▼          ▼          ▼          ▼
   eapi.pcloud.com  (EU region)
```

## Configuration

See [`config.example.toml`](config.example.toml) for all options.

Key environment variables for pCloud tokens:
- `PCLOUD_APP_CLIENT_ID` / `PCLOUD_APP_CLIENT_SECRET` — OAuth app credentials
- `PCLOUD_TOKEN` — Token for `nova-video@agentmail.to`
- `PCLOUD_TOKEN_VIDEO_01` — Token for `nova-video-01@agentmail.to`
- (add more as needed)

## Project Status

- ✅ Project scaffolded and compiles
- ✅ CLI with all management commands
- ✅ Storage engine with pCloud backend
- ✅ SQLite metadata database
- ✅ S3-compatible API (basic operations)
- ✅ WebDAV server
- 🔄 NFS v3 server (stub — in progress)
- 🔄 Local disk cache
- 🔄 Tests and CI

## License

MIT
