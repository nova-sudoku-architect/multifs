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
access_keys = ""          # S3 SigV4 keys "AKID:SECRET,…"; empty = UNAUTHENTICATED

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

1. Get an OAuth token (via the pCloud OAuth flow) and export it, then run
   `multifs account add <email> --token-env PCLOUD_TOKEN_X` (or
   `--token-stdin` to pipe it). The token **value** is written to the env file
   (`/etc/multifs.env`, mode `0600`); only its `token_env` **name** goes in the
   config. Tokens are never passed as plain arguments (no shell-history leak).
2. That command also appends the `[[storage.accounts]]` block to the config.
3. Restart: `sudo systemctl restart multifs.service`.

To rotate a token later: `multifs account update-token <email> --token-env VAR`
(validates against pCloud unless `--no-verify`). Non-secret fields:
`multifs account set <email> --quota-gb N --priority P --mount-prefix /p`.

### S3 authentication (SigV4)

**The S3 endpoint is unauthenticated by default.** To require AWS Signature Version 4
on every request, set `server.access_keys` to a comma-separated `AKID:SECRET` list:

```toml
[server]
access_keys = "AKIAEXAMPLE:secret1,AKIAEXAMPLE2:secret2"
```

Prefer supplying it from the environment (`MULTIFS_ACCESS_KEYS`) so secrets never sit in
the config file; config still only holds values, not a path, so env-substitution or an
`EnvironmentFile=` in the systemd unit is the way.

- With keys configured, every request must carry a valid
  `Authorization: AWS4-HMAC-SHA256 …` header signed with one of the keys. Bad/absent
  signatures get `403 AccessDenied` / `SignatureDoesNotMatch`; `x-amz-date` outside a
  15-minute window gets `403 RequestTimeTooSkewed`.
- With **no** keys configured the endpoint stays open (local/dev); the daemon logs a
  loud `S3 endpoint is UNAUTHENTICATED` warning at startup. **Never expose port `9000` to
  an untrusted network without `access_keys`.**

Client setup (any S3 tool) then uses the configured pair:

```bash
aws configure set endpoint_url http://localhost:9000
aws configure set aws_access_key_id AKIAEXAMPLE
aws configure set aws_secret_access_key secret1
```

> Only *header*-based SigV4 is supported (the normal path for aws-cli/rclone).
> Presigned query-string URLs (`X-Amz-Signature=…`) and STS temporary credentials
> are **not** yet supported.


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

### From a package (recommended)

Build and install the Debian package (see [`packaging/README.md`](../packaging/README.md)):

```bash
packaging/build-deb.sh                 # -> target/deb/multifs_<version>_amd64.deb
sudo apt install ./target/deb/multifs_0.1.0_amd64.deb
sudo systemctl enable --now multifs.service
```

The package places the binary, the systemd units (`multifs.service`, the vacuum timer,
and the optional mount unit), a generic `/etc/multifs.toml`, and an empty `/etc/multifs.env`
(both conffiles, so upgrades preserve your edits), and creates the state/cache dirs.
`apt remove` keeps config + state; `apt purge` removes them.

### From source (manual)

```bash
cargo build --release
sudo systemctl stop multifs.service
sudo cp target/release/multifs /usr/local/bin/multifs
sudo systemctl start multifs.service
```

Deployed as systemd `multifs.service`: binary `/usr/local/bin/multifs`, config
`/etc/multifs.toml` (a `/etc/multifs/config.toml` symlink also works), env
`/etc/multifs.env`, DB `/var/lib/multifs/meta.db`. (Host-specific values live in the
instance config, not this repo.)


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
