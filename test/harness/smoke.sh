#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# UUDA harness smoke test. Brings up the three-container stack, exercises
# register + heartbeat end-to-end, and reports pass/fail.
#
# Usage:
#   ./smoke.sh                # full run (cleanup + up + test + cleanup)
#   ./smoke.sh --keep         # leave stack running after test for poking
#   ./smoke.sh --logs         # show container logs and exit
#   ./smoke.sh --down         # tear down and exit

set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
case "$(uname -m)" in
    arm64|aarch64) HARNESS_TARGET="aarch64-unknown-linux-gnu" ;;
    x86_64|amd64)  HARNESS_TARGET="x86_64-unknown-linux-gnu" ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac
# Never assume agent/target/. A global `target-dir` in ~/.cargo/config.toml or a
# CARGO_TARGET_DIR in the environment relocates the whole build, and this gate
# then fails with "missing linux binary" for a build that actually succeeded.
# Ask cargo where it puts things.
CARGO_TARGET_ROOT="$(cd "$REPO_ROOT/agent" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$CARGO_TARGET_ROOT" ] || CARGO_TARGET_ROOT="$REPO_ROOT/agent/target"
LINUX_BIN="$CARGO_TARGET_ROOT/$HARNESS_TARGET/release/uc-agent"
STAGED_BIN="$HARNESS_DIR/uc-agent-test/uc-agent"
COMPOSE=(docker compose --project-name uuda-harness)

log()  { printf "\033[1;36m[smoke]\033[0m %s\n" "$*"; }
ok()   { printf "\033[1;32m[ok]\033[0m %s\n" "$*"; }
fail() { printf "\033[1;31m[fail]\033[0m %s\n" "$*" >&2; exit 1; }

dump_logs() {
    log "container logs:"
    "${COMPOSE[@]}" -f "$HARNESS_DIR/docker-compose.yml" logs --tail 80 || true
}

stage_binary() {
    [ -f "$LINUX_BIN" ] || fail "missing linux binary at $LINUX_BIN
build first:
  cd $REPO_ROOT/agent && cargo zigbuild --release --target $HARNESS_TARGET -p uc-agent"
    cp "$LINUX_BIN" "$STAGED_BIN"
    log "staged $(du -h "$STAGED_BIN" | awk '{print $1}') binary at $STAGED_BIN"
}

bring_up() {
    cd "$HARNESS_DIR"
    "${COMPOSE[@]}" -f docker-compose.yml down -v --remove-orphans >/dev/null 2>&1 || true
    log "building images"
    "${COMPOSE[@]}" -f docker-compose.yml build
    log "bringing stack up"
    "${COMPOSE[@]}" -f docker-compose.yml up -d
}

bring_down() {
    cd "$HARNESS_DIR"
    "${COMPOSE[@]}" -f docker-compose.yml down -v --remove-orphans >/dev/null 2>&1 || true
}

ops_center_url() { echo "http://127.0.0.1:18084"; }

wait_for_ops_center() {
    log "waiting for ops-center-mini"
    for _ in $(seq 1 30); do
        if curl -fsS -m 2 "$(ops_center_url)/healthz" >/dev/null 2>&1; then
            ok "ops-center-mini up"
            return 0
        fi
        sleep 1
    done
    fail "ops-center-mini did not come up"
}

wait_for_register() {
    local hostname="$1"
    log "waiting for $hostname to register"
    for _ in $(seq 1 60); do
        local body
        body="$(curl -fsS -m 3 "$(ops_center_url)/api/v1/uuda/devices" 2>/dev/null || echo '[]')"
        if echo "$body" | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if any(r.get('hostname') == '$hostname' for r in d) else 1)"; then
            ok "$hostname registered"
            return 0
        fi
        sleep 1
    done
    fail "$hostname never registered. devices: $(curl -fsS "$(ops_center_url)/api/v1/uuda/devices" || true)"
}

wait_for_heartbeat() {
    local hostname="$1"
    log "waiting for $hostname heartbeat (within 30s)"
    for _ in $(seq 1 30); do
        local body
        body="$(curl -fsS -m 3 "$(ops_center_url)/api/v1/uuda/devices")"
        if echo "$body" | python3 -c "
import json,sys,datetime
d = json.load(sys.stdin)
for r in d:
    if r.get('hostname') == '$hostname' and r.get('last_heartbeat_at'):
        sys.exit(0)
sys.exit(1)
"; then
            ok "$hostname heartbeat received"
            return 0
        fi
        sleep 1
    done
    fail "$hostname never heartbeat'd"
}

verify_mtls_listener() {
    log "verifying agent's mTLS listener on :47000 presents expected cert"
    local out
    out="$(openssl s_client -connect 127.0.0.1:47000 -showcerts </dev/null 2>&1 || true)"
    echo "$out" | grep -q "CN=uuda-test-node-01" \
        || fail "agent cert subject CN not seen on mTLS listener"
    echo "$out" | grep -q "UUDA Root CA" \
        || fail "agent cert not chained to UUDA Root CA"
    ok "mTLS listener presents agent cert chained to UUDA root"
}

verify_audit_row() {
    local hostname="$1"
    log "verifying register audit row for $hostname"
    local sql="SELECT a.action, a.status FROM uuda_audit a JOIN uuda_devices d ON a.device_id = d.device_id WHERE d.hostname = '$hostname' ORDER BY a.id DESC LIMIT 5;"
    "${COMPOSE[@]}" -f "$HARNESS_DIR/docker-compose.yml" exec -T postgres \
        psql -U uuda -d uuda_test -c "$sql" \
        | tee /tmp/uuda_audit.txt
    grep -q "uuda.register" /tmp/uuda_audit.txt || fail "no uuda.register audit row found"
    ok "audit row present"
}

run_smoke() {
    stage_binary
    bring_up
    wait_for_ops_center

    local hostname="uuda-test-node-01"
    wait_for_register "$hostname"
    wait_for_heartbeat "$hostname"
    verify_mtls_listener
    verify_audit_row "$hostname"

    log "fetching device record for visual check"
    curl -fsS "$(ops_center_url)/api/v1/uuda/devices" \
        | python3 -m json.tool | head -40

    ok "smoke complete: register + heartbeat + audit verified"
}

main() {
    local action="full"
    while [ $# -gt 0 ]; do
        case "$1" in
            --keep) action="keep"; shift ;;
            --logs) action="logs"; shift ;;
            --down) action="down"; shift ;;
            *) fail "unknown arg: $1" ;;
        esac
    done

    case "$action" in
        logs) dump_logs ;;
        down) bring_down; ok "stack torn down" ;;
        keep)
            run_smoke || { dump_logs; exit 1; }
            log "stack left running. tear down with: $0 --down"
            ;;
        full)
            trap 'rc=$?; if [ $rc -ne 0 ]; then dump_logs; fi; bring_down; exit $rc' EXIT
            run_smoke
            ;;
    esac
}

main "$@"
