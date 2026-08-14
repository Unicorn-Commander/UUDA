#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Builds a .deb for uc-agent from an already cross-compiled binary. Plain files
# plus dpkg-deb, no build system, no fakeroot.
#
# Usage:
#   ./build-deb.sh --binary <path> --arch amd64|arm64 [--version X.Y.Z] [--out DIR]
#
# Env equivalents: UC_AGENT_BINARY, UC_DEB_ARCH, UC_DEB_VERSION, UC_DEB_OUT.
#
# Typical run, from the repo root:
#   cd agent
#   cargo zigbuild --release --target x86_64-unknown-linux-gnu  -p uc-agent
#   cargo zigbuild --release --target aarch64-unknown-linux-gnu -p uc-agent
#   packaging/build-deb.sh --arch amd64 \
#       --binary target/x86_64-unknown-linux-gnu/release/uc-agent
#   packaging/build-deb.sh --arch arm64 \
#       --binary target/aarch64-unknown-linux-gnu/release/uc-agent
#
# Installed layout is exactly what install.sh lays down:
#   /usr/local/libexec/uc-agent
#   /etc/systemd/system/uc-agent.service
#   /etc/uc-agent/            (config.example.toml; config.toml is the operator's)
#   /var/lib/uc-agent/certs/
#
# The package never starts the service. Registration needs a single-use
# bootstrap token in config.toml, and a start without one just burns the
# unit's restart budget.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$AGENT_DIR/.." && pwd)"

BINARY="${UC_AGENT_BINARY:-}"
ARCH="${UC_DEB_ARCH:-}"
VERSION="${UC_DEB_VERSION:-}"
OUT_DIR="${UC_DEB_OUT:-$REPO_ROOT/dist}"

PKG_NAME="uc-agent"
MAINTAINER="Magic Unicorn Tech <hello@magicunicorn.tech>"
HOMEPAGE="https://unicorncommander.ai"

log()  { printf "\033[1;36m[build-deb]\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33m[build-deb]\033[0m %s\n" "$*" >&2; }
fail() { printf "\033[1;31m[build-deb]\033[0m %s\n" "$*" >&2; exit 1; }

usage() {
    sed -n '3,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary)  BINARY="$2";  shift 2 ;;
        --arch)    ARCH="$2";    shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --out)     OUT_DIR="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) fail "unknown arg: $1 (see --help)" ;;
    esac
done

command -v dpkg-deb >/dev/null 2>&1 \
    || fail "dpkg-deb not found (Debian/Ubuntu: apt install dpkg; macOS: brew install dpkg)"

[ -n "$BINARY" ] || fail "no binary: pass --binary or set UC_AGENT_BINARY"
[ -f "$BINARY" ] || fail "binary not found: $BINARY"

case "$ARCH" in
    amd64|arm64) ;;
    "") fail "no architecture: pass --arch amd64|arm64" ;;
    *)  fail "unsupported arch: $ARCH (amd64|arm64)" ;;
esac

if [ -z "$VERSION" ]; then
    # [workspace.package] version in agent/Cargo.toml is the release version.
    VERSION="$(awk '
        /^\[workspace\.package\]/ { in_pkg = 1; next }
        /^\[/                     { in_pkg = 0 }
        in_pkg && /^version[[:space:]]*=/ {
            gsub(/[^0-9A-Za-z.+~-]/, "", $3); print $3; exit
        }' "$AGENT_DIR/Cargo.toml")"
fi
[ -n "$VERSION" ] || fail "could not determine version; pass --version"

UNIT_SRC="$SCRIPT_DIR/systemd/uc-agent.service"
CONFIG_SRC="$SCRIPT_DIR/config.example.toml"
[ -f "$UNIT_SRC" ]   || fail "missing systemd unit at $UNIT_SRC"
[ -f "$CONFIG_SRC" ] || fail "missing example config at $CONFIG_SRC"

# An ELF header check is cheap and catches the mistake this script invites:
# packaging the amd64 binary with --arch arm64. e_machine lives at offset 18,
# little-endian: 0x3e = x86-64, 0xb7 = aarch64.
verify_elf_arch() {
    local bin="$1" want="$2" magic machine
    magic="$(od -An -t x1 -j 0 -N 4 "$bin" | tr -d ' \n')"
    [ "$magic" = "7f454c46" ] || fail "not an ELF binary: $bin"
    machine="$(od -An -t x1 -j 18 -N 2 "$bin" | tr -d ' \n')"
    case "$want:$machine" in
        amd64:3e00|arm64:b700) ;;
        *) fail "binary is not $want (ELF e_machine=0x${machine:0:2}): $bin" ;;
    esac
}
verify_elf_arch "$BINARY" "$ARCH"

# Depends must reflect the binary, not a guess. The agent links rustls (no
# OpenSSL) and bundles SQLite, so the runtime surface is glibc + libgcc. The
# floor comes from the highest GLIBC_2.x version reference in the binary
# itself; grep -a avoids needing binutils on the build host.
detect_depends() {
    local bin="$1" minor
    minor="$(LC_ALL=C grep -a -o 'GLIBC_2\.[0-9]\{1,3\}' "$bin" \
             | sed 's/GLIBC_2\.//' | sort -n | tail -1 || true)"
    if [ -n "$minor" ]; then
        printf 'libc6 (>= 2.%s), libgcc-s1\n' "$minor"
    else
        warn "no GLIBC_2.x references found; falling back to an unversioned libc6 dependency"
        printf 'libc6, libgcc-s1\n'
    fi
}
DEPENDS="$(detect_depends "$BINARY")"

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/uc-agent-deb.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

install -d -m 0755 "$STAGE/DEBIAN"
install -d -m 0755 "$STAGE/usr/local/libexec"
install -d -m 0755 "$STAGE/etc/systemd/system"
install -d -m 0750 "$STAGE/etc/uc-agent"
install -d -m 0750 "$STAGE/var/lib/uc-agent"
install -d -m 0700 "$STAGE/var/lib/uc-agent/certs"
install -d -m 0755 "$STAGE/usr/share/doc/$PKG_NAME"

install -m 0755 "$BINARY"     "$STAGE/usr/local/libexec/uc-agent"
install -m 0644 "$UNIT_SRC"   "$STAGE/etc/systemd/system/uc-agent.service"
install -m 0640 "$CONFIG_SRC" "$STAGE/etc/uc-agent/config.example.toml"

cat > "$STAGE/usr/share/doc/$PKG_NAME/copyright" <<'DOC'
Upstream-Name: uuda
Source: https://git.unicorncommander.ai/UnicornCommander/UUDA

Files: *
Copyright: Magic Unicorn Tech
License: Apache-2.0
 Licensed under the Apache License, Version 2.0. The full text ships in the
 source tree as LICENSE-APACHE and is available at
 https://www.apache.org/licenses/LICENSE-2.0
DOC
chmod 0644 "$STAGE/usr/share/doc/$PKG_NAME/copyright"

INSTALLED_SIZE="$(du -sk "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: $PKG_NAME
Version: $VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Section: admin
Priority: optional
Homepage: $HOMEPAGE
Depends: $DEPENDS
Recommends: smartmontools
Suggests: zfsutils-linux
Installed-Size: $INSTALLED_SIZE
Description: UUDA per-device agent
 uc-agent registers with the UUDA control plane, heartbeats, and serves a
 typed, read-mostly tool surface over mTLS on TCP 47000, bound to the tailnet
 address only. It runs no LLM.
 .
 The service is installed but not started: registration needs a single-use
 bootstrap token in /etc/uc-agent/config.toml (see config.example.toml), and
 the unit requires a running tailscaled.
 .
 smartmontools is recommended because the disk.smart probe shells out to
 smartctl; without it the probe reports smartctl_available: false rather than
 failing.
CONTROL
chmod 0644 "$STAGE/DEBIAN/control"

cat > "$STAGE/DEBIAN/conffiles" <<'CONFFILES'
/etc/systemd/system/uc-agent.service
/etc/uc-agent/config.example.toml
CONFFILES
chmod 0644 "$STAGE/DEBIAN/conffiles"

cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e

case "$1" in
    configure)
        install -d -m 0750 /etc/uc-agent
        install -d -m 0750 /var/lib/uc-agent
        install -d -m 0700 /var/lib/uc-agent/certs

        # No systemd in a container or a chroot; do not pretend otherwise.
        if [ -d /run/systemd/system ]; then
            systemctl daemon-reload || true
            # $2 is the previously configured version: set on upgrade, empty on
            # first install. Only ever restart something already running; never
            # start the service here.
            if [ -n "$2" ] && systemctl is-active --quiet uc-agent.service; then
                systemctl try-restart uc-agent.service || true
            fi
        fi

        if [ ! -f /etc/uc-agent/config.toml ]; then
            echo "uc-agent: installed but NOT started." >&2
            echo "uc-agent: write /etc/uc-agent/config.toml (see /etc/uc-agent/config.example.toml)," >&2
            echo "uc-agent: then run: systemctl enable --now uc-agent.service" >&2
        fi
        ;;
esac

exit 0
POSTINST
chmod 0755 "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e

case "$1" in
    remove|deconfigure)
        if [ -d /run/systemd/system ]; then
            systemctl stop uc-agent.service || true
            systemctl disable uc-agent.service || true
        fi
        ;;
esac

exit 0
PRERM
chmod 0755 "$STAGE/DEBIAN/prerm"

# Purge drops config and device state, matching `install.sh --uninstall`. That
# includes the device certificate, so a purged host needs a fresh bootstrap
# token to come back.
cat > "$STAGE/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e

case "$1" in
    remove)
        if [ -d /run/systemd/system ]; then
            systemctl daemon-reload || true
        fi
        ;;
    purge)
        rm -rf /var/lib/uc-agent /etc/uc-agent
        if [ -d /run/systemd/system ]; then
            systemctl daemon-reload || true
        fi
        ;;
esac

exit 0
POSTRM
chmod 0755 "$STAGE/DEBIAN/postrm"

install -d -m 0755 "$OUT_DIR"
DEB_PATH="$OUT_DIR/${PKG_NAME}_${VERSION}_${ARCH}.deb"
rm -f "$DEB_PATH"

BUILD_ARGS="--build"
# --root-owner-group (dpkg >= 1.19) means the package is reproducible without
# running the build as root or under fakeroot.
if dpkg-deb --help 2>&1 | grep -q -- --root-owner-group; then
    BUILD_ARGS="--root-owner-group --build"
fi

# shellcheck disable=SC2086
dpkg-deb $BUILD_ARGS "$STAGE" "$DEB_PATH" >/dev/null

log "built $DEB_PATH"
log "depends: $DEPENDS"
dpkg-deb --contents "$DEB_PATH" | awk '{print "  " $6, $1}'
