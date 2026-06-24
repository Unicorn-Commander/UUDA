// SPDX-License-Identifier: Apache-2.0

//! Generated wire types for UUDA. Source: `../../proto/uc/v1/*.proto`.
//! Both prost and pbjson outputs are committed into `src/generated/`
//! at build time; never edit them by hand.

#![allow(clippy::all)]
#![allow(rustdoc::all)]

pub mod uc {
    pub mod v1 {
        include!("generated/uc.v1.rs");
        include!("generated/uc.v1.serde.rs");
    }
}

pub use uc::v1::*;

pub const PROTO_PACKAGE: &str = "uc.v1";
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default tailnet-bound MCP port. Reserved 47000-47009 for the UC daemon
/// family. Documented rationale lives in `docs/PHASE1_PLAN.md` §3.
pub const DEFAULT_MCP_PORT: u16 = 47000;

/// Default heartbeat cadence to ops-center.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Hard cap on metrics series per response, regardless of `max_series`.
pub const MAX_SERIES_HARD: u32 = 500;

/// Default series cap when the request leaves `max_series` zero.
pub const MAX_SERIES_DEFAULT: u32 = 100;

/// Hard cap on log lines per `system.logs` response.
pub const MAX_LOG_LINES_HARD: u32 = 10_000;
