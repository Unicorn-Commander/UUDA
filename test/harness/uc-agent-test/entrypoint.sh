#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# uc-agent test container entrypoint. Waits for ops-center, requests a
# bootstrap token, renders the config, and execs the agent in foreground.

set -euo pipefail

log() { printf "\033[1;36m[entrypoint]\033[0m %s\n" "$*"; }
fail() { printf "\033[1;31m[entrypoint]\033[0m %s\n" "$*" >&2; exit 1; }

: "${UC_AGENT_BASE_URL:?UC_AGENT_BASE_URL must be set}"
: "${UC_AGENT_HOSTNAME:?UC_AGENT_HOSTNAME must be set}"
: "${UC_AGENT_TIER:?UC_AGENT_TIER must be set}"
: "${UC_AGENT_OWNER:?UC_AGENT_OWNER must be set}"

# The agent advertises UC_AGENT_LISTEN_IP as its dial address (tailnet.rs:
# local_ipv4 returns it verbatim) AND binds the mTLS listener to it. In the
# harness there is no tailscale0, and 0.0.0.0 is a valid BIND address but NOT a
# routable DIAL target — the gateway, in a sibling container, would try to reach
# https://0.0.0.0:47000 and fail. So resolve our real bridge IP and advertise THAT,
# which is exactly the address the `uuda-test-node-01` network alias resolves to.
detect_container_ip() {
    local ip
    ip="$(ip -4 -o route get 1.1.1.1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p')"
    [ -n "$ip" ] || ip="$(getent hosts "$(cat /etc/hostname)" 2>/dev/null | awk '{print $1; exit}')"
    [ -n "$ip" ] || ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    printf '%s' "$ip"
}

if [ -z "${UC_AGENT_LISTEN_IP:-}" ] || [ "${UC_AGENT_LISTEN_IP}" = "0.0.0.0" ]; then
    UC_AGENT_LISTEN_IP="$(detect_container_ip)"
    [ -n "$UC_AGENT_LISTEN_IP" ] || fail "could not determine container IP for the agent to advertise"
    export UC_AGENT_LISTEN_IP
    log "resolved container dial address: $UC_AGENT_LISTEN_IP"
fi

log "waiting for ops-center at $UC_AGENT_BASE_URL"
for _ in $(seq 1 60); do
    if curl -fsS -m 2 "$UC_AGENT_BASE_URL/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
curl -fsS -m 5 "$UC_AGENT_BASE_URL/healthz" >/dev/null \
    || fail "ops-center never became reachable"

log "requesting bootstrap token for $UC_AGENT_HOSTNAME"
RESP="$(curl -fsS -X POST "$UC_AGENT_BASE_URL/api/v1/uuda/tokens" \
    -H "Content-Type: application/json" \
    -d "{\"hostname\":\"$UC_AGENT_HOSTNAME\",\"tier\":\"$UC_AGENT_TIER\",\"owner\":\"$UC_AGENT_OWNER\",\"ttl_minutes\":15}")"
TOKEN="$(printf "%s" "$RESP" | jq -r '.token')"
[ -n "$TOKEN" ] && [ "$TOKEN" != "null" ] || fail "no token in response: $RESP"
log "got token (truncated): ${TOKEN:0:24}..."

sed \
    -e "s|{{BASE_URL}}|$UC_AGENT_BASE_URL|g" \
    -e "s|{{TOKEN}}|$TOKEN|g" \
    -e "s|{{TIER}}|$UC_AGENT_TIER|g" \
    -e "s|{{OWNER}}|$UC_AGENT_OWNER|g" \
    /etc/uc-agent/config.toml.template > /etc/uc-agent/config.toml
chmod 0640 /etc/uc-agent/config.toml

log "starting uc-agent (listen on $UC_AGENT_LISTEN_IP:47000)"
exec /usr/local/libexec/uc-agent
