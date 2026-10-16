# MultiFS packaging

Build a Debian package for MultiFS from a release build.

```bash
packaging/build-deb.sh              # -> target/deb/multifs_<version>_amd64.deb (+ .sha256)
```

Requires `cargo`, `dpkg-deb` and `fakeroot` (no `cargo-deb`, no network).

## What the package installs

| Path | Purpose |
|------|---------|
| `/usr/local/bin/multifs` | the daemon + CLI binary |
| `/lib/systemd/system/multifs.service` | main daemon unit (S3 + optional web/metrics) |
| `/lib/systemd/system/multifs-vacuum.{service,timer}` | hourly `vacuum` GC |
| `/lib/systemd/system/multifs-mount.service` | optional rclone mount (see below) |
| `/etc/multifs.toml` | **conffile** — generic template on first install |
| `/etc/multifs.env` | **conffile**, mode 0600 — backend tokens (empty on install) |

State/cache directories (`/var/lib/multifs`, `/var/cache/multifs`) are created by
`postinst`.

## Install / upgrade / remove

```bash
sudo dpkg -i multifs_0.1.0_amd64.deb      # or: apt install ./multifs_0.1.0_amd64.deb
sudo systemctl enable --now multifs.service
```

- **Upgrade** — `/etc/multifs.toml` and `/etc/multifs.env` are Debian **conffiles**, so
  `dpkg`/`apt` preserve operator edits and only prompt when the shipped template changes.
  State (`/var/lib/multifs`) and cache (`/var/cache/multifs`) are never touched.
- **Remove** — `sudo apt remove multifs` stops/disables the daemon and removes the program
  + units, but keeps config and state.
- **Purge** — `sudo apt purge multifs` additionally removes `/var/lib/multifs`,
  `/var/cache/multifs`, `/etc/multifs.env` and `/etc/multifs`.

## First-run steps

```bash
sudoedit /etc/multifs.toml                       # bind, ports, backends
sudo multifs account add <email> --token-env VAR # writes the token to /etc/multifs.env (0600)
sudo systemctl restart multifs.service
```

## Optional: rclone mount

`multifs-mount.service` is gated on `/etc/multifs/mount.conf`; create it (mode 0644)
with the instance values, then enable the unit:

```sh
RCLONE_REMOTE=multifs-s3:video
MOUNT_POINT=/mnt/multifs
MOUNT_UID=1000
MOUNT_GID=1000
```

```bash
sudo systemctl enable --now multifs-mount.service
```

## Notes

- The package ships the **generic** `config.example.toml` as `/etc/multifs.toml` — never a
  deployment config (project rule: deployment configs are never committed; see `AGENTS.md`).
- No secrets are packaged: `/etc/multifs.env` is empty and mode 0600; tokens are added on
  the host via `multifs account add` / `account update-token`.
- Secondary targets (`.rpm`, static musl tarball) are tracked as follow-ups on issue #8.
