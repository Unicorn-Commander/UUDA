#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# UUDA agent installer. Drops the binary, lays down config + service unit,
# and triggers registration. Linux first; macOS path falls through.
#
# Required env (or pass --token / --tier / --owner):
#   UC_AGENT_TOKEN         single-use registration token from ops-center
#   UC_AGENT_TIER          "prod" | "dev" | "endpoint"
#   UC_AGENT_OWNER         "infra" | "aaron" | "shafen" | ...
#
# Optional env:
#   UC_AGENT_BINARY        path to a prebuilt binary; if unset, attempts download
#   UC_AGENT_BASE_URL      ops-center base URL; default tailnet-direct
#   UC_AGENT_DOWNLOAD_URL  direct binary download URL
#   UC_AGENT_NO_START      "1" to skip starting the service after install
#
# Usage:
#   sudo UC_AGENT_TOKEN=xxx UC_AGENT_TIER=prod UC_AGENT_OWNER=infra ./install.sh
#   sudo ./install.sh --uninstall

set -euo pipefail

PREFIX_BIN="/usr/local/libexec"
PREFIX_CFG="/etc/uc-agent"
PREFIX_STATE="/var/lib/uc-agent"
PREFIX_CERT="/var/lib/uc-agent/certs"
DEFAULT_BASE_URL="http://commander.unicorncommander.net:8084"

log()  { printf "\033[1;36m[uc-agent]\033[0m %s\n" "$*"; }
fail() { printf "\033[1;31m[uc-agent]\033[0m %s\n" "$*" >&2; exit 1; }

require_root() {
    [ "$(id -u)" -eq 0 ] || fail "must run as root"
}

detect_os() {
    case "$(uname -s)" in
        Linux)  echo linux ;;
        Darwin) echo macos ;;
        *)      fail "unsupported OS: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo x86_64 ;;
        arm64|aarch64)  echo aarch64 ;;
        *)              fail "unsupported arch: $(uname -m)" ;;
    esac
}

require_tailscale() {
    command -v tailscale >/dev/null 2>&1 || fail "tailscale CLI not found"
    tailscale ip --4 >/dev/null 2>&1 || fail "tailscale not connected; run 'tailscale up' first"
}

uninstall() {
    require_root
    log "stopping service"
    if [ "$(detect_os)" = linux ]; then
        systemctl stop uc-agent.service 2>/dev/null || true
        systemctl disable uc-agent.service 2>/dev/null || true
        rm -f /etc/systemd/system/uc-agent.service
        systemctl daemon-reload || true
    else
        launchctl bootout system/tech.magicunicorn.uc-agent 2>/dev/null || true
        rm -f /Library/LaunchDaemons/tech.magicunicorn.uc-agent.plist
    fi
    rm -f "$PREFIX_BIN/uc-agent"
    rm -rf "$PREFIX_CFG" "$PREFIX_STATE"
    log "uninstalled. tailnet membership left as-is."
}

install_binary() {
    local os="$1" arch="$2"
    if [ -n "${UC_AGENT_BINARY:-}" ]; then
        [ -f "$UC_AGENT_BINARY" ] || fail "UC_AGENT_BINARY not found: $UC_AGENT_BINARY"
        log "installing binary from $UC_AGENT_BINARY"
        install -d -m 0755 "$PREFIX_BIN"
        install -m 0755 "$UC_AGENT_BINARY" "$PREFIX_BIN/uc-agent"
    elif [ -n "${UC_AGENT_DOWNLOAD_URL:-}" ]; then
        log "downloading binary from $UC_AGENT_DOWNLOAD_URL"
        local tmp; tmp="$(mktemp)"
        curl -fsSL "$UC_AGENT_DOWNLOAD_URL" -o "$tmp"
        install -d -m 0755 "$PREFIX_BIN"
        install -m 0755 "$tmp" "$PREFIX_BIN/uc-agent"
        rm -f "$tmp"
    else
        fail "no binary available: set UC_AGENT_BINARY or UC_AGENT_DOWNLOAD_URL"
    fi
}

write_config() {
    local tier="$1" owner="$2" token="$3" base_url="$4"
    install -d -m 0750 "$PREFIX_CFG"
    cat > "$PREFIX_CFG/config.toml" <<TOML
[agent]
mcp_port = 47000
heartbeat_interval = "30s"
register_timeout = "30s"

[control_plane]
base_url = "$base_url"
tls_skip_verify = false

[paths]
state_dir = "$PREFIX_STATE"
cert_dir = "$PREFIX_CERT"

[bootstrap]
token = "$token"
tier = "$tier"
owner = "$owner"
TOML
    chmod 0640 "$PREFIX_CFG/config.toml"
    install -d -m 0750 "$PREFIX_STATE"
    install -d -m 0700 "$PREFIX_CERT"
}

install_service_linux() {
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local unit_src="$script_dir/packaging/systemd/uc-agent.service"
    [ -f "$unit_src" ] || fail "missing systemd unit at $unit_src"
    install -m 0644 "$unit_src" /etc/systemd/system/uc-agent.service
    systemctl daemon-reload
}

install_service_macos() {
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local plist_src="$script_dir/packaging/launchd/tech.magicunicorn.uc-agent.plist"
    [ -f "$plist_src" ] || fail "missing launchd plist at $plist_src"
    install -d -m 0755 /Library/LaunchDaemons
    install -m 0644 "$plist_src" /Library/LaunchDaemons/tech.magicunicorn.uc-agent.plist

    # Pre-authorize the binary in the Application Firewall so the first launch
    # doesn't pop a Gatekeeper-style prompt.
    local fw=/usr/libexec/ApplicationFirewall/socketfilterfw
    if [ -x "$fw" ]; then
        "$fw" --add "$PREFIX_BIN/uc-agent" >/dev/null 2>&1 || true
        "$fw" --unblockapp "$PREFIX_BIN/uc-agent" >/dev/null 2>&1 || true
    fi
}

apply_firewall_linux() {
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
        ufw allow in on tailscale0 to any port 47000 proto tcp >/dev/null 2>&1 || true
        log "ufw rule added: allow tailscale0 -> :47000"
    elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        firewall-cmd --permanent --zone=trusted --add-interface=tailscale0 >/dev/null 2>&1 || true
        firewall-cmd --permanent --zone=trusted --add-port=47000/tcp >/dev/null 2>&1 || true
        firewall-cmd --reload >/dev/null 2>&1 || true
        log "firewalld rule added: trusted zone <- tailscale0, port 47000"
    fi
}

start_service() {
    if [ "${UC_AGENT_NO_START:-0}" = "1" ]; then
        log "UC_AGENT_NO_START=1; skipping start"
        return 0
    fi
    if [ "$(detect_os)" = linux ]; then
        systemctl enable --now uc-agent.service
        sleep 2
        systemctl is-active --quiet uc-agent.service \
            && log "uc-agent.service active" \
            || fail "uc-agent.service failed to start; see 'journalctl -u uc-agent'"
    else
        launchctl bootstrap system /Library/LaunchDaemons/tech.magicunicorn.uc-agent.plist
        log "uc-agent loaded into launchd"
    fi
}

main() {
    local action=install
    while [ $# -gt 0 ]; do
        case "$1" in
            --uninstall) action=uninstall; shift ;;
            --token)     UC_AGENT_TOKEN="$2"; shift 2 ;;
            --tier)      UC_AGENT_TIER="$2";  shift 2 ;;
            --owner)     UC_AGENT_OWNER="$2"; shift 2 ;;
            --base-url)  UC_AGENT_BASE_URL="$2"; shift 2 ;;
            --binary)    UC_AGENT_BINARY="$2"; shift 2 ;;
            *) fail "unknown arg: $1" ;;
        esac
    done

    if [ "$action" = uninstall ]; then
        uninstall
        return 0
    fi

    require_root
    require_tailscale

    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"
    log "platform: $os/$arch"

    : "${UC_AGENT_TOKEN:?set UC_AGENT_TOKEN or pass --token}"
    : "${UC_AGENT_TIER:?set UC_AGENT_TIER or pass --tier}"
    : "${UC_AGENT_OWNER:?set UC_AGENT_OWNER or pass --owner}"
    local base_url="${UC_AGENT_BASE_URL:-$DEFAULT_BASE_URL}"

    install_binary "$os" "$arch"
    write_config "$UC_AGENT_TIER" "$UC_AGENT_OWNER" "$UC_AGENT_TOKEN" "$base_url"

    if [ "$os" = linux ]; then
        install_service_linux
        apply_firewall_linux
    else
        install_service_macos
    fi

    start_service
    log "install complete; tailing service logs:"
    if [ "$os" = linux ]; then
        journalctl -u uc-agent.service -n 30 --no-pager || true
    else
        tail -n 30 /var/log/uc-agent.log 2>/dev/null || true
    fi
}

main "$@"
