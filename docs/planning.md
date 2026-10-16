# MultiFS — Planning & Evaluation

For deciding whether to adopt MultiFS: what it is, its design goals, what it is **not**,
and where it is heading.

MultiFS pools many independent storage backends (currently ~77 pCloud OAuth accounts + a
local disk) behind a single S3-compatible endpoint, using single self-contained blobs and
copy-on-write MVCC versioning. It favours **capacity aggregation and simple, crash-safe
semantics** over redundancy and multi-tenancy (both on the roadmap).

## Caveats & limitations

Honest list of what MultiFS is **not** today — read before relying on it.

1. **No redundancy across accounts.** Each blob lives on exactly one pCloud
   account (single-blob model, no erasure coding / replication). If an account
   is suspended, rate-limited, or loses data, **those blobs are gone**. There is
   no automatic repair.
2. **Aggregating many free accounts is fragile.** The pool is built from ~77
   independently-registered pCloud accounts. Accounts can change quota
   (provisioning lag), get rate-limited, or be disabled; the effective capacity
   and availability therefore drift. `refresh_quotas` caches fill ratios, so
   placement can be briefly stale.
3. **No upload retry / backoff.** pCloud quota-full (`2008`), rate-limit (`429`),
   and auth failures (`2094`) fail the request immediately rather than retrying
   or falling back to another account. A transient backend error surfaces as a
   failed upload.
4. **Credentials are optional and coarse.** The S3 endpoint is **unauthenticated by
   default**; set `server.access_keys` to require AWS SigV4 signature verification
   (see the [operator guide](operator-guide.md#s3-authentication-sigv4)). There are not
   yet **per-key access policies, quotas, or multi-tenancy** — any valid key can reach
   every bucket. Until those land, do not expose port `9000` to untrusted networks unless
   `access_keys` is set; otherwise bind to a private/VPN interface (the deployed config
   binds a Tailscale IP).
5. **Multipart objects are read-assembled unless coalesced.** A completed multipart
   upload keeps its part blobs and is streamed back part-by-part, so a large object
   pays a per-part backend round-trip on read and every part row stays in the DB.
   `multifs object coalesce` (or `storage.auto_coalesce_multipart = true`) rewrites
   the assembled bytes as a single blob, after which reads are one fetch and the old
   parts are reclaimed by `vacuum`.
6. **`import` is destructive to unmanaged sources.** An imported object's blob is
   indistinguishable from a MultiFS-owned one, so deleting the record (and later
   `vacuum`) deletes the *source* file on pCloud. Never delete records for files
   that matter elsewhere.
7. **Web UI is read-only by design, and off by default.** It exposes only GET
   routes; enable it (`server.enable_web = true`) only on a trusted interface.
8. **NFS and WebDAV are not available.** WebDAV was removed earlier; the NFS
   stub was deleted in the dead-code cleanup. `enable_nfs` / `nfs_port` remain in
   the config schema but do nothing.
9. **Single-node, single-process.** State lives in one SQLite DB (`meta.db`) and
   one process loop; there is no clustering, leader election, or replication.
   SQLite WAL serializes writers.
10. **No encryption at rest / no integrity on the wire to pCloud beyond TLS.**
    pCloud holds blobs as plain files; checksums detect *drift*, not tampering.
11. **Vacuum uses wall-clock grace windows.** A very long in-flight read that
    outlives the superseded grace period could, in principle, race GC; the 10-min
    window is sized well above normal request durations.
12. **Deployed config lives on the host, not in the repo.** Real account emails,
    bind addresses, and quotas are instance-specific and stay in `/etc/multifs.toml`
    (see [Deploy](operator-guide.md#deploy)); `config.example.toml` is a generic template. Account
    count/quota quoted above reflect the current pool and drift as accounts are
    added.


## Future improvements

Roughly ordered by impact. Tracked as GitHub issues (see the repo's issue list for the
live backlog).

- **Redundancy / durability.** Erasure coding or N-way replication across
  accounts (the code already reserves the `.c1` blob slot and a chunk-index
  field), so a single account failure doesn't lose data. Add a repair/scrubber
  pass (`fsck --repair`) that reconstructs missing blobs.
- **Authentication & multi-tenancy.** Per-key access policies and quotas, and bucket
  namespacing, so the endpoint can be safely shared by multiple tenants. *(stage 1 —
  header-based SigV4 verification — is shipped; policies/quotas/tenancy remain)*.
- **Upload resilience.** Retry with exponential backoff on `429`/transient
  errors, and **automatic failover** to another account when a backend is
  quota-full or unauthorized.
- **CLI config management.** Implement `config set`, and account add/remove/token
  rotation without leaking secrets via argv. *(stage 1 shipped — `config get/set`,
  `account add/update-token/set` with env/stdin tokens; see the CLI reference)*
- **Packaging.** Ship MultiFS as an installable unit (deb + systemd) with a clean
  upgrade path. *(stage 1 shipped — `packaging/build-deb.sh` produces a `.deb` with units +
  conffiles; `.rpm` / static tarball are follow-ups)*
- **S3 `StorageBackend`.** Add an S3-compatible backend for providers that speak
  S3 (B2/Wasabi/R2/MinIO/AWS). *(design record)*
- **Quota freshness.** Replace the cached fill-ratio snapshot with a lightweight
  TTL + periodic re-check, and expose quota drift in `status`.
- **Web UI writes (opt-in).** Optional, authenticated upload/delete behind a flag,
  reusing the engine's write path — keeping GET-only as the safe default.
- **Object lifecycle rules.** Server-side expiry/transition policies so buckets
  can auto-tier or auto-expire instead of manual `vacuum`/`audit cleanup`.
- **Background integrity.** Scheduled `checksum verify --all` + self-healing to
  catch silent backend corruption early.

### Recently shipped

- **S3 SigV4 authentication** — optional header-based signature verification (`server.access_keys`).
- **Observability** — `/metrics` + `/healthz` + `/readyz`.
- **Multipart read optimization** — `multifs object coalesce` (single-blob rewrite).
- **CLI term cleanup** — `shard` → `backend` (with compat alias).

