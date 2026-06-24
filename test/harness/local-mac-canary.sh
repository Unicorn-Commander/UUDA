#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Local macOS canary. Brings up the control plane in Docker, runs the
# native macOS arm64 uc-agent binary on the host (no sudo, no launchd),
# and verifies register + heartbeat + mTLS listener.
#
# Run from anywhere on macstudio:
#   ./local-mac-canary.sh
#
# Stop with Ctrl-C; teardown is automatic.

set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
MAC_BIN="$REPO_ROOT/agent/target/release/uc-agent"
RUN_DIR="${UC_AGENT_RUN_DIR:-$HOME/uc-agent-canary}"
COMPOSE=(docker compose --project-name uuda-mac-canary --file "$HARNESS_DIR/docker-compose.cp-only.yml")

log()  { printf "\033[1;36m[canary]\033[0m %s\n" "$*" >&2; }
ok()   { printf "\033[1;32m[ok]\033[0m %s\n" "$*" >&2; }
fail() { printf "\033[1;31m[fail]\033[0m %s\n" "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "this script is for macOS only"
[ -f "$MAC_BIN" ] || fail "missing native macOS binary at $MAC_BIN
build first:
  cd $REPO_ROOT/agent && cargo build --release -p uc-agent"

bring_up_cp() {
    log "bringing up postgres + ops-center-mini"
    "${COMPOSE[@]}" up -d --build
    log "waiting for ops-center-mini"
    for _ in $(seq 1 30); do
        if curl -fsS -m 2 "http://127.0.0.1:18084/healthz" >/dev/null 2>&1; then
            ok "control plane up"; return 0
        fi
        sleep 1
    done
    fail "ops-center-mini did not come up"
}

bring_down_cp() {
    log "tearing down control plane"
    "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    # Belt-and-suspenders: any uc-agent lingering on :47000 (e.g. from a
    # manual kill that bypassed the trap).
    local stuck
    stuck="$(lsof -ti :47000 2>/dev/null || true)"
    if [ -n "$stuck" ]; then
        kill $stuck 2>/dev/null || true
    fi
}

prepare_run_dir() {
    log "preparing $RUN_DIR (clean)"
    rm -rf "$RUN_DIR"
    mkdir -p "$RUN_DIR" "$RUN_DIR/certs"
}

issue_token() {
    local hostname="$1"
    log "issuing bootstrap token for $hostname"
    curl -fsS -X POST "http://127.0.0.1:18084/api/v1/uuda/tokens" \
        -H "Content-Type: application/json" \
        -d "{\"hostname\":\"$hostname\",\"tier\":\"endpoint\",\"owner\":\"aaron\",\"ttl_minutes\":30}" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])"
}

write_config() {
    local hostname="$1" token="$2"
    cat > "$RUN_DIR/config.toml" <<TOML
[agent]
mcp_port = 47000
heartbeat_interval = "5s"
register_timeout = "10s"

[control_plane]
base_url = "http://127.0.0.1:18084"
tls_skip_verify = false

[paths]
state_dir = "$RUN_DIR"
cert_dir = "$RUN_DIR/certs"

[bootstrap]
token = "$token"
tier = "endpoint"
owner = "aaron"
TOML
    chmod 0640 "$RUN_DIR/config.toml"
}

verify_register() {
    local hostname="$1"
    log "waiting for $hostname to register"
    for _ in $(seq 1 30); do
        local body
        body="$(curl -fsS -m 3 http://127.0.0.1:18084/api/v1/uuda/devices 2>/dev/null || echo '[]')"
        if echo "$body" | python3 -c "import json,sys; sys.exit(0 if any(r.get('hostname') == '$hostname' for r in json.load(sys.stdin)) else 1)"; then
            ok "$hostname registered"; return 0
        fi
        sleep 1
    done
    fail "$hostname never registered"
}

verify_heartbeat() {
    local hostname="$1"
    log "waiting for heartbeat (within 30s)"
    for _ in $(seq 1 30); do
        local body
        body="$(curl -fsS -m 3 http://127.0.0.1:18084/api/v1/uuda/devices)"
        if echo "$body" | python3 -c "
import json,sys
for r in json.load(sys.stdin):
    if r.get('hostname') == '$hostname' and r.get('last_heartbeat_at'):
        sys.exit(0)
sys.exit(1)
"; then
            ok "heartbeat received"; return 0
        fi
        sleep 1
    done
    fail "no heartbeat from $hostname"
}

verify_mtls() {
    log "verifying mTLS listener on :47000"
    local out
    out="$(openssl s_client -connect 127.0.0.1:47000 -showcerts </dev/null 2>&1 || true)"
    echo "$out" | grep -q "CN=$HOSTNAME_TAG" \
        || fail "agent cert subject CN not present for $HOSTNAME_TAG"
    echo "$out" | grep -q "UUDA Root CA" \
        || fail "agent cert not chained to UUDA Root CA"
    ok "mTLS listener up"
}

verify_mcp_tools() {
    log "running MCP probe (initialize + 5 tool calls)"
    if ! python3 "$HARNESS_DIR/mcp_probe.py" \
        --base "https://$HOSTNAME_TAG:47000" \
        --cert "$RUN_DIR/certs/cert.pem" \
        --key  "$RUN_DIR/certs/key.pem" \
        --ca   "$RUN_DIR/certs/ca-chain.pem" \
        --resolve "$HOSTNAME_TAG=127.0.0.1" >&2; then
        fail "MCP probe failed"
    fi
    ok "MCP probe: all 5 tools returned valid responses"
}

verify_mtls_healthz() {
    log "calling /healthz over mTLS using agent's cert as client cert"
    local body
    body="$(curl -s --cacert "$RUN_DIR/certs/ca-chain.pem" \
                  --cert "$RUN_DIR/certs/cert.pem" \
                  --key  "$RUN_DIR/certs/key.pem" \
                  --resolve "$HOSTNAME_TAG:47000:127.0.0.1" \
                  "https://$HOSTNAME_TAG:47000/healthz" 2>&1 || true)"
    echo "$body" | python3 -c "import json,sys; j=json.loads(sys.stdin.read()); sys.exit(0 if j.get('status') == 'ok' else 1)" \
        || fail "/healthz over mTLS did not return ok. got: $body"
    ok "GET /healthz via mTLS returned ok"

    log "calling /version over mTLS"
    body="$(curl -s --cacert "$RUN_DIR/certs/ca-chain.pem" \
                  --cert "$RUN_DIR/certs/cert.pem" \
                  --key  "$RUN_DIR/certs/key.pem" \
                  --resolve "$HOSTNAME_TAG:47000:127.0.0.1" \
                  "https://$HOSTNAME_TAG:47000/version" 2>&1 || true)"
    local agent_version
    agent_version="$(echo "$body" | python3 -c "import json,sys; j=json.loads(sys.stdin.read()); print(j.get('agent_version',''))")"
    [ -n "$agent_version" ] || fail "/version over mTLS did not return version. got: $body"
    ok "GET /version via mTLS returned agent_version=$agent_version"
}

run_agent_in_background() {
    local logfile="$RUN_DIR/agent.log"
    log "starting native uc-agent (logs: $logfile)"
    UC_AGENT_CONFIG="$RUN_DIR/config.toml" \
    UC_AGENT_HOSTNAME="$HOSTNAME_TAG" \
    UC_AGENT_LISTEN_IP="127.0.0.1" \
    RUST_LOG="info,uc_agent=debug,uc_toolkit=debug" \
        "$MAC_BIN" >"$logfile" 2>&1 &
    AGENT_PID=$!
    log "agent pid=$AGENT_PID"
}

main() {
    HOSTNAME_TAG="${UC_AGENT_HOSTNAME:-macstudio-canary}"

    AGENT_PID=""
    trap '[ -n "$AGENT_PID" ] && kill "$AGENT_PID" 2>/dev/null || true; bring_down_cp; exit ${EXIT_CODE:-0}' EXIT

    bring_up_cp

    prepare_run_dir
    local token; token="$(issue_token "$HOSTNAME_TAG")"
    write_config "$HOSTNAME_TAG" "$token"
    run_agent_in_background

    sleep 2
    if ! kill -0 "$AGENT_PID" 2>/dev/null; then
        log "agent exited early; tail of log:"
        tail -40 "$RUN_DIR/agent.log" || true
        fail "agent died before register"
    fi

    verify_register "$HOSTNAME_TAG"
    verify_heartbeat "$HOSTNAME_TAG"
    verify_mtls
    verify_mtls_healthz
    verify_mcp_tools

    log "device record:"
    curl -fsS http://127.0.0.1:18084/api/v1/uuda/devices | python3 -m json.tool

    log "agent log tail:"
    tail -30 "$RUN_DIR/agent.log"

    ok "macOS canary green: native binary registered + heartbeat + mTLS"
    log "agent still running. Ctrl-C to stop and tear down. State at: $RUN_DIR"

    wait "$AGENT_PID"
}

main "$@"
