# UUDA Architecture

## What UUDA actually is

UUDA is the actuator layer for managing the Magic Unicorn fleet (and, eventually, external customer fleets via an MSP offering). It is *not* an MDM, does *not* manage iOS/Android, does *not* compete with Jamf/Kandji on FileVault attestation or compliance evidence. It serves servers and workstations on a private mesh.

The product story above the actuator is the **LtCol officer model**: each device (or device group) is shepherded by a Brigade persona — Katie for KDE/Linux end-user workstations, Edge for Linux edge nodes, McKenzy for macOS, Winston for Windows (Phase 6+), per-machine Colonels for full-stack UC rollouts. The Colonel sits above the LtCols; The General sits above the network. UUDA is what the LtCols stand on. Brigade is where the LtCols live.

## System overview — two modes

UUDA's `uc-agent` runs in one of two modes from the same binary:

### Headless mode (servers)

```
┌─────────────────────────────────────────────────────────┐
│  Brigade (LtCol Edge / LtCol Katie-server / Maj.DevOps) │
└────────────────┬────────────────────────────────────────┘
                 │ MCP / HTTPS (via gateway)
                 ▼
┌─────────────────────────────────────────────────────────┐
│  uc-control  (in Ops Center @ unicorncommander.ai)      │
│  - device registry, audit, policy, MCP gateway          │
└────────────────┬────────────────────────────────────────┘
                 │ mTLS over tailnet
                 ▼
┌─────────────────────────────────────────────────────────┐
│  uc-agent (headless daemon)                             │
│    typed MCP tool surface, mTLS, heartbeat              │
└─────────────────────────────────────────────────────────┘
```

Used for servers: `commander`, `magicunicorn`, `gfl`, `midboy1`, `centerdeep`, etc. No on-device AI; uc-agent is purely an actuator.

### Squad mode (user workstations)

```
┌──────────────┐                 ┌────────────────────────┐
│  End user    │ ─────────────▶  │  Unicorn Squad UI      │
└──────────────┘                 └─────────┬──────────────┘
                                           │
                                           ▼
                                ┌─────────────────────────┐
                                │  on-device LtCol Katie  │
                                │  (LLM agent, continuity) │
                                └────┬─────────────────┬──┘
                                     │ tools           │ briefings
                                     ▼                 ▼
                                ┌─────────┐   ┌─────────────────┐
                                │uc-agent │ ◀ │ Brigade-side    │
                                │(local)  │   │ LtCol Katie     │
                                └─────────┘   └─────────────────┘
```

Used for user workstations: future KDE devices, the Strix Halo machines, future MSP customer machines. The on-device persona is the **same character** the user talks to in Squad and the **same character** Brigade orchestrates fleet-side. Brigade pushes briefings and prompt updates via uc-agent's `agent.brief`; the on-device agent reports back via `agent.report`.

### Mode-selection rules

| Device | Mode | Reason |
|---|---|---|
| Servers (`tier=prod`, headless Linux) | Headless | No end user; the LtCol is fleet-side only. |
| Workstations (`tier=endpoint`) | Squad-when-installed | Squad is what makes the device feel managed-with-personality. |
| Per-customer UC stacks | Squad with per-machine Colonel | Customer gets a named individual (e.g. `Col. Smith`). |

uc-agent never runs an LLM itself. The on-device LLM is a separate process (Unicorn Squad). uc-agent is the briefing relay and the typed-tool surface.

## What uc-agent serves

```
┌─────────────────────────────────────────────────────────┐
│  uc-agent (one binary, two modes)                       │
│    typed MCP tool surface (5 in P1, ~30 in P2)          │
│    mTLS over tailnet on :47000                          │
│    state reporter (heartbeat every 30s)                 │
│    Phase 2+: agent.brief / agent.report relay           │
│    Phase 5: self-update + crash-restart                 │
└─────────────────────────────────────────────────────────┘
```

## Three layers, one identity model

1. **Network layer** — Tailscale mesh (`unicorncommander.net`). Every device joins via Headscale pre-auth key. Already exists.
2. **Identity layer** — Keycloak SSO (already exists at `unicorncommander.ai`). Every device registers as a workload identity. Every operator (human or AI) authenticates against it. Token-bound to tailnet IP for replay protection.
3. **Tool layer** — UUDA's contribution. Typed MCP tools per device, policy engine in front, audit log behind.

## The agent (uc-agent)

**Language:** Rust. Justification: small static binary (~5–10 MB), no runtime deps, easy to ship via single-file install, memory-safe in long-running daemon role, mature MCP/protobuf libraries.

**Footprint target:** < 30 MB RAM idle, < 1% CPU idle, < 100 MB disk including state cache.

**Lifecycle:**
- Install: `curl ucinstall.sh | bash` — registers with Headscale, fetches Keycloak workload cert, joins tailnet, registers with uc-control.
- Run: systemd / launchd / Windows service.
- Update: agent polls control plane for version manifest, downloads + verifies signature + atomically swaps binary.
- Uninstall: clean tailnet leave + Keycloak revoke + remove files.

**Tool API (MCP):** see `TOOL_SURFACE.md` for the full list. Categories:
- **Read-only telemetry** — `system.health`, `system.metrics`, `system.logs`, `process.list`, `network.connections`
- **Package management** — `package.install`, `package.update`, `package.remove` (cross-platform: brew/apt/dnf/winget)
- **Service management** — `service.start/stop/restart/status` (systemd, launchd, Windows services)
- **Config management** — `config.apply(yaml)`, `config.diff(yaml)` (declarative, always dry-run first)
- **File operations** — `file.get`, `file.put`, `file.patch`, `file.delete`
- **App lifecycle** — `app.install`, `app.configure`, `app.uninstall` (high-level, recipe-based)
- **Escape hatch** — `exec.run(cmd, policy_class)` — gated, audited, rate-limited
- **Self** — `agent.version`, `agent.update`, `agent.logs`

## The control plane (uc-control)

Integrated into the existing Ops Center frontend/backend at `unicorncommander.ai`. Adds:

- **`/admin/devices`** — fleet inventory: every device, status, tags, last-seen, current vs declared state, drift indicators.
- **`/admin/devices/<id>/tools`** — interactive tool invocation (operator UI).
- **`/admin/devices/<id>/audit`** — full action log with replay.
- **`/admin/policies`** — policy engine UI: who can call what, blast radius limits, dry-run defaults.
- **`/admin/recipes`** — high-level apps composed of tool calls (e.g. "set up a Mac for a new dev" = brew install + dotfiles + ssh keys).
- **MCP gateway** — exposes the device tools as one big MCP surface, multiplexed by device ID. Brigade agents connect here.

## The policy engine

This is the load-bearing wall. Without it, the AI is a liability. Design:

- **Policy classes** per tool: `read`, `mutate-local`, `mutate-fleet`, `destructive`, `escape-hatch`.
- **RBAC:** maps actor (Keycloak group OR Brigade persona) → allowed policy classes per device tag.
- **Defaults:**
  - `read` → always allowed.
  - `mutate-local` → dry-run first, then apply.
  - `mutate-fleet` → canary one node, wait for green, prompt human for fan-out.
  - `destructive` → require human approval, recorded in audit.
  - `escape-hatch` → rate-limited, signed-off by senior operator, full session recording.
- **Blast radius cap:** N changes per minute per actor, M changes per hour fleet-wide.

## Drift detection

Each device reports its actual state every 30s. Control plane stores declared state (the "what should be true") and compares.

- Drift surfaces as **work to do**, not as alarms.
- Operator (or AI) sees "12 devices have drift, 3 are critical" — clicks one, sees diff, applies remediation as a tool call.
- Continuous reconciliation loop optional per device tag (default off — opt in for production tags).

## Network surface

Decision: each agent listens on its tailnet IP only at TCP `47000`, mTLS required, no reverse proxy on the device. Reasoning recorded here so future-us doesn't relitigate.

**Three layers of access control, in order of inspection:**

1. **Tailscale ACL.** Headscale enforces who-on-the-mesh can dial whom on which port. ops-center on `commander` (`tag:trusted`) is the only allowed source for `tcp:47000` against any UUDA-managed device. Updated 2026-04-26 to add `47000` to existing `tag:trusted → tag:admin` and `tag:trusted → tag:trusted` rules, plus a new `tag:trusted → tag:partner:47000` (the latter is what unblocks `gfl`). Backup of pre-change file lives on commander.
2. **Network bind.** Agent calls `tailscale ip --4` at startup and binds the listener only to that single tailnet address. The bind itself prevents anything off-tailnet from reaching the socket; nothing on `0.0.0.0` and nothing on the public IP. Belt-and-suspenders firewall rules (ufw / firewalld / Application Firewall on macOS) get added by `install.sh` on platforms where they're active.
3. **mTLS.** The TLS handshake demands a client cert chained to the UUDA root. Even if the network layer were compromised at one peer, an unauthorized caller can't open a session. Agent cert is unique per device and short-lived (90 d for Phase 1; Keycloak workload identities replace that in Phase 2).

**Why not a reverse proxy on the device.** Adding nginx/caddy/traefik on every box puts the agent in the same operational class as the things UUDA is meant to manage. Cert rotation, vhost config, and a second daemon to keep alive are exactly the kind of plumbing UUDA exists to make unnecessary. mTLS at a reverse proxy also breaks identity at the agent unless we forward client-cert headers, which is fragile and easy to misconfigure.

**Why not a reverse tunnel.** Reverse tunnels (agent dials out, holds a long-lived socket, control plane multiplexes RPCs back through it) exist because most RMM tools can't assume inbound reachability. Tailscale removes that constraint; layering a NAT-traversal workaround on top of a real mesh is the wrong direction. It also forces the control plane to maintain N long-lived sockets and invent stream multiplexing for heartbeat + RPC + log-streaming over each — complexity we don't need.

**Where a proxy does belong.** Traefik on `commander`, in front of ops-center, is correct and already exists. Brigade and operator browsers reach ops-center via Traefik on 443; ops-center then dials out over the tailnet directly to agents. Brigade-on-bigboy in particular must reach ops-center at the tailnet IP (or via Traefik on the tailnet, not via `unicorncommander.ai` through Cloudflare) so that long-lived SSE streams aren't capped at the 100s Cloudflare proxy timeout.

**Long-term.** Stay with this design. The only future case that would justify a reverse-tunnel mode is managing customer hardware that can't be on the tailnet at all (e.g. air-gapped or hostile-NAT consumer endpoints). Even then, the tunnel is an additive path, not a replacement for the direct-mesh path that defines the product.

## Identity & secrets

- Device cert: SPIFFE-shaped workload identity, issued by Keycloak, rotated weekly.
- Operator identity: Keycloak access token, group membership defines RBAC.
- Brigade agent identity: dedicated service account per persona (`maj-devops`, `the-general`, etc.), audit log captures both the agent persona AND the originating human request.
- Secrets in transit: mTLS over tailnet, both peer-authenticated.
- Secrets at rest on device: integrate with macOS Keychain / Linux `secret-service` / Windows DPAPI. No secrets in agent config files.

## Multi-tenancy

Day 1: single-tenant (Magic Unicorn fleet only).
Day 90: multi-tenant capable (Brightwater Labs as a separate tenant; future external customers).
Tenant isolation: separate Keycloak realm + separate control-plane DB schema + tenant-scoped MCP gateways.

## Open questions

- **Recipe DSL** — YAML + Jinja for Phase 2; revisit CUE in Phase 5 once specs have actually exercised real edge cases.
- **State store** — control plane Postgres (already in ops-center). Per-device fact store keyed by `device_id` for LtCol per-machine memory. Per-device state cache in agent is SQLite in Phase 4.
- **Network egress when offline** — agent buffers state changes locally, replays on reconnect. Bounded buffer (24h or 100 MB).
- **On-device LLM placement (Squad mode)** — Phase 2 hits the existing LiteLLM proxy on commander. Phase 5 adds local-LLM-on-device for the offline/sovereignty story (Strix Halo can run inference locally).
- **Authority on conflicts** — per-device `authority` column. `managed` (Brigade wins) is the default; `user-owned` (user wins) is for personal devices that opt in to UUDA help. MSP customer machines default to `managed`.
- **Prompt-version rollout** — Brigade canaries new system-prompt versions to one device, soaks, then fan-outs across the OS-family. Same shape as software canary.

## Non-goals (firm)

- Replacing Tailscale (we use it, don't compete with it).
- iOS / Android MDM. Jamf/Kandji own that, we don't enter.
- Compliance evidence collection (SOC 2, HIPAA tooling). Out of scope through Phase 5.
- Observability dashboards. UUDA exposes metrics; Grafana/Datadog/Bits AI visualize them.
- A new identity system. Keycloak handles login.
- Cloud-hosted-only / SaaS-first. Self-hostable from day 1; that's the wedge vs JumpCloud/Kandji and the foundation of the sovereignty pitch.
- Help-desk affordances: remote desktop, screen sharing, file transfer, end-user portals. Not what we are.

## Strategic positioning

UUDA's wedge isn't "RMM with AI bolted on." It's **mesh-native, AI-first-operator, self-hostable** for orgs that already chose Tailscale + Keycloak + Brigade and won't go cloud-only. That's a narrower target than "fleet management" as a category, but it's a target nobody else owns today.

The defensible moat over time isn't the agent or the tool surface — those will be commoditized. It's:
1. The Brigade integration (LtCol personas as named characters with continuity).
2. The Squad-mode UX (the same AI manages the box and serves the user).
3. The sovereignty positioning (open-core agent, self-host control plane, no required-cloud dependency).

Frame UC as "Windows for the AI era" only with the qualifier "self-hostable / sovereign." Without that qualifier the framing oversells against incumbents we'd rather not fight head-on.
