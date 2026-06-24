# UUDA Proto — Source of Truth

SPDX-License-Identifier: Apache-2.0

These `.proto` files define every wire type that crosses the
agent ↔ control-plane boundary. Both Rust (`agent/crates/uc-types`) and
Python (`control-plane/codegen`) generate from here. A round-trip contract
test in CI catches drift between languages.

## Why proto, not JSON Schema

Phase 1 transport is JSON-RPC over MCP, but proto gives us:
- one schema source consumed by two languages without hand-mirroring,
- stable wire compatibility rules (add fields, never renumber/repurpose),
- a path to gRPC later if we ever want non-JSON transport for, say, a
  high-frequency state stream.

JSON-on-the-wire is handled via `pbjson` on the Rust side and
`betterproto` (or hand-written Pydantic mirrors with the contract test)
on the Python side.

## Files

```
proto/
├── README.md              (this)
└── uc/v1/
    ├── errors.proto       Error envelope and ErrorCategory enum
    ├── agent.proto        Registration, heartbeat, identity, device facts
    └── tools.proto        The 5 Phase-1 tools: typed inputs and outputs
```

`uc.v1` is the package. Phase-2 wire-incompatible changes go in `uc.v2`,
not by mutating `v1`.

## Field-numbering rules

- Reserve numbers when removing fields: `reserved 5;`.
- Never re-purpose a number across versions of the same message.
- Add new fields at the end with the next available number.
- Boolean fields: name them positively (`enabled`, not `disabled`).

## Codegen

Rust: `agent/crates/uc-types/build.rs` runs `prost-build` + `pbjson-build`
during `cargo build`. No external `buf` dependency for Phase 1.

Python: `control-plane/codegen/regenerate.sh` runs `protoc` against these
files and emits Pydantic-compatible mirrors. Run by hand on schema change;
CI re-runs it and fails on uncommitted output.

## Style

- snake_case fields, PascalCase messages, SCREAMING_SNAKE_CASE enum values.
- Enum value 0 is always `*_UNSPECIFIED` so default-constructed messages
  are explicit about meaning nothing.
- Free-form `map<string, string> details` / `extra` for forward-compat
  metadata. Don't use it as a typed field substitute.
