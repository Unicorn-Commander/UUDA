# UUDA test harness

End-to-end sandbox for the agent + control plane. Three containers,
one Docker network, no real fleet exposure. Lets us exercise:

- Registration (CSR signed by ops-center-issued root CA)
- Heartbeat with `X-Agent-Key`
- mTLS listener on the agent (port 47000 inside the container network)
- MCP tool calls direct to the agent
- Audit-log writes

## Stack

```
test/harness/
├── README.md                       (this file)
├── docker-compose.yml
├── ops-center-mini/
│   ├── Dockerfile                  python + asyncpg + cryptography + httpx + fastapi
│   ├── requirements.txt
│   ├── main.py                     FastAPI app, mounts uuda + uuda_mcp + uuda_drift_api
│   ├── server_stub.py              get_current_user / require_admin (X-Uuda-Actor identity)
│   └── connection.py               asyncpg pool helper
├── uc-agent-test/
│   ├── Dockerfile                  ubuntu 24.04 + cross-compiled binary
│   ├── config.toml.template        squad mode on (agent.brief round-trip is observable)
│   └── entrypoint.sh               waits for ops-center, resolves dial IP, starts agent
├── e2e_probe.py                    full governed-loop driver (drives ops-center-mini)
├── mcp_probe.py                    direct mTLS probe of the agent (bypasses the gateway)
├── smoke.sh                        Phase-1 driver: register + heartbeat + mTLS + audit
└── smoke-full.sh                   FULL driver: the whole governed agentic loop
```

## Requirements

- Docker Desktop running.
- A Linux `uc-agent` binary at `agent/target/<host-target>/release/uc-agent`
  (`smoke.sh`/`smoke-full.sh` pick the target for the host arch and stage it).
  Rebuild via `cargo zigbuild --release --target aarch64-unknown-linux-gnu -p uc-agent`
  (or `x86_64-...` on an Intel host). The binary must include the stateless MCP
  serve fix (`fix/mcp-stateless-transport`), or the gateway's bare `tools/call` 422s.

## Run

```
cd test/harness
./smoke.sh          # Phase 1: register + heartbeat + mTLS listener + audit row
./smoke-full.sh     # FULL governed agentic loop (superset of smoke.sh)
```

`smoke-full.sh` brings up postgres + ops-center-mini (mounting the REAL gateway +
drift routers) + the agent, then runs `e2e_probe.py` to drive — and assert — the
whole loop against the live agent: gateway→agent mTLS discovery, a `system.health`
read, the dry-run hardening, the policy chokepoint (a real mutation is held), the
approval workflow incl. **separation of duties** (the requester can't approve their
own action), effect verification (the brief actually lands), desired-state **drift
detection**, **governed remediation** (the fix routes through the same approval
gate), the closed loop (approve → re-scan → drift cleared), and the **autonomy
dial** (a scoped `auto_approve` grant lets an in-scope mutating remediation
auto-run). It finishes by printing the `uuda_audit` trail + approval queue.
`--keep` leaves the stack up; `--down` tears it down.

## What this does NOT cover

- macOS-specific tool backends (brew, launchctl, log show). Those need real
  hosts; verified separately on macstudio when we canary. (In the Linux harness
  `package.install`'s apt backend is also a no-op — the pruned apt lists make a
  dry-run error; the harness asserts the *governance* decision, not the backend.)
- Real Tailscale ACL or MagicDNS. The agent talks to ops-center over the Docker
  network; `entrypoint.sh` resolves the container's bridge IP and advertises it
  as the dial address (the production analogue is the tailnet IP).
- Real Keycloak auth. ops-center-mini stubs `get_current_user` / `require_admin`,
  taking the acting identity from an `X-Uuda-Actor` header so the driver can act
  as distinct operators (which is what makes the separation-of-duties test real).
