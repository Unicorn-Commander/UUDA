// SPDX-License-Identifier: Apache-2.0

//! Tool implementations exposed by uc-agent. Phase 1 ships five:
//! `system.health`, `system.metrics`, `system.logs`, `package.install`,
//! `service.status`. Per-OS backends sit behind small traits so adding
//! dnf/pacman/winget in Phase 2 is additive.
//!
//! The fleet-health probes (`disk.smart`, `zfs.status`, `update.status`,
//! `container.images`) live in [`smart`], [`zfs`], [`updates`] and
//! [`containers`]. They are strictly read-only observers, and every external
//! command they run is time-bounded by [`exec`] — a probe must never be able to
//! hang the agent or mutate the host it is measuring.

pub mod brief;
pub mod containers;
pub mod error;
pub mod exec;
pub mod files;
pub mod health;
pub mod logs;
pub mod messages;
pub mod metrics;
pub mod package;
pub mod service;
pub mod smart;
pub mod updates;
pub mod zfs;

pub use error::{ToolError, ToolResult};
