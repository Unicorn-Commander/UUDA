// SPDX-License-Identifier: Apache-2.0

//! `container.images` — running containers and how stale/unpinnable their
//! images are.
//!
//! Image currency is untracked across the fleet (~205 containers on one host,
//! ~53 on another). This probe answers, per host: what is running, from which
//! image reference, how old that image is, whether the reference is pinnable at
//! all, and whether anything is crash-looping.
//!
//! # Limitation: strictly local inspection, no registry contact
//!
//! This tool **never contacts a registry or any network**. It cannot tell you
//! that `nginx:latest` has a newer digest upstream — answering that needs a
//! registry client plus per-registry credentials, which is out of scope here.
//! What it *can* tell you is the local truth: the image's own build date, its
//! local digest, and whether the tag it was pulled by is even capable of
//! identifying a specific build. A `:latest` reference is flagged as
//! **unpinnable** for exactly that reason — its staleness is unknowable by
//! design, so it is reported as a property of the deployment, not measured.
//!
//! # Read-only
//!
//! Only `docker version`, `docker ps -q`, `docker inspect` and
//! `docker image inspect` are invoked. Never `pull`, `run`, `stop`, `rm`, or
//! `restart`. Root is not required: if the socket is not readable the tool
//! reports `docker_available: false` rather than failing or escalating.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::error::ToolResult;
use crate::exec;

/// Default age above which an image is called stale.
pub const DEFAULT_STALE_AFTER_DAYS: u32 = 90;
/// Default restart count above which a container is called crash-looping.
pub const DEFAULT_RESTART_THRESHOLD: u32 = 5;

const SECS_PER_DAY: i64 = 86_400;

/// Read-only docker subcommands this module may run.
const DOCKER_VERSION_ARGS: &[&str] = &["version", "--format", "{{.Server.Version}}"];
const DOCKER_PS_ARGS: &[&str] = &["ps", "-q", "--no-trunc"];

// ---- result types ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerInfo {
    pub name: String,
    pub id: String,
    /// The reference the container was created from, e.g. "nginx:1.27.1".
    pub image_ref: String,
    /// Resolved local image id (sha256:…).
    pub image_id: String,
    /// First RepoDigest, when the image was pulled rather than built locally.
    pub image_digest: Option<String>,
    pub image_created_unix: Option<i64>,
    pub image_age_days: Option<i64>,
    pub container_created_unix: Option<i64>,
    pub container_age_days: Option<i64>,
    /// "no" | "always" | "unless-stopped" | "on-failure".
    pub restart_policy: String,
    pub restart_count: u32,
    /// "healthy" | "unhealthy" | "starting" | None when no healthcheck.
    pub health_status: Option<String>,
    /// "running" | "restarting" | "exited" …
    pub state: String,

    /// The reference cannot identify a specific build (`:latest`, or no tag).
    pub unpinnable: bool,
    /// `restart_count` exceeds the threshold.
    pub restarting_frequently: bool,
    /// Image build date older than `stale_after_days`.
    pub image_stale: bool,
    /// Human-readable summary of the flags above.
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerImagesResult {
    /// False when docker is absent or the socket is not readable. Not an error.
    pub docker_available: bool,
    pub containers: Vec<ContainerInfo>,
    pub total_running: u32,
    pub unpinnable_count: u32,
    pub stale_count: u32,
    pub restarting_count: u32,
    pub unhealthy_count: u32,
    /// The threshold used, echoed so a caller knows what "stale" meant here.
    pub stale_after_days: u32,
    pub restart_threshold: u32,
    /// Always true: no registry was contacted. See the module docs.
    pub registry_checked: bool,
    pub error: Option<String>,
}

impl ContainerImagesResult {
    fn unavailable(opts: &ContainerOptions, error: Option<String>) -> Self {
        Self {
            docker_available: false,
            containers: Vec::new(),
            total_running: 0,
            unpinnable_count: 0,
            stale_count: 0,
            restarting_count: 0,
            unhealthy_count: 0,
            stale_after_days: opts.stale_after_days,
            restart_threshold: opts.restart_threshold,
            registry_checked: false,
            error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContainerOptions {
    pub stale_after_days: u32,
    pub restart_threshold: u32,
    pub timeout: Option<Duration>,
}

impl Default for ContainerOptions {
    fn default() -> Self {
        Self {
            stale_after_days: DEFAULT_STALE_AFTER_DAYS,
            restart_threshold: DEFAULT_RESTART_THRESHOLD,
            timeout: None,
        }
    }
}

// ---- parsing ---------------------------------------------------------------

/// Parse a docker RFC3339 timestamp (`2026-05-14T02:11:33.918204361Z`).
///
/// Docker writes `0001-01-01T00:00:00Z` for "never", which is returned as
/// `None` rather than as a nonsensical negative epoch.
pub fn parse_docker_time(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("0001-01-01") {
        return None;
    }
    let ts = time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .ok()?
        .unix_timestamp();
    (ts > 0).then_some(ts)
}

/// Is this image reference incapable of identifying a specific build?
///
/// True for `:latest` and for a bare name with no tag (docker implies
/// `:latest`). False for a real version tag or a digest pin. The registry-host
/// port in `registry.example.com:5000/app` is *not* a tag — only a colon in the
/// final path segment is.
pub fn is_unpinnable(image_ref: &str) -> bool {
    let r = image_ref.trim();
    if r.is_empty() {
        return true;
    }
    // A digest pin is the strongest form of pinning there is.
    if r.contains("@sha256:") {
        return false;
    }
    let last_segment = r.rsplit('/').next().unwrap_or(r);
    match last_segment.split_once(':') {
        Some((_, tag)) => tag.is_empty() || tag == "latest",
        None => true, // no tag at all => implicitly :latest
    }
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    match cur {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn u32_at(v: &Value, path: &[&str]) -> Option<u32> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_u64().map(|n| n as u32)
}

/// A `docker image inspect` entry, reduced to what we report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImageMeta {
    pub id: String,
    pub created_unix: Option<i64>,
    pub digest: Option<String>,
    pub tags: Vec<String>,
}

/// Parse `docker image inspect <id…>` (a JSON array) into id -> metadata.
pub fn parse_image_inspect(raw: &str) -> Vec<ImageMeta> {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return Vec::new();
    };
    let Some(list) = v.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|img| {
            let id = str_at(img, &["Id"])?;
            Some(ImageMeta {
                id,
                created_unix: str_at(img, &["Created"]).as_deref().and_then(parse_docker_time),
                digest: img
                    .get("RepoDigests")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string),
                tags: img
                    .get("RepoTags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn days_between(then: Option<i64>, now: i64) -> Option<i64> {
    then.map(|t| (now - t).max(0) / SECS_PER_DAY)
}

/// Build the full report from `docker inspect` + `docker image inspect` output.
///
/// `now_unix` is injected so ages are deterministic under test.
pub fn build_report(
    containers_raw: &str,
    images_raw: &str,
    opts: &ContainerOptions,
    now_unix: i64,
) -> ContainerImagesResult {
    let images = parse_image_inspect(images_raw);
    let list = serde_json::from_str::<Value>(containers_raw.trim())
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut containers: Vec<ContainerInfo> = Vec::with_capacity(list.len());
    for c in &list {
        let Some(id) = str_at(c, &["Id"]) else {
            continue;
        };
        let image_id = str_at(c, &["Image"]).unwrap_or_default();
        let image_ref = str_at(c, &["Config", "Image"]).unwrap_or_default();
        let meta = images.iter().find(|i| i.id == image_id);

        let image_created_unix = meta.and_then(|m| m.created_unix);
        let container_created_unix = str_at(c, &["Created"]).as_deref().and_then(parse_docker_time);

        let restart_count = u32_at(c, &["RestartCount"]).unwrap_or(0);
        let health_status = str_at(c, &["State", "Health", "Status"]);
        let state = str_at(c, &["State", "Status"]).unwrap_or_default();

        let image_age_days = days_between(image_created_unix, now_unix);
        let unpinnable = is_unpinnable(&image_ref);
        let restarting_frequently = restart_count > opts.restart_threshold;
        let image_stale = image_age_days
            .map(|d| d > opts.stale_after_days as i64)
            .unwrap_or(false);

        let mut flags = Vec::new();
        if unpinnable {
            flags.push(format!(
                "image reference {:?} cannot identify a build (unpinnable)",
                if image_ref.is_empty() { "<none>" } else { &image_ref }
            ));
        }
        if image_stale {
            flags.push(format!(
                "image built {} days ago (threshold {})",
                image_age_days.unwrap_or(0),
                opts.stale_after_days
            ));
        }
        if restarting_frequently {
            flags.push(format!(
                "restarted {restart_count} times (threshold {})",
                opts.restart_threshold
            ));
        }
        if health_status.as_deref() == Some("unhealthy") {
            flags.push("healthcheck reports unhealthy".to_string());
        }

        containers.push(ContainerInfo {
            name: str_at(c, &["Name"])
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default(),
            id,
            image_ref,
            image_id,
            image_digest: meta.and_then(|m| m.digest.clone()),
            image_created_unix,
            image_age_days,
            container_created_unix,
            container_age_days: days_between(container_created_unix, now_unix),
            restart_policy: str_at(c, &["HostConfig", "RestartPolicy", "Name"])
                .unwrap_or_else(|| "no".to_string()),
            restart_count,
            health_status,
            state,
            unpinnable,
            restarting_frequently,
            image_stale,
            flags,
        });
    }

    ContainerImagesResult {
        docker_available: true,
        total_running: containers.len() as u32,
        unpinnable_count: containers.iter().filter(|c| c.unpinnable).count() as u32,
        stale_count: containers.iter().filter(|c| c.image_stale).count() as u32,
        restarting_count: containers
            .iter()
            .filter(|c| c.restarting_frequently)
            .count() as u32,
        unhealthy_count: containers
            .iter()
            .filter(|c| c.health_status.as_deref() == Some("unhealthy"))
            .count() as u32,
        containers,
        stale_after_days: opts.stale_after_days,
        restart_threshold: opts.restart_threshold,
        registry_checked: false,
        error: None,
    }
}

// ---- collection ------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Inspect the running containers on this host. Local only — no registry, no
/// network, no root. Never returns `Err` for the "docker absent / socket not
/// readable" cases.
pub async fn collect(opts: ContainerOptions) -> ToolResult<ContainerImagesResult> {
    let timeout = opts.timeout.unwrap_or(exec::clamp_timeout(0));

    // Daemon reachability. A non-zero exit here is the unreadable-socket case
    // (unprivileged user, docker not running) — report it, never escalate.
    match exec::run("docker", DOCKER_VERSION_ARGS, timeout).await {
        Ok(out) if out.success => {}
        Ok(out) => {
            let why = out
                .stderr
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("docker daemon is not reachable")
                .to_string();
            return Ok(ContainerImagesResult::unavailable(&opts, Some(why)));
        }
        Err(e) if e.is_not_found() => {
            return Ok(ContainerImagesResult::unavailable(
                &opts,
                Some("docker CLI is not installed on this host".to_string()),
            ));
        }
        Err(e) => return Ok(ContainerImagesResult::unavailable(&opts, Some(e.to_string()))),
    }

    let ids: Vec<String> = match exec::run("docker", DOCKER_PS_ARGS, timeout).await {
        Ok(out) => out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(e) => {
            let mut r = ContainerImagesResult::unavailable(&opts, Some(e.to_string()));
            r.docker_available = true; // the daemon answered a moment ago
            return Ok(r);
        }
    };

    if ids.is_empty() {
        return Ok(build_report("[]", "[]", &opts, now_unix()));
    }

    let mut args: Vec<&str> = vec!["inspect"];
    args.extend(ids.iter().map(String::as_str));
    let containers_raw = match exec::run("docker", &args, timeout).await {
        Ok(out) => out.stdout,
        Err(e) => {
            let mut r = ContainerImagesResult::unavailable(&opts, Some(e.to_string()));
            r.docker_available = true;
            return Ok(r);
        }
    };

    // Unique image ids referenced by those containers, inspected in one call.
    let image_ids: BTreeSet<String> = serde_json::from_str::<Value>(containers_raw.trim())
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| str_at(c, &["Image"]))
        .collect();

    let images_raw = if image_ids.is_empty() {
        "[]".to_string()
    } else {
        let mut iargs: Vec<&str> = vec!["image", "inspect"];
        iargs.extend(image_ids.iter().map(String::as_str));
        match exec::run("docker", &iargs, timeout).await {
            Ok(out) => out.stdout,
            // Image metadata is a nice-to-have; a failure here downgrades the
            // report (no ages) rather than losing the container inventory.
            Err(_) => "[]".to_string(),
        }
    };

    let mut report = build_report(&containers_raw, &images_raw, &opts, now_unix());
    if report.containers.is_empty() && !ids.is_empty() {
        report.error = Some("docker inspect returned no parsable containers".to_string());
    }
    Ok(report)
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-13T00:00:00Z — the fixed "now" all age assertions use, so ages
    /// are deterministic rather than drifting with the wall clock.
    const NOW: i64 = 1_786_579_200;

    const INSPECT_JSON: &str = r#"[
  {
    "Id": "9c1f0a3b7e5d4c2a1b0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3928172635f4",
    "Created": "2026-08-01T04:12:09.113224Z",
    "Name": "/listmonk",
    "Image": "sha256:aaa1111111111111111111111111111111111111111111111111111111111111",
    "RestartCount": 0,
    "State": { "Status": "running", "Running": true, "StartedAt": "2026-08-01T04:12:10.001Z",
               "Health": { "Status": "healthy", "FailingStreak": 0 } },
    "Config": { "Image": "listmonk/listmonk:latest", "Hostname": "9c1f0a3b7e5d" },
    "HostConfig": { "RestartPolicy": { "Name": "unless-stopped", "MaximumRetryCount": 0 } }
  },
  {
    "Id": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809",
    "Created": "2026-07-30T22:00:00Z",
    "Name": "/postiz",
    "Image": "sha256:bbb2222222222222222222222222222222222222222222222222222222222222",
    "RestartCount": 41,
    "State": { "Status": "running", "Running": true, "StartedAt": "2026-08-12T23:58:00Z",
               "Health": { "Status": "unhealthy", "FailingStreak": 7 } },
    "Config": { "Image": "ghcr.io/gitroomhq/postiz-app:v1.44.0" },
    "HostConfig": { "RestartPolicy": { "Name": "always", "MaximumRetryCount": 0 } }
  },
  {
    "Id": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    "Created": "2026-02-11T09:30:00Z",
    "Name": "/registry-cache",
    "Image": "sha256:ccc3333333333333333333333333333333333333333333333333333333333333",
    "RestartCount": 1,
    "State": { "Status": "running", "Running": true, "StartedAt": "2026-02-11T09:30:05Z" },
    "Config": { "Image": "registry.example.com:5000/internal/cache" },
    "HostConfig": { "RestartPolicy": { "Name": "no", "MaximumRetryCount": 0 } }
  }
]"#;

    const IMAGE_INSPECT_JSON: &str = r#"[
  {
    "Id": "sha256:aaa1111111111111111111111111111111111111111111111111111111111111",
    "RepoTags": ["listmonk/listmonk:latest"],
    "RepoDigests": ["listmonk/listmonk@sha256:1111feed1111feed1111feed1111feed1111feed1111feed1111feed1111feed"],
    "Created": "2026-07-20T11:02:44.918204361Z"
  },
  {
    "Id": "sha256:bbb2222222222222222222222222222222222222222222222222222222222222",
    "RepoTags": ["ghcr.io/gitroomhq/postiz-app:v1.44.0"],
    "RepoDigests": ["ghcr.io/gitroomhq/postiz-app@sha256:2222beef2222beef2222beef2222beef2222beef2222beef2222beef2222beef"],
    "Created": "2026-07-28T06:41:00Z"
  },
  {
    "Id": "sha256:ccc3333333333333333333333333333333333333333333333333333333333333",
    "RepoTags": ["registry.example.com:5000/internal/cache:latest"],
    "RepoDigests": [],
    "Created": "2025-01-04T18:20:00Z"
  }
]"#;

    fn opts() -> ContainerOptions {
        ContainerOptions::default()
    }

    #[test]
    fn report_covers_every_running_container() {
        let r = build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &opts(), NOW);
        assert!(r.docker_available);
        assert_eq!(r.total_running, 3);
        assert_eq!(r.stale_after_days, DEFAULT_STALE_AFTER_DAYS);
        assert!(!r.registry_checked, "no registry is ever contacted");

        let names: Vec<&str> = r.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["listmonk", "postiz", "registry-cache"],
            "the leading slash is stripped from container names"
        );
    }

    #[test]
    fn identity_ages_and_policy_are_reported() {
        let r = build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &opts(), NOW);
        let listmonk = &r.containers[0];
        assert_eq!(listmonk.image_ref, "listmonk/listmonk:latest");
        assert_eq!(
            listmonk.image_id,
            "sha256:aaa1111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(listmonk
            .image_digest
            .as_deref()
            .unwrap()
            .starts_with("listmonk/listmonk@sha256:"));
        assert_eq!(listmonk.restart_policy, "unless-stopped");
        assert_eq!(listmonk.restart_count, 0);
        assert_eq!(listmonk.health_status.as_deref(), Some("healthy"));
        assert_eq!(listmonk.state, "running");
        assert_eq!(listmonk.container_age_days, Some(11), "created 2026-08-01");
        assert_eq!(listmonk.image_age_days, Some(23), "built 2026-07-20");
    }

    /// `:latest` and bare references cannot identify a build; a version tag or
    /// a digest can. The registry port is not a tag.
    #[test]
    fn unpinnable_references_are_identified_without_confusing_registry_ports() {
        assert!(is_unpinnable("nginx:latest"));
        assert!(is_unpinnable("nginx"));
        assert!(is_unpinnable("listmonk/listmonk:latest"));
        assert!(is_unpinnable("registry.example.com:5000/internal/cache"));
        assert!(is_unpinnable(""));

        assert!(!is_unpinnable("ghcr.io/gitroomhq/postiz-app:v1.44.0"));
        assert!(!is_unpinnable("registry.example.com:5000/internal/cache:2.1.0"));
        assert!(!is_unpinnable(
            "nginx@sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn unpinnable_and_stale_and_restarting_are_flagged_and_counted() {
        let r = build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &opts(), NOW);

        let listmonk = &r.containers[0];
        assert!(listmonk.unpinnable, ":latest is unpinnable");
        assert!(!listmonk.image_stale, "23 days is inside the 90-day window");

        let postiz = &r.containers[1];
        assert!(!postiz.unpinnable, "v1.44.0 is a real pin");
        assert!(postiz.restarting_frequently, "41 restarts");
        assert_eq!(postiz.health_status.as_deref(), Some("unhealthy"));
        assert!(postiz.flags.iter().any(|f| f.contains("restarted 41 times")));
        assert!(postiz.flags.iter().any(|f| f.contains("unhealthy")));

        let cache = &r.containers[2];
        assert!(cache.unpinnable, "a registry port is not a tag");
        assert!(cache.image_stale, "built January 2025");
        assert!(cache.image_age_days.unwrap() > 500);

        assert_eq!(r.unpinnable_count, 2);
        assert_eq!(r.stale_count, 1);
        assert_eq!(r.restarting_count, 1);
        assert_eq!(r.unhealthy_count, 1);
    }

    #[test]
    fn stale_threshold_is_honored() {
        let strict = ContainerOptions {
            stale_after_days: 10,
            ..Default::default()
        };
        let r = build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &strict, NOW);
        assert_eq!(r.stale_count, 3, "at 10 days every image is stale");
        assert_eq!(r.stale_after_days, 10, "the threshold used is echoed back");

        let lax = ContainerOptions {
            stale_after_days: 10_000,
            ..Default::default()
        };
        assert_eq!(
            build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &lax, NOW).stale_count,
            0
        );
    }

    #[test]
    fn a_container_with_no_healthcheck_has_no_health_status() {
        let r = build_report(INSPECT_JSON, IMAGE_INSPECT_JSON, &opts(), NOW);
        assert_eq!(r.containers[2].health_status, None);
    }

    /// Missing image metadata must degrade to "age unknown", never to "fresh".
    #[test]
    fn missing_image_metadata_leaves_age_unknown_and_not_stale() {
        let r = build_report(INSPECT_JSON, "[]", &opts(), NOW);
        assert_eq!(r.total_running, 3, "the inventory survives");
        assert!(r.containers.iter().all(|c| c.image_age_days.is_none()));
        assert_eq!(r.stale_count, 0, "unknown age is not asserted to be stale");
        assert!(r.containers.iter().all(|c| c.image_digest.is_none()));
    }

    #[test]
    fn docker_timestamps_parse_including_the_never_sentinel() {
        assert_eq!(
            parse_docker_time("2026-08-01T04:12:09.113224Z"),
            Some(1_785_557_529)
        );
        assert_eq!(parse_docker_time("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_docker_time(""), None);
        assert_eq!(parse_docker_time("not a timestamp"), None);
    }

    #[test]
    fn empty_and_garbage_docker_output_never_panics() {
        let empty = build_report("[]", "[]", &opts(), NOW);
        assert_eq!(empty.total_running, 0);
        assert!(empty.containers.is_empty());

        let garbage = build_report("this is not json", "neither is this", &opts(), NOW);
        assert_eq!(garbage.total_running, 0);
        assert!(parse_image_inspect("{}").is_empty());
    }

    #[tokio::test]
    async fn absent_docker_reports_unavailable_not_an_error() {
        let res = collect(ContainerOptions {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        })
        .await
        .expect("never errors");
        if !res.docker_available {
            assert!(res.containers.is_empty());
            assert!(res.error.is_some(), "explains why docker is unavailable");
            assert_eq!(res.stale_after_days, DEFAULT_STALE_AFTER_DAYS);
        }
    }
}
