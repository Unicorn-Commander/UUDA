// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{ToolError, ToolResult};
use crate::logs;

#[async_trait]
pub trait ServiceBackend: Send + Sync {
    async fn status(&self, name: &str) -> ToolResult<uc_types::ServiceStatusResponse>;
}

pub fn detect() -> Box<dyn ServiceBackend> {
    if cfg!(target_os = "linux") {
        return Box::new(SystemdBackend);
    }
    if cfg!(target_os = "macos") {
        return Box::new(LaunchdBackend);
    }
    Box::new(NoBackend)
}

pub async fn status(name: &str) -> ToolResult<uc_types::ServiceStatusResponse> {
    if name.is_empty() {
        return Err(ToolError::InvalidInput("service name is required".into()));
    }
    detect().status(name).await
}

pub struct SystemdBackend;

#[async_trait]
impl ServiceBackend for SystemdBackend {
    async fn status(&self, name: &str) -> ToolResult<uc_types::ServiceStatusResponse> {
        let props = systemctl_show(name).await?;

        let exists =
            !props.is_empty() && props.get("LoadState").map(|v| v.as_str()) != Some("not-found");

        let state = match props.get("ActiveState").map(String::as_str) {
            Some("active") => uc_types::ServiceState::Active,
            Some("inactive") => uc_types::ServiceState::Inactive,
            Some("failed") => uc_types::ServiceState::Failed,
            Some("activating") => uc_types::ServiceState::Activating,
            Some("deactivating") => uc_types::ServiceState::Deactivating,
            _ => uc_types::ServiceState::Unknown,
        };

        let enabled = props.get("UnitFileState").map(String::as_str) == Some("enabled");

        let pid = props
            .get("MainPID")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|p| *p != 0);

        let since = props.get("ActiveEnterTimestamp").and_then(|s| {
            if s.is_empty() || s == "n/a" {
                None
            } else {
                parse_systemd_timestamp(s)
            }
        });

        let recent_logs = if exists {
            logs::collect(name, None, 20)
                .await
                .map(|r| r.entries)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(uc_types::ServiceStatusResponse {
            name: name.to_string(),
            exists,
            state: state as i32,
            enabled,
            since,
            pid,
            recent_logs,
            error: None,
        })
    }
}

pub struct LaunchdBackend;

#[async_trait]
impl ServiceBackend for LaunchdBackend {
    #[cfg(target_os = "macos")]
    async fn status(&self, name: &str) -> ToolResult<uc_types::ServiceStatusResponse> {
        let target = format!("system/{name}");
        let out = Command::new("launchctl")
            .arg("print")
            .arg(&target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("launchctl: {e}")))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("Could not find service") || stderr.contains("not found") {
                return Ok(uc_types::ServiceStatusResponse {
                    name: name.to_string(),
                    exists: false,
                    state: uc_types::ServiceState::Unknown as i32,
                    enabled: false,
                    since: None,
                    pid: None,
                    recent_logs: Vec::new(),
                    error: None,
                });
            }
            return Err(ToolError::Backend(format!(
                "launchctl print {target} failed: {}",
                stderr.trim()
            )));
        }

        let body = String::from_utf8_lossy(&out.stdout).into_owned();
        let parsed = parse_launchctl_print(&body);

        let recent_logs = logs::collect(name, None, 20)
            .await
            .map(|r| r.entries)
            .unwrap_or_default();

        Ok(uc_types::ServiceStatusResponse {
            name: name.to_string(),
            exists: true,
            state: parsed.state as i32,
            enabled: parsed.enabled,
            since: None,
            pid: parsed.pid,
            recent_logs,
            error: None,
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn status(&self, _name: &str) -> ToolResult<uc_types::ServiceStatusResponse> {
        Err(ToolError::Backend(
            "launchd backend only available on macOS".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
struct LaunchctlParsed {
    state: uc_types::ServiceState,
    enabled: bool,
    pid: Option<u32>,
}

#[cfg(target_os = "macos")]
fn parse_launchctl_print(body: &str) -> LaunchctlParsed {
    let mut state = uc_types::ServiceState::Unknown;
    let mut enabled = false;
    let mut pid = None;
    let mut last_exit_code: Option<i32> = None;

    for raw in body.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("state = ") {
            state = match rest.trim() {
                "running" => uc_types::ServiceState::Active,
                "waiting" => uc_types::ServiceState::Inactive,
                "exited" => uc_types::ServiceState::Inactive,
                "spawning" => uc_types::ServiceState::Activating,
                _ => uc_types::ServiceState::Unknown,
            };
        } else if let Some(rest) = line.strip_prefix("pid = ") {
            pid = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix("last exit code = ") {
            last_exit_code = rest.trim().parse::<i32>().ok();
        } else if let Some(rest) = line.strip_prefix("active count = ") {
            if rest.trim().parse::<u32>().unwrap_or(0) > 0 {
                enabled = true;
            }
        } else if line.starts_with("path = ") {
            enabled = true;
        }
    }

    if matches!(state, uc_types::ServiceState::Inactive) && last_exit_code.unwrap_or(0) != 0 {
        state = uc_types::ServiceState::Failed;
    }

    LaunchctlParsed {
        state,
        enabled,
        pid,
    }
}

pub struct NoBackend;

#[async_trait]
impl ServiceBackend for NoBackend {
    async fn status(&self, _name: &str) -> ToolResult<uc_types::ServiceStatusResponse> {
        Err(ToolError::Backend(
            "no service backend available on this platform".into(),
        ))
    }
}

async fn systemctl_show(name: &str) -> ToolResult<HashMap<String, String>> {
    let out = Command::new("systemctl")
        .args(["show", name, "--no-pager"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ToolError::Backend(format!("systemctl: {e}")))?;
    if !out.status.success() {
        return Ok(HashMap::new());
    }
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

fn parse_systemd_timestamp(raw: &str) -> Option<pbjson_types::Timestamp> {
    let format = time::format_description::parse(
        "[weekday repr:short] [year]-[month]-[day] [hour]:[minute]:[second] [period case:upper]",
    )
    .ok()?;
    let local = time::PrimitiveDateTime::parse(raw, &format).ok()?;
    let utc = local.assume_utc();
    Some(pbjson_types::Timestamp {
        seconds: utc.unix_timestamp(),
        nanos: 0,
    })
}
