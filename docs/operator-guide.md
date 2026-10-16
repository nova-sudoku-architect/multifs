# MultiFS — Operator Guide

For running the daemon: configuration, backends, deployment, observability, and garbage collection.

## Configuration

Config lives at `/etc/multifs.toml` (or `/etc/multifs/config.toml`). See
[`config.example.toml`](config.example.toml) for an annotated, **generic**
template. Deployment-specific configs (real accounts, bind IPs, quotas) are kept
on the host only and are deliberately not committed to this repo.

```toml
[server]
bind = "0.0.0.0"
s3_port = 9000
enable_s3 = true
enable_nfs = false        # NFS support removed; field kept for config compatibility
enable_web = false        # read-only web UI (GET-only browser)
web_port = 9001
enable_metrics = false    # Prometheus /metrics + /healthz + /readyz
metrics_port = 9100

[storage]
meta_db_path = "/var/lib/multifs/meta.db"
placement_strategy = "Utilization"   # or "RoundRobin"
# auto_coalesce_multipart = false   # periodically rewrite multipart objects as single blobs

# pCloud account (cloud backends default to priority 0 = preferred)
[[storage.accounts]]
email = "nova-video-01@agentmail.to"
backend_type = "pcloud"
token_env = "PCLOUD_TOKEN_VIDEO_01"   # env var holding the OAuth token
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

The config value is `"Utilization"` or `"RoundRobin"` (PascalCase, e.g.
`placement_strategy = "Utilization"`); the CLI/help form is lowercase
(`utilization` / `round-robin`).

- **`Utilization` (default, tiered)** — fill the lowest `priority` tier first,
  least-full backend within the tier wins; spill to the next tier only when the
  preferred tier is full. Local disk (`priority = 1`) only receives writes when
  every cloud account is out of free space.
- **`RoundRobin`** — cycles across all backends, ignoring priority.

### pCloud OAuth tokens

Tokens are read from environment variables at startup and referenced by
`token_env` name only — never stored in code or config. For the deployed host
they live in `/etc/multifs.env` (sourced by systemd) / `~/.openclaw/.env`.

- `PCLOUD_TOKEN_*` — one per account
- `PCLOUD_APP_CLIENT_ID` / `PCLOUD_APP_CLIENT_SECRET` — OAuth app credentials

> ⚠️ All pCloud accounts are registered in the **EU** region — every request must
> use `https://eapi.pcloud.com` (not `api.pcloud.com`).

To add a pCloud account:

1. Get an OAuth token: `multifs account add <email>` (or the manual OAuth flow).
2. Add an `[[storage.accounts]]` block to the config.
3. Restart: `sudo systemctl restart multifs.service`.


## Backends

| Type | Notes |
|------|-------|
| pCloud (EU) | One backend per pCloud account — the reference deployment pools ~77 accounts (~325 GB combined). OAuth tokens via `token_env`. |
| Local disk | Single overflow backend (`priority = 1`). |

A backend is any type implementing the `StorageBackend` trait. Three ship in
tree: `PCloudBackend` (real), `LocalDiskBackend` (overflow tier), and
`MockBackend` / `TrackedBackend` (unit tests). See
[`architecture.md`](architecture.md#backend-interface-storagebackend-trait).


## Deploy

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

Deployed as systemd `multifs.service`: binary `/usr/local/bin/multifs`, config
`/etc/multifs.toml`, env `/etc/multifs.env`, DB `/var/lib/multifs/meta.db`.
(Host-specific values live in the instance config, not this repo.)


## Observability

Log-only by default; optionally expose metrics and health endpoints for a
scrape/alerting stack (Prometheus, Uptime Kuma, …). Enable with
`server.enable_metrics = true` (port `server.metrics_port`, default `9100`):

| Route | Purpose |
|-------|---------|
| `/metrics` | Prometheus text exposition (see series below) |
| `/healthz` | Liveness — `200` while the process is up |
| `/readyz` | Readiness — `200` when the metadata DB is reachable and ≥1 backend is configured, else `503` |

Exposed series (in-process counters + per-backend gauges from the metadata DB — a
scrape makes **no** backend API calls): `multifs_requests_total{status_class}`,
`multifs_errors_total`, `multifs_bytes_in_total` / `multifs_bytes_out_total`,
`multifs_gc_runs_total` / `multifs_gc_reclaimed_items_total`, `multifs_backends`,
and per-backend `multifs_backend_objects` / `_parts` / `_used_bytes` /
`_quota_bytes` / `_fill_ratio{backend=…}`.

```bash
curl -s http://localhost:9100/metrics
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:9100/readyz
```


## Garbage collection

`multifs vacuum [--dry-run]` reclaims blobs no live file references (the server
also runs it automatically every 10 minutes):

1. **Abandoned uploads** — `pending` versions older than 1 hour (reserved but never committed).
2. **Superseded versions** — committed versions whose `superseded_at` is older than 10 minutes.
3. **Abandoned multipart uploads** — in-progress uploads initiated more than 24 hours ago:
   their staged part blobs, `multipart_uploads` and `multipart_parts` rows are all reclaimed.

Completed multipart objects keep their part rows for read assembly and are left
untouched. In-progress uploads can also be cancelled explicitly via S3
AbortMultipartUpload (`DELETE /bucket/key?uploadId=...`).

Multipart parts are reclaimed **per-account**: because each `UploadPart` picks a
backend independently, one upload's parts can live on 2–4 backends, and GC
deletes each part from the backend that actually holds it. A failed delete keeps
the metadata rows so a later vacuum retries instead of leaking bytes.


## Operational caveats

See [planning → design limitations](planning.md#caveats--limitations) — notably: no
redundancy across accounts, no upload retry/backoff, static unenforced credentials, and
single-node/single-process state. Back up `/var/lib/multifs/meta.db`.
