// SPDX-License-Identifier: Apache-2.0

use std::net::Ipv4Addr;
use std::process::Command;

use crate::error::{AgentError, Result};

pub fn local_ipv4() -> Result<Ipv4Addr> {
    if let Ok(raw) = std::env::var("UC_AGENT_LISTEN_IP") {
        return raw.trim().parse::<Ipv4Addr>().map_err(|e| {
            AgentError::Tailscale(format!("UC_AGENT_LISTEN_IP not parseable {raw:?}: {e}"))
        });
    }
    let output = Command::new("tailscale")
        .args(["ip", "--4"])
        .output()
        .map_err(|e| AgentError::Tailscale(format!("could not invoke tailscale: {e}")))?;
    if !output.status.success() {
        return Err(AgentError::Tailscale(format!(
            "tailscale ip --4 exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let line = raw
        .lines()
        .next()
        .ok_or_else(|| AgentError::Tailscale("tailscale ip --4 returned no output".into()))?;
    line.trim()
        .parse::<Ipv4Addr>()
        .map_err(|e| AgentError::Tailscale(format!("could not parse tailnet ip {line:?}: {e}")))
}

pub fn hostname() -> Result<String> {
    if let Ok(name) = std::env::var("UC_AGENT_HOSTNAME") {
        if !name.trim().is_empty() {
            return Ok(name.trim().to_string());
        }
    }
    let output = Command::new("hostname")
        .output()
        .map_err(|e| AgentError::Tailscale(format!("could not invoke hostname: {e}")))?;
    if !output.status.success() {
        return Err(AgentError::Tailscale(format!(
            "hostname exited {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
