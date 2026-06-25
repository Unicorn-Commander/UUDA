#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
UUDA full-stack agentic-loop E2E driver.

Unlike mcp_probe.py (which hits the agent's mTLS endpoint DIRECTLY), this drives
the whole governed control plane over plain HTTP against ops-center-mini, which
mounts the REAL routers (uuda + uuda_mcp gateway + uuda_drift_api). Every call
therefore exercises the production path: gateway mints a CA-trusted client cert,
dials the registered agent over mTLS, the agent serves a stateless `tools/call`,
and the policy / tenancy / approval / drift logic governs it.

It proves, against a real Rust agent, the invariants unit tests can only fake:

  GOVERNED CORE (default):
    0  inventory         the agent registered with a DIALABLE address
    1  discovery         gateway -> agent mTLS, stateless tools/list
    2  read exec         system.health returns real device state
    3  dry-run safety    a mutating tool w/o a real flag is a preview, never gated/applied
    4  policy chokepoint  a REAL mutation is held (needs_approval), not executed
    5  queue visibility  the held action is listed
    6  separation of duties  the requester cannot approve their own action (403)
    7  approval -> exec  a different operator approves; the gateway replays it
    8  effect verified   the device actually changed (brief version landed)
    9  desired-state     define checks
    10 drift detected    observe + diff over the real agent
    11 governed remediation  a fix routes through the SAME approval gate (not a backdoor)
    12 closed loop       approve the fix -> re-scan -> drift cleared

  AUTONOMY DIAL (--autonomy-only, after an auto_approve grant is seeded):
    13-16  a scoped grant lets a mutating remediation auto-run and converge,
           with no human in the loop — the fleet "autonomy dial".

Exit 0 iff every check passes. Pure stdlib (urllib) so it runs on the host python.
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request

BASE = ""
RESULTS: list[tuple[str, bool, str]] = []


def http(method: str, path: str, body=None, actor: str | None = None):
    """Return (status, parsed_body). Never raises on HTTP error — captures it."""
    url = BASE + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if actor:
        req.add_header("X-Uuda-Actor", actor)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw, st = r.read().decode(), r.status
    except urllib.error.HTTPError as e:
        raw, st = e.read().decode(), e.code
    except Exception as e:  # connection-level
        return 0, {"_error": str(e)}
    try:
        return st, json.loads(raw)
    except json.JSONDecodeError:
        return st, raw


def check(name: str, ok: bool, detail: str = "") -> bool:
    RESULTS.append((name, bool(ok), detail))
    mark = "\033[1;32mPASS\033[0m" if ok else "\033[1;31mFAIL\033[0m"
    print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""))
    return bool(ok)


def section(title: str) -> None:
    print(f"\n\033[1;36m== {title} ==\033[0m")


def _detail(body) -> str:
    """Pull a human reason out of a FastAPI error body."""
    if isinstance(body, dict):
        d = body.get("detail", body)
        return json.dumps(d) if not isinstance(d, str) else d
    return str(body)[:200]


# -- the standard brief argument shape (snake_case, per AgentBriefIn) ----------

def brief_args(version: str, by: str) -> dict:
    return {
        "version": version,
        "system_prompt": f"Fleet briefing {version}. Operate within policy.",
        "context": {"source": "uuda-e2e"},
        "tasks": [],
        "issued_by": by,
    }


def discover_device() -> str | None:
    st, devs = http("GET", "/api/v1/uuda/devices")
    if st != 200 or not isinstance(devs, list) or not devs:
        check("0. inventory: one device registered", False, f"status={st} body={_detail(devs)}")
        return None
    dev = devs[0]
    ip = dev.get("tailnet_ip")
    ok = bool(dev.get("device_id")) and bool(ip) and ip != "0.0.0.0"
    check("0. inventory: agent registered with a dialable address", ok,
          f"hostname={dev.get('hostname')} tailnet_ip={ip} device_id={(dev.get('device_id') or '')[:12]}…")
    return dev.get("device_id") if ok else None


def governed_core(dev: str) -> None:
    # 1 — discovery proves the gateway->agent mTLS + stateless tools/list path
    section("Gateway → agent (mTLS, stateless transport)")
    st, body = http("GET", f"/api/v1/uuda/mcp/{dev}/tools")
    names = []
    if st == 200 and isinstance(body, dict) and body.get("ok"):
        res = body.get("result") or {}
        tools = res.get("tools") if isinstance(res, dict) else None
        names = [t.get("name") for t in tools] if isinstance(tools, list) else []
    want = {"system.health", "system.metrics", "package.install", "service.status", "agent.brief", "agent.report"}
    check("1. discovery: gateway dialed the agent over mTLS and listed its tools",
          st == 200 and want.issubset(set(names)),
          f"status={st} tools={sorted(names)}" if names else f"status={st} body={_detail(body)}")

    # 2 — read execution returns real device state
    section("Read execution")
    st, body = http("POST", f"/api/v1/uuda/mcp/{dev}/call", {"tool": "system.health", "arguments": {}})
    res = body.get("result") if isinstance(body, dict) else {}
    mem = (res or {}).get("memory", {}) if isinstance(res, dict) else {}
    ok = (st == 200 and isinstance(body, dict) and body.get("ok")
          and bool((res or {}).get("agentVersion")) and int(mem.get("totalBytes", 0) or 0) > 0)
    check("2. read: system.health returned live device state", ok,
          (f"os={res.get('os')!r} version={res.get('agentVersion')} "
           f"mem={int(mem.get('totalBytes',0))//(1024**3)}GiB") if ok else f"status={st} body={_detail(body)}")

    # 3 — dry-run SEMANTICS (the C1 red-team hardening): an explicit dry_run:true
    #     preview is allowed without approval, but a MISSING flag is NOT a free pass —
    #     it defaults to REAL and is held. (A truthy-but-not-bool "dry_run" can't fake
    #     a preview.) Neither sub-check depends on the apt backend succeeding.
    section("Dry-run semantics (explicit preview vs. ambiguous-defaults-to-real)")
    st, prev = http("POST", f"/api/v1/uuda/mcp/{dev}/call",
                    {"tool": "package.install", "arguments": {"name": "htop", "dry_run": True}})
    check("3a. an EXPLICIT package.install dry_run:true preview is allowed (not held)",
          not (isinstance(prev, dict) and prev.get("status") == "needs_approval"),
          f"status={st} decision={prev.get('status') if isinstance(prev,dict) else 'allowed'}")
    st, real = http("POST", f"/api/v1/uuda/mcp/{dev}/call",
                    {"tool": "package.install", "arguments": {"name": "htop"}})
    check("3b. package.install with NO dry_run flag defaults to REAL → held for approval",
          st == 200 and isinstance(real, dict) and real.get("status") == "needs_approval"
          and real.get("policy_class") == "mutating",
          f"status={st} decision={real.get('status') if isinstance(real,dict) else real}")

    # 4 — a REAL mutation is held for approval, not executed
    section("Policy chokepoint + approval workflow")
    APPROVED_VERSION = "e2e-approved-1"
    st, body = http("POST", f"/api/v1/uuda/mcp/{dev}/call",
                    {"tool": "agent.brief", "arguments": brief_args(APPROVED_VERSION, "alice")}, actor="alice")
    pid = body.get("pending_id") if isinstance(body, dict) else None
    check("4. policy: a real agent.brief mutation was HELD (needs_approval), not run",
          st == 200 and isinstance(body, dict) and body.get("status") == "needs_approval"
          and pid is not None and body.get("policy_class") == "mutating",
          f"status={st} decision={body.get('status') if isinstance(body,dict) else body} pending_id={pid}")

    # 5 — the held action is visible in the queue
    st, q = http("GET", "/api/v1/uuda/mcp/approvals?status=pending", actor="alice")
    listed = isinstance(q, list) and any(p.get("id") == pid and p.get("tool") == "agent.brief"
                                         and p.get("actor_id") == "alice" for p in q)
    check("5. queue: the held action is listed for a human to review", listed,
          f"pending={[p.get('id') for p in q] if isinstance(q,list) else q}")

    # 6 — the requester cannot approve their own action
    st, body = http("POST", f"/api/v1/uuda/mcp/approvals/{pid}/approve", actor="alice")
    check("6. separation of duties: alice cannot approve her OWN action (403)",
          st == 403 and "separation of duties" in _detail(body).lower(),
          f"status={st} detail={_detail(body)}")

    # 7 — a different operator approves; the gateway replays it onto the device
    st, body = http("POST", f"/api/v1/uuda/mcp/approvals/{pid}/approve", actor="bob")
    check("7. approval → execution: bob approved; the gateway replayed the tool",
          st == 200 and isinstance(body, dict) and body.get("status") == "executed" and body.get("ok"),
          f"status={st} final={body.get('status') if isinstance(body,dict) else body}")

    # 8 — the device actually changed (the brief landed)
    st, body = http("POST", f"/api/v1/uuda/mcp/{dev}/call", {"tool": "agent.report", "arguments": {}})
    cur = (body.get("result") or {}).get("currentBriefVersion") if isinstance(body, dict) else None
    check("8. effect verified: the approved brief is live on the device", cur == APPROVED_VERSION,
          f"currentBriefVersion={cur!r} (expected {APPROVED_VERSION!r})")

    # 9..12 — desired-state drift, governed remediation, closed loop
    drift_closed_loop(dev)


def drift_closed_loop(dev: str) -> None:
    section("Desired-state drift → governed remediation → convergence")
    BASELINE = "fleet-baseline-v1"

    # 9 — declare desired state: one always-ok baseline + one that will drift,
    #     with a remediation (push the baseline brief) that actually resolves it.
    st1, _ = http("POST", "/api/v1/uuda/drift/checks", {
        "key": "e2e-agent-reachable", "probe_tool": "agent.report",
        "match_path": "hasLocalAgent", "match_op": "eq", "match_value": True,
        "description": "device runs its on-board agent (squad mode)",
    })
    st2, _ = http("POST", "/api/v1/uuda/drift/checks", {
        "key": "e2e-baseline-brief", "probe_tool": "agent.report",
        "match_path": "currentBriefVersion", "match_op": "eq", "match_value": BASELINE,
        "remediation_tool": "agent.brief", "remediation_args": brief_args(BASELINE, "uuda-drift"),
        "description": "every device must run the fleet baseline brief",
    })
    check("9. desired-state: defined a baseline check + a remediable drift check",
          st1 == 200 and st2 == 200, f"reachable={st1} baseline={st2}")

    # 10 — scan: observe + diff over the real agent
    st, rep = http("POST", f"/api/v1/uuda/drift/{dev}/scan")
    items = {i.get("key"): i.get("status") for i in rep.get("items", [])} if isinstance(rep, dict) else {}
    check("10. drift detected: baseline brief is out of desired state",
          st == 200 and rep.get("checked") == 2 and rep.get("drift", 0) >= 1
          and items.get("e2e-baseline-brief") == "drift" and items.get("e2e-agent-reachable") == "ok",
          f"checked={rep.get('checked')} ok={rep.get('ok')} drift={rep.get('drift')} items={items}")

    # 11 — remediate: the admin-defined fix is MUTATING, so it must route through
    #      the approval gate, NOT auto-run. (A fix is not a backdoor.)
    st, rem = http("POST", f"/api/v1/uuda/drift/{dev}/remediate", {"only_keys": ["e2e-baseline-brief"]}, actor="carol")
    out = {o.get("key"): o for o in rem.get("outcomes", [])} if isinstance(rem, dict) else {}
    rid = (out.get("e2e-baseline-brief") or {}).get("pending_id")
    check("11. governed remediation: the fix was HELD for approval, not auto-run",
          st == 200 and rem.get("queued", 0) >= 1
          and (out.get("e2e-baseline-brief") or {}).get("status") == "needs_approval" and rid is not None,
          f"summary={{remediated:{rem.get('remediated')},queued:{rem.get('queued')},denied:{rem.get('denied')}}} pending_id={rid}")

    # 12 — approve the remediation (different operator) and re-scan: drift clears.
    st, body = http("POST", f"/api/v1/uuda/mcp/approvals/{rid}/approve", actor="dave")
    approved_ok = st == 200 and isinstance(body, dict) and body.get("status") == "executed" and body.get("ok")
    st2, rep2 = http("POST", f"/api/v1/uuda/drift/{dev}/scan")
    items2 = {i.get("key"): i.get("status") for i in rep2.get("items", [])} if isinstance(rep2, dict) else {}
    check("12. closed loop: approved fix executed → re-scan shows drift resolved",
          approved_ok and st2 == 200 and items2.get("e2e-baseline-brief") == "ok" and rep2.get("drift", 1) == 0,
          f"approve={body.get('status') if isinstance(body,dict) else body} after_scan={items2}")


def autonomy_dial(dev: str) -> None:
    """Run AFTER an auto_approve grant (mutating, tier=dev) is seeded. A scoped
    grant is the fleet 'autonomy dial': an in-scope mutating remediation now
    auto-runs and converges with no human in the loop."""
    section("Autonomy dial (operator-opted-in auto-remediation)")
    AUTO = "autonomy-baseline-v2"
    http("POST", "/api/v1/uuda/drift/checks", {
        "key": "e2e-autonomy-brief", "probe_tool": "agent.report",
        "match_path": "currentBriefVersion", "match_op": "eq", "match_value": AUTO,
        "remediation_tool": "agent.brief", "remediation_args": brief_args(AUTO, "uuda-autonomy"),
        "description": "auto-converged baseline (autonomy dial on)",
    })
    st, rep = http("POST", f"/api/v1/uuda/drift/{dev}/scan")
    items = {i.get("key"): i.get("status") for i in rep.get("items", [])} if isinstance(rep, dict) else {}
    check("13. autonomy: the new baseline check drifts", items.get("e2e-autonomy-brief") == "drift",
          f"status={items.get('e2e-autonomy-brief')}")

    st, rem = http("POST", f"/api/v1/uuda/drift/{dev}/remediate",
                   {"only_keys": ["e2e-autonomy-brief"]}, actor="eve")
    out = {o.get("key"): o for o in rem.get("outcomes", [])} if isinstance(rem, dict) else {}
    check("14. autonomy: the grant let the mutating fix AUTO-run (no approval queued)",
          st == 200 and rem.get("remediated", 0) >= 1
          and (out.get("e2e-autonomy-brief") or {}).get("status") == "remediated",
          f"summary={{remediated:{rem.get('remediated')},queued:{rem.get('queued')}}}")

    st, rep2 = http("POST", f"/api/v1/uuda/drift/{dev}/scan")
    items2 = {i.get("key"): i.get("status") for i in rep2.get("items", [])} if isinstance(rep2, dict) else {}
    check("15. autonomy: re-scan shows the auto-remediated check converged",
          items2.get("e2e-autonomy-brief") == "ok", f"status={items2.get('e2e-autonomy-brief')}")


def main() -> int:
    global BASE
    p = argparse.ArgumentParser()
    p.add_argument("base", help="ops-center-mini base URL, e.g. http://127.0.0.1:18084")
    p.add_argument("--device", default=None, help="device_id (default: auto-discover)")
    p.add_argument("--autonomy-only", action="store_true",
                   help="run only the autonomy-dial flow (expects an auto_approve grant seeded)")
    args = p.parse_args()
    BASE = args.base.rstrip("/")

    print(f"\033[1mUUDA full-stack agentic-loop E2E\033[0m  →  {BASE}")
    dev = args.device or discover_device()
    if not dev:
        print("\n\033[1;31mno usable device — aborting\033[0m")
        return 1

    if args.autonomy_only:
        autonomy_dial(dev)
    else:
        governed_core(dev)

    passed = sum(1 for _, ok, _ in RESULTS if ok)
    total = len(RESULTS)
    print(f"\n\033[1m{'='*60}\033[0m")
    if passed == total:
        print(f"\033[1;32m✓ ALL {total} CHECKS PASSED — the agentic fleet loop works end to end.\033[0m")
        return 0
    print(f"\033[1;31m✗ {total - passed}/{total} FAILED\033[0m")
    for name, ok, detail in RESULTS:
        if not ok:
            print(f"    \033[1;31m- {name}\033[0m  ({detail})")
    return 2


if __name__ == "__main__":
    sys.exit(main())
