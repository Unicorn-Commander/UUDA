// SPDX-License-Identifier: Apache-2.0

//! Tool implementations exposed by uc-agent. Phase 1 ships five:
//! `system.health`, `system.metrics`, `system.logs`, `package.install`,
//! `service.status`. Per-OS backends sit behind small traits so adding
//! dnf/pacman/winget in Phase 2 is additive.

pub mod brief;
pub mod error;
pub mod files;
pub mod health;
pub mod logs;
pub mod messages;
pub mod metrics;
pub mod package;
pub mod service;

pub use error::{ToolError, ToolResult};
