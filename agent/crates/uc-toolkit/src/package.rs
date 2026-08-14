// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::info;

use crate::error::{ToolError, ToolResult};

pub struct InstallPlan {
    pub planned_changes: Vec<uc_types::Change>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

pub struct InstallApplied {
    pub applied_changes: Vec<uc_types::Change>,
    pub installed_version: Option<String>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

#[async_trait]
pub trait PackageBackend: Send + Sync {
    fn kind(&self) -> uc_types::PackageBackendKind;

    async fn current_version(&self, name: &str) -> ToolResult<Option<String>>;

    async fn plan_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallPlan>;

    async fn apply_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallApplied>;
}

pub fn detect() -> Box<dyn PackageBackend> {
    if cfg!(target_os = "macos") {
        if Path::new("/opt/homebrew/bin/brew").exists() || Path::new("/usr/local/bin/brew").exists()
        {
            return Box::new(BrewBackend);
        }
    }
    if cfg!(target_os = "linux") && Path::new("/usr/bin/apt-get").exists() {
        return Box::new(AptBackend);
    }
    Box::new(NoBackend)
}

pub fn from_kind(kind: uc_types::PackageBackendKind) -> Box<dyn PackageBackend> {
    match kind {
        uc_types::PackageBackendKind::Auto => detect(),
        uc_types::PackageBackendKind::Apt => Box::new(AptBackend),
        uc_types::PackageBackendKind::Brew => Box::new(BrewBackend),
    }
}

pub async fn install(
    request: uc_types::PackageInstallRequest,
) -> ToolResult<uc_types::PackageInstallResponse> {
    if request.name.is_empty() {
        return Err(ToolError::InvalidInput("package name is required".into()));
    }

    let dry_run = request.meta.as_ref().map(|m| m.dry_run).unwrap_or(true);

    let backend = match uc_types::PackageBackendKind::try_from(request.backend) {
        Ok(k) => from_kind(k),
        Err(_) => detect(),
    };

    info!(
        name = %request.name,
        version = ?request.version,
        backend = ?backend.kind(),
        dry_run,
        "package.install"
    );

    if dry_run {
        let plan = backend
            .plan_install(&request.name, request.version.as_deref())
            .await?;
        Ok(uc_types::PackageInstallResponse {
            change_set: Some(uc_types::ChangeSet {
                changes: plan.planned_changes,
                applied: false,
            }),
            backend_used: backend.kind() as i32,
            installed_version: None,
            stdout_tail: plan.stdout_tail,
            stderr_tail: plan.stderr_tail,
            error: None,
        })
    } else {
        let applied = backend
            .apply_install(&request.name, request.version.as_deref())
            .await?;
        Ok(uc_types::PackageInstallResponse {
            change_set: Some(uc_types::ChangeSet {
                changes: applied.applied_changes,
                applied: true,
            }),
            backend_used: backend.kind() as i32,
            installed_version: applied.installed_version,
            stdout_tail: applied.stdout_tail,
            stderr_tail: applied.stderr_tail,
            error: None,
        })
    }
}

pub struct AptBackend;

#[async_trait]
impl PackageBackend for AptBackend {
    fn kind(&self) -> uc_types::PackageBackendKind {
        uc_types::PackageBackendKind::Apt
    }

    async fn current_version(&self, name: &str) -> ToolResult<Option<String>> {
        let out = Command::new("dpkg-query")
            .args(["-W", "-f=${Version}", name])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("dpkg-query: {e}")))?;
        if !out.status.success() {
            return Ok(None);
        }
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if v.is_empty() {
            Ok(None)
        } else {
            Ok(Some(v))
        }
    }

    async fn plan_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallPlan> {
        let target = qualified_name(name, version);
        let out = Command::new("apt-get")
            .args(["-s", "install", "-y", &target])
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("apt-get: {e}")))?;
        if !out.status.success() {
            return Err(ToolError::Backend(format!(
                "apt-get -s install {target} failed: {}",
                tail(&String::from_utf8_lossy(&out.stderr), 1024)
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let changes = parse_apt_simulation(&stdout);
        Ok(InstallPlan {
            planned_changes: changes,
            stdout_tail: tail(&stdout, 4096),
            stderr_tail: tail(&stderr, 4096),
        })
    }

    async fn apply_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallApplied> {
        let target = qualified_name(name, version);
        let out = Command::new("apt-get")
            .args(["install", "-y", &target])
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("apt-get: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(ToolError::Backend(format!(
                "apt-get install {target} failed: {}",
                tail(&stderr, 1024)
            )));
        }
        let installed = self.current_version(name).await.ok().flatten();
        let changes = vec![uc_types::Change {
            kind: "package.install".into(),
            target: name.to_string(),
            before: "absent".into(),
            after: installed.clone().unwrap_or_else(|| "installed".into()),
        }];
        Ok(InstallApplied {
            applied_changes: changes,
            installed_version: installed,
            stdout_tail: tail(&stdout, 4096),
            stderr_tail: tail(&stderr, 4096),
        })
    }
}

pub struct BrewBackend;

#[cfg(target_os = "macos")]
fn brew_path() -> Option<&'static str> {
    if Path::new("/opt/homebrew/bin/brew").exists() {
        Some("/opt/homebrew/bin/brew")
    } else if Path::new("/usr/local/bin/brew").exists() {
        Some("/usr/local/bin/brew")
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn brew_owner_username() -> ToolResult<String> {
    use std::os::unix::fs::MetadataExt;
    let path = brew_path().ok_or_else(|| ToolError::Backend("brew not found in PATH".into()))?;
    let meta =
        std::fs::metadata(path).map_err(|e| ToolError::Backend(format!("stat {path}: {e}")))?;
    let uid = nix::unistd::Uid::from_raw(meta.uid());
    let user = nix::unistd::User::from_uid(uid)
        .map_err(|e| ToolError::Backend(format!("getpwuid: {e}")))?
        .ok_or_else(|| ToolError::Backend(format!("no passwd entry for uid {}", uid.as_raw())))?;
    Ok(user.name)
}

#[cfg(target_os = "macos")]
async fn brew_command(brew_user: &str) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n").arg("-u").arg(brew_user).arg("-H");
    cmd.arg(brew_path().expect("brew path validated upstream"));
    cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    cmd.env("HOMEBREW_NO_INSTALL_CLEANUP", "1");
    cmd.env("HOMEBREW_NO_ANALYTICS", "1");
    cmd
}

#[async_trait]
impl PackageBackend for BrewBackend {
    fn kind(&self) -> uc_types::PackageBackendKind {
        uc_types::PackageBackendKind::Brew
    }

    #[cfg(target_os = "macos")]
    async fn current_version(&self, name: &str) -> ToolResult<Option<String>> {
        let user = brew_owner_username()?;
        let mut cmd = brew_command(&user).await;
        cmd.arg("list").arg("--versions").arg(name);
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("brew list: {e}")))?;
        if !out.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let version = line
            .split_whitespace()
            .nth(1)
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty());
        Ok(version)
    }

    #[cfg(target_os = "macos")]
    async fn plan_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallPlan> {
        let user = brew_owner_username()?;
        if let Some(existing) = self.current_version(name).await? {
            return Ok(InstallPlan {
                planned_changes: vec![],
                stdout_tail: format!("{name} already installed at {existing}\n"),
                stderr_tail: String::new(),
            });
        }
        let mut cmd = brew_command(&user).await;
        cmd.arg("info").arg(name);
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("brew info: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(ToolError::Backend(format!(
                "brew info {name} failed: {}",
                tail(&stderr, 1024)
            )));
        }
        let target_version = parse_brew_info_version(&stdout).unwrap_or_else(|| "latest".into());
        let _ = version;
        Ok(InstallPlan {
            planned_changes: vec![uc_types::Change {
                kind: "package.install".into(),
                target: name.to_string(),
                before: "absent".into(),
                after: target_version,
            }],
            stdout_tail: tail(&stdout, 4096),
            stderr_tail: tail(&stderr, 4096),
        })
    }

    #[cfg(target_os = "macos")]
    async fn apply_install(&self, name: &str, version: Option<&str>) -> ToolResult<InstallApplied> {
        let user = brew_owner_username()?;
        let mut cmd = brew_command(&user).await;
        cmd.arg("install").arg(name);
        let _ = version;
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("brew install: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(ToolError::Backend(format!(
                "brew install {name} failed: {}",
                tail(&stderr, 1024)
            )));
        }
        let installed = self.current_version(name).await.ok().flatten();
        Ok(InstallApplied {
            applied_changes: vec![uc_types::Change {
                kind: "package.install".into(),
                target: name.to_string(),
                before: "absent".into(),
                after: installed.clone().unwrap_or_else(|| "installed".into()),
            }],
            installed_version: installed,
            stdout_tail: tail(&stdout, 4096),
            stderr_tail: tail(&stderr, 4096),
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn current_version(&self, _name: &str) -> ToolResult<Option<String>> {
        Err(ToolError::Backend(
            "brew backend only available on macOS".into(),
        ))
    }

    #[cfg(not(target_os = "macos"))]
    async fn plan_install(&self, _name: &str, _version: Option<&str>) -> ToolResult<InstallPlan> {
        Err(ToolError::Backend(
            "brew backend only available on macOS".into(),
        ))
    }

    #[cfg(not(target_os = "macos"))]
    async fn apply_install(
        &self,
        _name: &str,
        _version: Option<&str>,
    ) -> ToolResult<InstallApplied> {
        Err(ToolError::Backend(
            "brew backend only available on macOS".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
fn parse_brew_info_version(stdout: &str) -> Option<String> {
    // brew info first line is like:  "wget: stable 1.21.4 (bottled)"
    let first = stdout.lines().next()?;
    let rest = first.split_once(':')?.1.trim();
    let mut parts = rest.split_whitespace();
    let _channel = parts.next()?;
    let v = parts.next()?;
    Some(v.to_string())
}

pub struct NoBackend;

#[async_trait]
impl PackageBackend for NoBackend {
    fn kind(&self) -> uc_types::PackageBackendKind {
        uc_types::PackageBackendKind::Auto
    }

    async fn current_version(&self, _: &str) -> ToolResult<Option<String>> {
        Err(ToolError::Backend(
            "no package backend available on this platform".into(),
        ))
    }

    async fn plan_install(&self, _: &str, _: Option<&str>) -> ToolResult<InstallPlan> {
        Err(ToolError::Backend(
            "no package backend available on this platform".into(),
        ))
    }

    async fn apply_install(&self, _: &str, _: Option<&str>) -> ToolResult<InstallApplied> {
        Err(ToolError::Backend(
            "no package backend available on this platform".into(),
        ))
    }
}

fn qualified_name(name: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{name}={v}"),
        None => name.to_string(),
    }
}

fn parse_apt_simulation(stdout: &str) -> Vec<uc_types::Change> {
    let mut changes = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Inst ") {
            if let Some(name) = rest.split_whitespace().next() {
                let version = rest
                    .split_whitespace()
                    .nth(1)
                    .map(|v| v.trim_matches(|c: char| c == '(' || c == ')').to_string())
                    .unwrap_or_default();
                changes.push(uc_types::Change {
                    kind: "package.install".into(),
                    target: name.to_string(),
                    before: "absent".into(),
                    after: version,
                });
            }
        } else if let Some(rest) = line.strip_prefix("Remv ") {
            if let Some(name) = rest.split_whitespace().next() {
                changes.push(uc_types::Change {
                    kind: "package.remove".into(),
                    target: name.to_string(),
                    before: "installed".into(),
                    after: "absent".into(),
                });
            }
        }
    }
    changes
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    s[start..].to_string()
}
