# Changelog

All notable changes to UUDA are recorded here. Versions track the `uc-agent`
workspace (`agent/Cargo.toml`); the control plane (`control-plane/`) ships with
the agent it was proven against. Format loosely follows Keep a Changelog.

## [0.2.0] — 2026-06-25

The full agentic fleet-management plane — built, governed, and **proven live end
to end** against the real agent.

### Added — control plane (`control-plane/ops-center-patch/`)
- **MCP gateway** (`uuda_gateway.py`, `uuda_mcp.py`): mints a CA-trusted client
  cert and drives a stateless `tools/call` to any registered device over mTLS;
  per-device tool discovery and bounded, failure-isolated fan-out. Routes under
  `/api/v1/uuda/mcp`.
- **Policy engine** (`uuda_policy.py`): per-tool policy classes
  (read / mutating / destructive / escape_hatch), dry-run-by-default for mutating
  tools, RBAC grants (`uuda_policy_rules`), and an approval queue
  (`uuda_pending_actions`) with atomic claim + separation of duties (the
  requester cannot approve their own action).
- **Multi-tenancy** (`uuda_tenancy.py`): opt-in org fence
  (`UUDA_TENANCY_ENFORCED`) for MSP "manage many fleets"; out-of-org devices are
  invisible (404).
- **Drift detection** (`uuda_drift.py`, `uuda_drift_api.py`): declared
  desired-state checks, a read-only scanner, and governed remediation that routes
  every fix through the same policy + approval chokepoint (a fix is never a
  backdoor). Routes under `/api/v1/uuda/drift`.
- SQL migrations `003`–`005` (policy, tenancy, drift), all additive.
- HTTP integration backstop + a full-stack E2E harness
  (`test/harness/smoke-full.sh` + `e2e_probe.py`) that drives the whole governed
  loop against the live agent — 18 asserted checks, deterministic.

### Added — agent (`agent/`)
- Stateless MCP serving (`server.rs`): `with_stateful_mode(false)` +
  `with_json_response(true)`, so a bare `tools/call` works without an
  initialize/session handshake (the shape every federation client POSTs).

### Added — Brigade (separate repo)
- `Maj. DevOps` fleet officer: 7 thin governed-gateway tools; it surfaces held
  mutations for a human to approve and never self-approves.

### Fixed
- Gateway could not dial any device: `tailnet_ip::text` rendered a host inet
  **with** its mask (`172.22.0.4/32`), poisoning the dial URL. Now selects
  `host(tailnet_ip)`, and `_device_url` strips any `/mask` and brackets IPv6.
- Tool discovery returned null: `_tool_payload` only handled a tools/`call`
  result; a tools/`list` `{tools:[…]}` has no content block. Now returns the whole
  result when there is no MCP content block.
- (harness) the agent advertised `0.0.0.0` — a valid bind address but not a
  routable dial target; it now advertises a reachable address.

### Security
- Adversarial red-team passes over the gateway / policy / tenancy / drift layers
  fixed multiple authorization bypasses (dry-run truthiness, self-approval, a
  TOCTOU double-execute, cross-tenant reads, desired-state check-key hijack, and
  availability DoS via hostile device payloads).

### Not yet shipped (go-live, deploy-gated)
- Mounting the routers into the live ops-center + applying SQL `001`–`005`.
- Certs + the `uc-agent` daemon on real devices.
- Operator UIs (fleet / drift / approvals dashboards) and the Phase-5
  productization items in `docs/ROADMAP.md` (installer, website, first pilot).

## [0.1.0] — 2026-04-26

Phase 1 — walking skeleton.

### Added
- `uc-agent` (Rust): registration (CSR signed by a control-plane-issued root CA),
  heartbeat, an mTLS MCP listener, and the typed tool surface
  (`system.health/metrics/logs`, `service.status`, `package.install`,
  `agent.brief/report`, plus a macOS Messages bridge).
- `uc-control` Phase-1 routers: register / heartbeat / devices / tokens; root CA
  in `uuda_root_ca`. SQL `001`–`002`.
- Proven live over the wire on the dev mesh.
