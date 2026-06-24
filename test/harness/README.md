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
│   ├── Dockerfile                  python + asyncpg + cryptography + fastapi
│   ├── requirements.txt
│   ├── main.py                     FastAPI app, mounts the real uuda router
│   ├── server_stub.py              get_current_user / require_admin stubs
│   └── connection.py               asyncpg pool helper
├── uc-agent-test/
│   ├── Dockerfile                  ubuntu 24.04 + cross-compiled binary
│   ├── config.toml.template
│   └── entrypoint.sh               waits for ops-center, requests token, starts agent
└── smoke.sh                        end-to-end driver
```

## Requirements

- Docker Desktop running (already present on macstudio).
- Cross-compiled Linux binary at `agent/target/x86_64-unknown-linux-gnu/release/uc-agent`
  (rebuild via `cargo zigbuild --release --target x86_64-unknown-linux-gnu -p uc-agent`).

## Run

```
cd test/harness
./smoke.sh
```

Smoke script tears down any prior stack, brings up postgres + ops-center-mini,
applies the schema, brings up the agent, drives the test sequence, and cleans up
on exit. Failure at any step prints the relevant container logs.

## What this does NOT cover

- macOS-specific tool backends (brew, launchctl, log show). Those need real
  hosts; verified separately on macstudio when we canary.
- Real Tailscale ACL or MagicDNS. The agent talks to ops-center via the
  Docker network; `UC_AGENT_LISTEN_IP` overrides the tailnet probe.
- Real Keycloak auth. ops-center-mini stubs `get_current_user` / `require_admin`
  so we can exercise the API surface without a full SSO loop.
- The MCP gateway proxy in production ops-center. We hit the agent's MCP
  endpoint directly with a registered client cert. Gateway tests come when
  we wire that up in the real ops-center.
