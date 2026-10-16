#!/usr/bin/env bash
# Build a Debian package for MultiFS from a release build.
#
# Usage:  packaging/build-deb.sh [OUTPUT_DIR]
# Requires: cargo, dpkg-deb, fakeroot (no cargo-deb / no network).
#
# Layout produced inside the .deb:
#   /usr/local/bin/multifs                     (binary)
#   /lib/systemd/system/multifs*.service|.timer (units)
#   /etc/multifs.toml                           (conffile — generic placeholder)
#   /etc/multifs.env                            (conffile placeholder, 0600 on install)
# State dirs (/var/lib/multifs, /var/cache/multifs) are created by postinst.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
out_dir="${1:-$repo/target/deb}"

# Package metadata (kept in sync with Cargo.toml).
name="multifs"
version="$(grep -m1 '^version' "$repo/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"
arch="$(dpkg --print-architecture)"
deb_arch=$( [ "$arch" = "amd64" ] && echo amd64 || echo "$arch" )
pkg="${name}_${version}_${deb_arch}"

echo "==> Building release binary"
( cd "$repo" && cargo build --release )

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$pkg"
mkdir -p "$root/DEBIAN" \
         "$root/usr/local/bin" \
         "$root/lib/systemd/system" \
         "$root/etc"

echo "==> Staging files"
install -m 0755 "$repo/target/release/multifs" "$root/usr/local/bin/multifs"
install -m 0644 "$here/multifs.service"          "$root/lib/systemd/system/multifs.service"
install -m 0644 "$here/multifs-vacuum.service"   "$root/lib/systemd/system/multifs-vacuum.service"
install -m 0644 "$here/multifs-vacuum.timer"     "$root/lib/systemd/system/multifs-vacuum.timer"
install -m 0644 "$here/multifs-mount.service"    "$root/lib/systemd/system/multifs-mount.service"

# Generic, non-secret config template installed as a conffile.
install -m 0644 "$repo/config.example.toml" "$root/etc/multifs.toml"
# Empty env file (tokens added by the operator); conffile so edits survive upgrades.
: > "$root/etc/multifs.env"
chmod 0600 "$root/etc/multifs.env"
# Mark both /etc files as conffiles so dpkg preserves operator edits on upgrade.
cat > "$root/DEBIAN/conffiles" <<'EOF'
/etc/multifs.toml
/etc/multifs.env
EOF

install -m 0755 "$here/postinst" "$root/DEBIAN/postinst"
install -m 0755 "$here/prerm"    "$root/DEBIAN/prerm"
install -m 0755 "$here/postrm"   "$root/DEBIAN/postrm"

installed_size="$(du -sk "$root" | cut -f1)"
cat > "$root/DEBIAN/control" <<EOF
Package: $name
Version: $version
Section: net
Priority: optional
Architecture: $deb_arch
Maintainer: Nova Claw <novaclawhk@github.com>
Installed-Size: $installed_size
Depends: libc6
Recommends: rclone
Description: Multi-cloud storage pool with an S3-compatible API
 MultiFS pools many independent storage backends (pCloud accounts, local disk)
 behind a single S3-compatible endpoint, with copy-on-write MVCC versioning and
 an optional read-only web UI and Prometheus metrics endpoint.
Homepage: https://github.com/nova-sudoku-architect/multifs
EOF

mkdir -p "$out_dir"
echo "==> Building $out_dir/$pkg.deb"
fakeroot dpkg-deb --build --root-owner-group "$root" "$out_dir/$pkg.deb" >/dev/null
( cd "$out_dir" && sha256sum "$pkg.deb" > "$pkg.deb.sha256" )

echo "==> Done"
ls -l "$out_dir/$pkg.deb" "$out_dir/$pkg.deb.sha256"
echo
echo "Inspect:  dpkg-deb --info $out_dir/$pkg.deb"
echo "          dpkg-deb --contents $out_dir/$pkg.deb"
