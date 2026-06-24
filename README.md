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
