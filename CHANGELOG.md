# Changelog

All notable changes to UUDA are recorded here. Versions track the `uc-agent`
workspace (`agent/Cargo.toml`); the control plane (`control-plane/`) ships with
the agent it was proven against. Format loosely follows Keep a Changelog.

## [Unreleased]

Went from zero deployed hosts to a fleet-wide deploy, and the first contact with
real hardware immediately paid for the probes.

### Added — agent (`agent/`)
- **Fleet-health probes** (`disk.smart`, `zfs.status`, `update.status`,
  `container.images`): four read-only observers, each modelled on a failure that
  was invisible across the fleet this week. `disk.smart` computes status from RAW
  attribute values only and carries `smartctl -H`'s verdict through as
  `smart_overall_passed` without ever letting it be an input. `zfs.status` reports
  SUSPENDED as its own boolean. `update.status` identifies the dpkg frontend lock
  holder via `/proc/locks` and reports how long it has been held. `container.images`
  flags unpinnable refs, crash loops and stale images, and contacts no registry
  (`registry_checked: false`).
- **Time-bounded exec** (`uc-toolkit/src/exec.rs`): every external command runs
  under a timeout (default 20s, cap 300s) with `kill_on_drop` and stdin from
  `/dev/null`. A probe that inherits a hang takes the agent with it.
- **`files.*` tool family**: on-device SQLite FTS5 index plus `files.search` /
  `files.index` / `files.status`, so Center Deep can federate keyword file search
  across the fleet. Off by default (`UC_FILES_ENABLED`); the FTS5 query sanitizer
  turns malformed input into an empty result instead of an error.

### Added — control plane (`control-plane/ops-center-patch/`)
- **Drift scheduler** (`uuda_scheduler.py`): the periodic scan loop the drift
  engine never had. A plain asyncio task in the existing app (no APScheduler, no
  Celery), scanning live devices only with bounded concurrency, a per-device
  timeout, a per-cycle device cap and an overlap guard. **Read-only by design**:
  it never runs a check's `remediation_tool` even when policy would allow it.
  Defaults OFF (`UUDA_DRIFT_SCAN_ENABLED=false`).
- **Alerting** (`uuda_alerts.py`): edge-triggered transitions, so drift lasting
  five cycles is one alert and recovery is one notice; per-key cooldown and a
  fleet-wide rate cap. Log sink always on, webhook and SMTP sinks opt-in, every
  sink timeout-bounded and failing soft. State primed from history at startup so a
  restart does not re-announce known drift.
- **Drift history** (`sql/006_uuda_drift_history.sql`, `/drift/history`,
  `/drift/summary`, `/drift/scheduler`): one observation row per check per device
  per cycle, so "this disk got worse over three weeks" is visible instead of only
  "it's bad now". Pruned on a retention window (default 90 days).
- Control-plane tests 88 → 154.

### Added — tools (`tools/`)
- `fleet-health-check.sh` vendored into the repo. It is the agent-independent
  fallback that runs on all 14 hosts, including the two that cannot take an
  agent, and it had been living only in a home directory.
- `uuda-liveness-watchdog.sh`: the drift scheduler only scans devices that
  heartbeat within 90s, so a dead host leaves the fleet quietly and every check
  goes green. This is the absence-of-signal alarm.
- `pg-backup-unicorn.sh`: replaces a nightly backup that had been failing
  silently since its script was renamed, leaving 14 production databases
  unprotected.

### Added — release engineering
- **CI** (`.forgejo/workflows/ci.yml`): the first CI in the repo. Rust job (fmt,
  clippy, `cargo test --workspace`) and Python job (pytest, deps pinned exactly),
  both on `ubuntu-latest`, the only label the runner advertises. Installs protoc,
  a C toolchain and rustup itself, since the runner image guarantees none of them.
  `cargo fmt --check` reports without gating until the tree is formatted once.
- **`.deb` packaging** (`agent/packaging/build-deb.sh`): amd64 and arm64 packages
  from an already cross-compiled binary, same layout as `install.sh`. `Depends` is
  derived from the binary's own GLIBC references; `smartmontools` is a Recommends.
  The package deliberately does not start the service, since registration needs a
  bootstrap token that only the operator can supply.
- **macOS signing** (`agent/packaging/sign-macos.sh`): codesign with Developer ID
  under the hardened runtime, `notarytool submit --wait`, `stapler staple`, then
  `codesign --verify --strict` and `spctl --assess`. Identity and notary
  credentials come from the environment; with nothing configured it fails in
  preflight and changes nothing.

### Fixed
- **SMART probe was blind on exactly the hosts that needed it.** Where SATA disks
  hang off a SAS HBA or expander, `smartctl --scan -j` reports `type=scsi`;
  passing that through made smartctl read SCSI mode pages and every raw counter
  came back null. All four of rackboy1's SSDs and bigboy's pool disks are
  expander-attached. Auto-detect first, fall back to the scanned type. The design
  held: those disks reported UNKNOWN rather than a false OK.
- **Power-on hours in the quadrillions.** Seagate packs minutes and seconds into
  the high bits of the Power_On_Hours raw integer. For attribute 9 only, read the
  leading integer of `raw.string`; everything else still trusts `raw.value`.
- **Every UUDA admin route returned 503 on the first real deploy.** ops-center
  imports these routers at module scope, so `from server import require_admin`
  ran while `server` was half-initialized, raised ImportError, and each router
  permanently substituted a fail-closed dependency. New `uuda_auth.py` resolves
  the host app's dependency per request and adapts to its real signature, still
  failing closed if the symbol never appears.
- **Every seeded drift check errored across all 9 devices** (90 ERROR, 0
  observations): the scanner refuses any probe not classified READ and unknown
  tools default to MUTATING. The fail-safe was right, the policy table was just
  missing the new tools. `files.index` stays unlisted, since it writes.
- **`container.images` counted lifetime restart churn as an active restart
  loop.** Docker's `RestartCount` is cumulative and never decreases, so a
  container that flapped once in April counted forever and a drift check built
  on it could fire permanently without ever clearing. bigboy reported 6 while
  docker reported zero containers restarting. Split into `high_restart_count`
  (history) and `restarting_now` (`State == restarting`).
- **`build.rs` baked a build-time path into the cached build-script binary.**
  `env!("CARGO_MANIFEST_DIR")` resolves when the build script is compiled, so a
  binary cached from a deleted git worktree kept pointing at a directory that no
  longer existed and every later cross-build failed with "Could not make proto
  path relative". Read the variable at runtime instead.
- **`dpkg_lock_held_secs` is 0, not null, when the lock is free.** A drift check
  reading `dpkg_lock_held_secs <= 1800` ERRORed on every healthy host. Null now
  means only "held, age unknown".
- `install.sh`'s `DEFAULT_BASE_URL` pointed at `commander.unicorncommander.net`,
  which NXDOMAINs; commander's MagicDNS name is `srv981531.unicorncommander.net`.
  Every unattended install taking the default would have failed to register.
- The smoke gates hardcoded `agent/target/`, so a global `CARGO_TARGET_DIR`
  (the dev Mac's configuration) made the repo's declared merge gate unrunnable
  there. They ask cargo now.
- `StartLimitIntervalSec` sat in `[Service]` in the systemd unit, where systemd
  ignores it with a log line on every start. Moved to `[Unit]` with
  `StartLimitBurst`, so `Restart=on-failure` actually has a burst limit.
- Patch README documented a database (`uchub-postgres -U opscenter`) whose role
  does not exist. Real target is `unicorn-postgresql` / `unicorn_db` / `unicorn`.

### Docs
- `CLAUDE.md` and `STATUS.md` landed in the repo, corrected against verified
  state rather than the April snapshot. `CLAUDE.md`'s target-population rule had
  excluded the ops-center hosts, contradicting both plan docs; it was the outlier.

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
