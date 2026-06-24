// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub agent: AgentSection,
    pub control_plane: ControlPlaneSection,
    pub paths: PathsSection,
    #[serde(default)]
    pub bootstrap: BootstrapSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentSection {
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    #[serde(default = "default_register_timeout", with = "humantime_serde")]
    pub register_timeout: Duration,
    /// Whether this device runs an on-device LtCol agent (Squad mode).
    /// Headless servers leave this false and `agent.report` returns
    /// `has_local_agent=false`; `agent.brief` still persists the brief
    /// to disk, but no LLM picks it up.
    #[serde(default)]
    pub has_local_agent: bool,

    /// Enable the on-device Messages bridge (messages.* tools). Off by default:
    /// reading chat.db and sending via AppleScript require macOS Full Disk
    /// Access + Automation consent, so this is opt-in per device.
    #[serde(default)]
    pub messages_enabled: bool,

    /// Override for the Messages database path. Defaults to the invoking user's
    /// ~/Library/Messages/chat.db; a root daemon should point this at the
    /// target user's home.
    #[serde(default)]
    pub messages_db_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ControlPlaneSection {
    pub base_url: String,
    #[serde(default = "default_tls_skip_verify")]
    pub tls_skip_verify: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PathsSection {
    pub state_dir: PathBuf,
    pub cert_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BootstrapSection {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub owner: String,
}

fn default_mcp_port() -> u16 {
    uc_types::DEFAULT_MCP_PORT
}

fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(uc_types::HEARTBEAT_INTERVAL_SECS)
}

fn default_register_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_tls_skip_verify() -> bool {
    false
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            AgentError::Config(format!("cannot read {}: {e}", path.display()))
        })?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.control_plane.base_url.is_empty() {
            return Err(AgentError::Config(
                "control_plane.base_url is required".into(),
            ));
        }
        if !self.control_plane.base_url.starts_with("http") {
            return Err(AgentError::Config(format!(
                "control_plane.base_url must be http(s) url, got {}",
                self.control_plane.base_url
            )));
        }
        if self.paths.state_dir.as_os_str().is_empty() {
            return Err(AgentError::Config("paths.state_dir is required".into()));
        }
        if self.paths.cert_dir.as_os_str().is_empty() {
            return Err(AgentError::Config("paths.cert_dir is required".into()));
        }
        Ok(())
    }

    pub fn register_url(&self) -> String {
        format!("{}/api/v1/uuda/register", self.control_plane.base_url.trim_end_matches('/'))
    }

    pub fn heartbeat_url(&self) -> String {
        format!("{}/api/v1/uuda/heartbeat", self.control_plane.base_url.trim_end_matches('/'))
    }

    pub fn cert_path(&self) -> PathBuf {
        self.paths.cert_dir.join("cert.pem")
    }

    pub fn key_path(&self) -> PathBuf {
        self.paths.cert_dir.join("key.pem")
    }

    pub fn ca_chain_path(&self) -> PathBuf {
        self.paths.cert_dir.join("ca-chain.pem")
    }

    pub fn registration_path(&self) -> PathBuf {
        self.paths.state_dir.join("registration.toml")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub device_id: String,
    pub agent_key: String,
    pub registered_at: String,
}

impl Registration {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(Some(toml::from_str(&raw)?))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, raw)?;
        Ok(())
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/uc-agent/config.toml";
