# Security

## Reporting

Email **security@magicunicorn.tech** with:

- the version (`uc-agent` prints it from the crate version, currently 0.2.0 in `agent/Cargo.toml`)
- the affected path (`agent/`, `proto/`, the installer, or the commercial control plane)
- what you can do, and what you already tried
- whether you want to be credited

Please give the project time to fix the issue before you publish it. A public GitHub issue or pull request is the wrong place for an unfixed vulnerability.

There is no bug-bounty program described in this repository.

## What the agent enforces

These are properties of the code in `agent/`, not a certification.

- The listener binds to the machine's Tailscale IPv4 address, unless `UC_AGENT_LISTEN_IP` is set. `install.sh` refuses to run until `tailscale ip --4` succeeds.
- The TLS listener requires a client certificate chained to the UUDA CA. `GET /healthz`, `GET /version`, and `POST /mcp/v1` share that requirement. Rustls with the ring provider is installed at startup.
- The device generates its own key, sends a CSR at registration, and stores `cert.pem`, `key.pem`, and `ca-chain.pem` under the configured cert directory. The heartbeat uses a separate agent key in the `X-Agent-Key` header.
- `package.install` and `messages.send` default to `dry_run: true` on the device. A boolean `false` applies the change.
- Fleet probes (`disk.smart`, `zfs.status`, `update.status`, `container.images`) are read-only, and every external command they run has a deadline (default 20 seconds, cap 300). `zfs.status` does not run scrub, clear, or pool import/export.
- The file index and the macOS Messages bridge are off unless the config turns them on.
- The daemon does not call a model. A brief is a JSON file.

A caller that already has a client certificate minted for this CA can invoke tools directly on the device. That call does not pass through the approval queue.

## What the control plane adds

The commercial control plane that ships with Ops-Center is the policy chokepoint:

- tool classes `read`, `mutating`, `destructive`, and `escape_hatch`
- a missing `dry_run` on the gateway counts as a real change for `package.install` and `messages.send`
- real mutations are queued until a different person approves them, unless a scoped `auto_approve` grant matches
- the requester cannot approve their own pending action
- a fully wildcard elevating grant is ignored
- unknown tool names are treated as mutating; names that look like a shell, a delete, or a shutdown are treated as an escape hatch and denied unless a grant allows it
- fan-out is capped at 250 devices
- org fencing (`UUDA_TENANCY_ENFORCED`) and the periodic drift scan (`UUDA_DRIFT_SCAN_ENABLED`) are off by default. The scan does not run remediation tools

Ops-Center performs the admin check on those routes. This repository does not implement that login.

## Design notes

`docs/SECURITY_POSTURE.md` is an earlier write-up (2026-04-26). Where it disagrees with the agent or with the policy behaviour above, the code wins. That document also names private hosts. Do not treat it as the current model, and do not copy those names into issues.
