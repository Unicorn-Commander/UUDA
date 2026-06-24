# UUDA Tool Surface

The 30-ish typed MCP tools `uc-agent` exposes. Categories below; each category has a policy class.

## Conventions

- All tools take a `dry_run: bool` parameter. Default `true` for mutating tools.
- All mutating tools return a `change_set` object describing what would change / what changed.
- Errors are typed (not strings).
- Long-running tools return a `task_id` and the operator polls.

## Policy classes

| Class | Default policy | Examples |
|---|---|---|
| `read` | always allowed | `system.health`, `process.list` |
| `mutate-local` | dry-run first, audited | `package.install`, `service.restart` |
| `mutate-fleet` | canary one node, then fan out with approval | invoked at the recipe level, not tool level |
| `destructive` | requires explicit human approval | `package.remove`, `file.delete`, `service.stop` |
| `escape-hatch` | rate-limited, full session recorded | `exec.run` |

## The tools

### Telemetry (read)

- `system.health()` → uptime, load, mem, disk, temp if available
- `system.metrics(window: duration)` → time-series CPU/mem/disk/net
- `system.logs(service: str, since: timestamp, lines: int)` → journald / unified logging / event log entries
- `system.info()` → OS version, hostname, hardware summary
- `process.list(filter: str?)` → running processes
- `process.tree(pid: int)` → process tree from a root PID
- `network.connections(state: str?)` → active TCP/UDP connections
- `network.interfaces()` → NICs with IPs and link state
- `disk.usage(path: str?)` → mountpoints and usage
- `disk.io()` → per-device I/O stats

### Package management (mutate-local)

- `package.list()` → installed packages
- `package.search(query: str)` → searchable index
- `package.install(name: str, version: str?)` → installs via brew/apt/dnf/winget
- `package.update(name: str?)` → update one or all
- `package.remove(name: str)` → **destructive**

### Service management (mutate-local)

- `service.list()` → all units
- `service.status(name: str)` → enabled, active, failed, last logs
- `service.start(name: str)`
- `service.stop(name: str)` → **destructive**
- `service.restart(name: str)`
- `service.enable(name: str)` / `service.disable(name: str)`

### Config management (mutate-local)

- `config.apply(yaml: str)` → declarative apply with diff
- `config.diff(yaml: str)` → show what would change without applying
- `config.dump(scope: str)` → snapshot current relevant config

### File operations

- `file.get(path: str)` → read file (read)
- `file.put(path: str, content: bytes, mode: int?)` → write/overwrite (mutate-local)
- `file.patch(path: str, diff: str)` → apply unified diff (mutate-local)
- `file.delete(path: str)` → **destructive**
- `file.stat(path: str)` → metadata
- `file.find(root: str, pattern: str)` → readonly search

### App lifecycle (recipe-driven)

- `app.install(spec: AppSpec)` → high-level install (multiple tool calls under the hood)
- `app.configure(spec: AppSpec)` → reapply config
- `app.uninstall(name: str)` → **destructive**

### Escape hatch

- `exec.run(cmd: str, cwd: str?, timeout: duration?, policy_class: str)` → audited shell exec
  - `policy_class` must be declared by caller — engine enforces it matches actor's RBAC
  - Full stdin/stdout/stderr captured to audit log
  - Rate limited per actor

### Self

- `agent.version()` → version, build hash
- `agent.update(version: str?)` → atomic self-update
- `agent.logs(since: timestamp, lines: int)` → agent's own logs
- `agent.restart()` → re-exec with new config (mutate-local)

## Recipes (Phase 2+)

A recipe is a YAML doc that composes tool calls. Stored in control plane, versioned, dry-run-able as a unit.

```yaml
name: setup-new-mac
description: Bootstrap a fresh Mac for development
tools:
  - package.install: { name: brew, channel: stable }
  - app.install: { spec: { name: vscode, source: brew } }
  - app.install: { spec: { name: docker, source: brew-cask } }
  - file.put:
      path: ~/.config/git/config
      content: !ref dotfiles/git-config
```

Recipes are how `app.install` is implemented internally. Day 1: hand-written. Day 90: AI generates them.
