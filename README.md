# UUDA — Uniform Unicorn Device Agent

A mesh-native, AI-first device management agent. Runs on every device in your fleet, exposes a structured tool surface (MCP), lets human operators or AI agents (e.g. Brigade) manage the fleet through a unified control plane.

## What it is

- **`uc-agent`** — small static binary (Rust), runs on macOS / Linux / Windows. Two modes from one binary:
  - **Headless mode** for servers (`commander`, `magicunicorn`, `gfl`, etc.) — pure actuator, no on-device AI.
  - **Squad mode** for user workstations — pairs with Unicorn Squad's on-device LtCol persona; uc-agent is the briefing relay + typed-tool surface.
- **`uc-control`** — control plane, integrated into Ops Center at `unicorncommander.ai`. Device registry, audit, policy, MCP gateway.
- **`uc-cli`** — operator-facing CLI for humans (Phase 2+).
- Identity via Tailscale + Keycloak. Network via tailnet. Tools via MCP. Zero-trust at the transport/identity layer from day one; dynamic policy lands in Phase 3.

UUDA is the **actuator** that Brigade's officer model stands on. LtCols (Katie / Edge / McKenzy / Winston) and per-machine Colonels are Brigade personas; they call UUDA tools to actually do things on devices. UUDA itself never runs an LLM.

## Why

Existing MDM/RMM (Jamf, Kandji, NinjaOne, Tactical RMM) was built for humans clicking through dashboards. Their "AI" is a chatbot bolted on top. UUDA flips it: the AI is a first-class operator with a typed, audited tool surface; the dashboard is a window into what the AI is doing.

UUDA is **not** an MDM. It does not manage iOS/Android, doesn't compete with Jamf/Kandji on FileVault attestation or compliance evidence. It serves servers and workstations on a private mesh.

Magic Unicorn already owns the adjacent layers (Tailscale mesh, Brigade agents, Keycloak SSO, LiteLLM routing, Ops Center). UUDA is the missing link, the per-device runtime that lets Brigade's officer personas actually do things.

## Status

**2026-06-25 — v0.2.0. The full stack is built and PROVEN LIVE end to end.**
Phases 1 → 5 are implemented across the agent (`uc-agent`) and control plane
(`uc-control`, mounted into ops-center): registration + mTLS + the typed tool
surface, the **MCP gateway** (a CA-trusted client cert drives a stateless
`tools/call` to any device, plus bounded fan-out), the **policy engine** (per-tool
classes, dry-run-by-default, RBAC grants, an approval queue with separation of
duties), **multi-tenancy** (opt-in org fence for MSP "manage many fleets"), and
**drift detection** (declared desired-state → observe → governed remediation
through the same approval chokepoint). Brigade's `Maj. DevOps` persona drives it
all as the fleet officer.

A full-stack harness (`test/harness/smoke-full.sh`) stands up postgres + the real
control-plane routers + the real Rust agent and drives the **entire governed loop
against the live agent over HTTP + mTLS** — 18 asserted checks, green and
deterministic: discovery, read, the dry-run hardening, the policy chokepoint, the
approval workflow incl. separation of duties, effect verification, drift
detection, governed remediation, the closed loop, and the autonomy dial. The live
run also caught and fixed three real gateway bugs that unit tests had missed.

Everything is built, unit-tested (88 control-plane + 15 Brigade), proven E2E, and
pushed. **The only remaining work is the go-live** (deploy): mount the routers
into ops-center, apply SQL `001–005`, set `UUDA_GATEWAY_URL`, provision the fleet
officer's auth, and run certs + the daemon on real devices. See
`control-plane/ops-center-patch/README.md` for the runbook and `CHANGELOG.md` for
the full history.

**2026-04-25** — Project bootstrapped. Design docs in `docs/`. Decisions locked: name = UUDA. License = open-core (Apache 2.0 agent, proprietary control plane). Implementation tooling = Claude Code primary, Brigade `code-reviewer` for parallel review.

**2026-04-26** — Phase 1 in flight. Repo: `https://git.unicorncommander.ai/UnicornCommander/UUDA`. Workspace skeleton compiling, proto contracts complete, uc-types crate building clean. Architecture extended with two-mode (headless/Squad) design and LtCol officer model. Currently working: register/heartbeat, MCP library spike, the 5 Phase-1 tools.

## Layout

```
UUDA/
├── README.md                    ← you are here
├── docs/
│   ├── ARCHITECTURE.md          ← technical design
│   ├── TOOL_SURFACE.md          ← the 30 typed tools
│   ├── COMPETITIVE_LANDSCAPE.md ← who else is doing what
│   ├── ROADMAP.md               ← 4-6mo MVP, 9-12mo OSS release
│   └── AGENT_PROMPT.md          ← handoff to another agent
├── agent/                       ← uc-agent (Rust)
├── control-plane/               ← uc-control (integrates with ops-center)
└── proto/                       ← shared protobuf / OpenAPI definitions
```

## Owners

- **Aaron Stransky** — vision, product, infra
- **Shafen** — partner (Brightwater Labs / Magic Unicorn)
- TBD — implementation lead

## License

**Open core.**

- `agent/` and `proto/` — Apache 2.0. Source-visible, free for any use including commercial. Required for fleet adoption (security teams won't run closed-binary root daemons).
- `control-plane/` — proprietary, commercial license. The control plane is the value-capture layer (RBAC, policy, audit, fleet view, Brigade integration). Sold as SaaS at `unicorncommander.ai` or self-hosted with a license key.

If competitors clone the agent at scale later, fallback option is to switch agent to BSL/Polyform Shield (source-visible but restricted from competing managed services, time-bomb to Apache after 4 years). Don't preempt; ship Apache first.
