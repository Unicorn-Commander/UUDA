#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# UUDA FULL-STACK smoke test. Brings up postgres + ops-center-mini (mounting the
# real gateway + drift routers) + the cross-compiled agent, then drives the
# COMPLETE agentic loop (discovery → read → policy gate → approval → drift →
# governed remediation → convergence → autonomy dial) via e2e_probe.py.
#
# This is the superset of smoke.sh: smoke.sh proves Phase-1 register/heartbeat;
# this proves the whole governed fleet plane against the real agent.
#
# Usage:
#   ./smoke-full.sh           # full run (cleanup + up + drive + cleanup)
#   ./smoke-full.sh --keep    # leave the stack up afterwards for poking
#   ./smoke-full.sh --down     # tear down and exit

set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
case "$(uname -m)" in
    arm64|aarch64) HARNESS_TARGET="aarch64-unknown-linux-gnu" ;;
    x86_64|amd64)  HARNESS_TARGET="x86_64-unknown-linux-gnu" ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac
LINUX_BIN="$REPO_ROOT/agent/target/$HARNESS_TARGET/release/uc-agent"
STAGED_BIN="$HARNESS_DIR/uc-agent-test/uc-agent"
COMPOSE=(docker compose --project-name uuda-harness)
OPS_URL="http://127.0.0.1:18084"
HOSTNAME_NODE="uuda-test-node-01"

log()  { printf "\033[1;36m[smoke-full]\033[0m %s\n" "$*"; }
ok()   { printf "\033[1;32m[ok]\033[0m %s\n" "$*"; }
fail() { printf "\033[1;31m[fail]\033[0m %s\n" "$*" >&2; exit 1; }

dump_logs() { log "container logs:"; "${COMPOSE[@]}" -f "$HARNESS_DIR/docker-compose.yml" logs --tail 100 || true; }

psql_q() { "${COMPOSE[@]}" -f "$HARNESS_DIR/docker-compose.yml" exec -T postgres psql -U uuda -d uuda_test "$@"; }

stage_binary() {
    [ -f "$LINUX_BIN" ] || fail "missing linux binary at $LINUX_BIN
build first:
  cd $REPO_ROOT/agent && cargo zigbuild --release --target $HARNESS_TARGET -p uc-agent"
    cp "$LINUX_BIN" "$STAGED_BIN"
    log "staged $(du -h "$STAGED_BIN" | awk '{print $1}') agent binary"
}

bring_up() {
    cd "$HARNESS_DIR"
    "${COMPOSE[@]}" -f docker-compose.yml down -v --remove-orphans >/dev/null 2>&1 || true
    log "building images"
    "${COMPOSE[@]}" -f docker-compose.yml build
    log "bringing the full stack up"
    "${COMPOSE[@]}" -f docker-compose.yml up -d
}

bring_down() { cd "$HARNESS_DIR"; "${COMPOSE[@]}" -f docker-compose.yml down -v --remove-orphans >/dev/null 2>&1 || true; }

wait_for_ops_center() {
    log "waiting for ops-center-mini (full control plane mounted)"
    for _ in $(seq 1 40); do
        curl -fsS -m 2 "$OPS_URL/healthz" >/dev/null 2>&1 && { ok "ops-center-mini up"; return 0; }
        sleep 1
    done
    fail "ops-center-mini did not come up"
}

wait_for_register() {
    log "waiting for $HOSTNAME_NODE to register (CSR → CA-signed cert)"
    for _ in $(seq 1 60); do
        local body; body="$(curl -fsS -m 3 "$OPS_URL/api/v1/uuda/devices" 2>/dev/null || echo '[]')"
        echo "$body" | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if any(r.get('hostname')=='$HOSTNAME_NODE' for r in d) else 1)" \
            && { ok "$HOSTNAME_NODE registered"; return 0; }
        sleep 1
    done
    fail "$HOSTNAME_NODE never registered. devices: $(curl -fsS "$OPS_URL/api/v1/uuda/devices" || true)"
}

wait_for_heartbeat() {
    log "waiting for first heartbeat"
    for _ in $(seq 1 30); do
        local body; body="$(curl -fsS -m 3 "$OPS_URL/api/v1/uuda/devices")"
        echo "$body" | python3 -c "
import json,sys
d=json.load(sys.stdin)
sys.exit(0 if any(r.get('hostname')=='$HOSTNAME_NODE' and r.get('last_heartbeat_at') for r in d) else 1)" \
            && { ok "heartbeat received"; return 0; }
        sleep 1
    done
    fail "$HOSTNAME_NODE never heartbeat'd"
}

seed_autonomy_grant() {
    # The fleet "autonomy dial": a scoped grant that auto-approves MUTATING
    # remediations on tier=dev. Seeded only AFTER the governed-core run so it can't
    # mask the needs_approval / separation-of-duties proofs.
    log "seeding an auto_approve grant (role=admin, mutating, scope tier=dev)"
    psql_q -c "INSERT INTO uuda_policy_rules (subject_role, classes, scope_tier, auto_approve, note, created_by)
               VALUES ('admin', ARRAY['mutating'], 'dev', true, 'e2e autonomy-dial demo', 'uuda-e2e');" >/dev/null
}

dump_audit() {
    log "the governed audit trail (uuda_audit — every fleet action, recorded):"
    psql_q -c "SELECT to_char(started_at,'HH24:MI:SS') t, actor_id, action, status FROM uuda_audit ORDER BY id;" || true
    log "approval queue (uuda_pending_actions — held → resolved):"
    psql_q -c "SELECT id, actor_id, tool, status, reviewed_by FROM uuda_pending_actions ORDER BY id;" || true
}

run() {
    stage_binary
    bring_up
    wait_for_ops_center
    wait_for_register
    wait_for_heartbeat

    log "driving the governed agentic loop"
    python3 "$HARNESS_DIR/e2e_probe.py" "$OPS_URL" || { dump_logs; fail "governed-core E2E failed"; }

    seed_autonomy_grant
    log "driving the autonomy-dial flow"
    python3 "$HARNESS_DIR/e2e_probe.py" "$OPS_URL" --autonomy-only || { dump_logs; fail "autonomy-dial E2E failed"; }

    dump_audit
    echo
    ok "FULL agentic loop verified end to end against the real uc-agent."
}

main() {
    local action="full"
    [ $# -gt 0 ] && case "$1" in
        --keep) action="keep" ;; --down) action="down" ;; --logs) action="logs" ;;
        *) fail "unknown arg: $1" ;;
    esac
    case "$action" in
        logs) dump_logs ;;
        down) bring_down; ok "stack torn down" ;;
        keep) run; log "stack left running. tear down with: $0 --down" ;;
        full) trap 'rc=$?; [ $rc -ne 0 ] && dump_logs; bring_down; exit $rc' EXIT; run ;;
    esac
}

main "$@"
