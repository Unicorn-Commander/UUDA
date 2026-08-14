// SPDX-License-Identifier: Apache-2.0

//! `update.status` — apt currency, and whether the update machinery is wedged.
//!
//! # The failure this exists to catch
//!
//! A suspended ZFS pool made `zfs-scrub@<pool>.service` run `zpool scrub -w`.
//! That call entered uninterruptible **D-state**, held the dpkg frontend lock,
//! and silently killed all apt activity for **14 hours 51 minutes**. Nothing
//! noticed — every individual service was "running", the host was up, and no
//! metric moved.
//!
//! So this probe reports three things that would each have caught it:
//!
//! 1. **How long** `/var/lib/dpkg/lock-frontend` has been held, and by which
//!    PID and cmdline. Not "is it held" — *for how long*.
//! 2. **The count of processes in D (uninterruptible) state**, with cmdlines.
//!    The cheap universal proxy for wedged I/O.
//! 3. The **age** of the last successful `apt update`, which is what silently
//!    stops advancing when the above happens.
//!
//! # Read-only
//!
//! Reads `/proc` and `/var/lib/apt` directly and shells out only to
//! `apt list --upgradable` and `apt-mark showhold` — neither takes the dpkg
//! frontend lock, and both are time-bounded. **Never** takes the lock itself:
//! a probe that flocks the file to test it would be a second lock holder.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::ToolResult;
use crate::exec;

/// The frontend lock — the one that stalled for 14h51m.
pub const DPKG_LOCK_FRONTEND: &str = "/var/lib/dpkg/lock-frontend";
const APT_UPDATE_STAMP: &str = "/var/lib/apt/periodic/update-success-stamp";
const APT_LISTS_DIR: &str = "/var/lib/apt/lists";
const UNATTENDED_LOG: &str = "/var/log/unattended-upgrades/unattended-upgrades.log";
const REBOOT_REQUIRED: &str = "/var/run/reboot-required";
const REBOOT_REQUIRED_PKGS: &str = "/var/run/reboot-required.pkgs";
const PROC_LOCKS: &str = "/proc/locks";

/// Kernel clock ticks per second (`USER_HZ`), fixed at 100 on every Linux
/// architecture Debian/Ubuntu ship.
const USER_HZ: f64 = 100.0;

/// Cap on how many upgradable package names we enumerate in the payload.
const MAX_LISTED_PACKAGES: usize = 200;

// ---- result types ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradablePackage {
    pub name: String,
    pub version: String,
    /// Origin suites, e.g. ["jammy-updates", "jammy-security"].
    pub suites: Vec<String>,
    pub security: bool,
}

/// A process holding the dpkg frontend lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockHolder {
    pub pid: i32,
    pub comm: String,
    pub cmdline: String,
    /// Process state letter: R, S, **D**, Z, T…
    pub state: String,
    /// How long the holder has been alive. Derived from the process start time,
    /// which is the practical answer to "how long has apt been stuck?".
    pub held_for_secs: Option<u64>,
    /// "POSIX" | "FLOCK" | "OFDLCK".
    pub lock_kind: String,
    /// "READ" | "WRITE".
    pub lock_rw: String,
}

/// A process in uninterruptible sleep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DStateProcess {
    pub pid: i32,
    pub comm: String,
    pub cmdline: String,
    pub running_secs: Option<u64>,
}

/// The most recent outcome line from the unattended-upgrades log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnattendedOutcome {
    /// Raw timestamp text as logged, e.g. "2026-08-10 06:33:11".
    pub at: Option<String>,
    /// "INFO" | "WARNING" | "ERROR".
    pub level: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UpdateStatusResult {
    /// False on macOS and on non-apt Linux. The result is still well-formed.
    pub apt_available: bool,

    /// Seconds since the last successful `apt update`.
    pub last_update_age_secs: Option<u64>,
    pub last_update_unix: Option<i64>,
    /// "update-success-stamp" | "apt-lists-mtime" — which source was used.
    pub last_update_source: Option<String>,

    pub upgradable_count: u32,
    pub security_count: u32,
    pub upgradable: Vec<UpgradablePackage>,
    /// True when `upgradable` was truncated at [`MAX_LISTED_PACKAGES`].
    pub upgradable_truncated: bool,

    pub unattended_last_outcome: Option<UnattendedOutcome>,
    /// mtime of the unattended-upgrades log — the authoritative "when".
    pub unattended_log_mtime_unix: Option<i64>,

    /// True when a holder of the dpkg frontend lock was found.
    pub dpkg_lock_held: bool,
    pub dpkg_lock_holders: Vec<LockHolder>,
    /// Longest holder duration, in seconds. **This is the 14h51m number.**
    pub dpkg_lock_held_secs: Option<u64>,
    /// False when `/proc/locks` could not be read, so `dpkg_lock_held: false`
    /// must not be trusted as "not held".
    pub dpkg_lock_scan_complete: bool,

    /// Count of processes in D (uninterruptible) state.
    pub uninterruptible_count: u32,
    pub uninterruptible_processes: Vec<DStateProcess>,

    pub reboot_required: bool,
    pub reboot_required_packages: Vec<String>,

    /// Informational only. GPU nodes deliberately hold nvidia/kernel packages;
    /// a non-zero count here is a decision, not an alert.
    pub held_packages: Vec<String>,
    pub held_package_count: u32,

    /// Non-fatal problems (a timed-out apt, an unreadable log…).
    pub errors: Vec<String>,
}

// ---- parsers ---------------------------------------------------------------

/// Parse `apt list --upgradable`:
/// `libssl3/jammy-updates,jammy-security 3.0.2-0ubuntu1.15 amd64 [upgradable from: …]`
pub fn parse_apt_list_upgradable(raw: &str) -> Vec<UpgradablePackage> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // apt prints "Listing..." and a CLI-stability warning on stderr/stdout.
        .filter(|l| l.contains('/') && l.contains("upgradable from"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name_suites = fields.next()?;
            let version = fields.next().unwrap_or_default().to_string();
            let (name, suites) = name_suites.split_once('/')?;
            let suites: Vec<String> = suites.split(',').map(|s| s.trim().to_string()).collect();
            let security = suites.iter().any(|s| is_security_suite(s));
            Some(UpgradablePackage {
                name: name.to_string(),
                version,
                suites,
                security,
            })
        })
        .collect()
}

/// Ubuntu tags security suites `<codename>-security`; Debian uses
/// `Debian-Security` / `stable-security`.
fn is_security_suite(suite: &str) -> bool {
    let s = suite.to_ascii_lowercase();
    s.ends_with("-security") || s.contains("security")
}

/// Outcome phrases unattended-upgrades logs at the end of a run. Scanned from
/// the newest line backwards; the first match wins.
const UNATTENDED_OUTCOMES: &[&str] = &[
    "All upgrades installed",
    "No packages found that can be upgraded unattended",
    "Packages that were successfully auto-removed",
    "Installing the upgrades failed",
    "Unattended-upgrade returned",
    "Cache has broken packages",
    "Lock could not be acquired",
    "lock could not be acquired",
    "Aborting",
];

/// Parse `/var/log/unattended-upgrades/unattended-upgrades.log`, returning the
/// most recent recognisable outcome. Lines look like:
/// `2026-08-10 06:33:11,004 INFO All upgrades installed`
pub fn parse_unattended_log(raw: &str) -> Option<UnattendedOutcome> {
    let lines: Vec<&str> = raw.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let pick = lines
        .iter()
        .rev()
        .find(|l| UNATTENDED_OUTCOMES.iter().any(|m| l.contains(m)))
        .or_else(|| {
            // No known outcome (e.g. a run that died mid-way): fall back to the
            // last ERROR/WARNING, then to the last line, so we never claim
            // "nothing happened" when something clearly did.
            lines
                .iter()
                .rev()
                .find(|l| l.contains("ERROR") || l.contains("WARNING"))
                .or_else(|| lines.last())
        })?;
    Some(split_unattended_line(pick))
}

fn split_unattended_line(line: &str) -> UnattendedOutcome {
    let fields: Vec<&str> = line.splitn(4, ' ').collect();
    // "2026-08-10" "06:33:11,004" "INFO" "All upgrades installed"
    let looks_dated = fields.len() >= 3
        && fields[0].len() == 10
        && fields[0].split('-').count() == 3
        && fields[1].contains(':');
    if looks_dated {
        let ts = format!("{} {}", fields[0], fields[1].split(',').next().unwrap_or(""));
        UnattendedOutcome {
            at: Some(ts),
            level: Some(fields[2].to_string()),
            message: fields.get(3).unwrap_or(&"").trim().to_string(),
        }
    } else {
        UnattendedOutcome {
            at: None,
            level: None,
            message: line.to_string(),
        }
    }
}

/// One row of `/proc/locks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockEntry {
    pub kind: String,
    pub mode: String,
    pub rw: String,
    pub pid: i32,
    pub inode: u64,
}

/// Parse `/proc/locks`:
/// `4: POSIX  ADVISORY  WRITE 214251 08:02:1310726 0 EOF`
///
/// World-readable, so the lock holder can be identified **without root** — the
/// point being that this probe works on any host, not just ones we run
/// privileged on.
pub fn parse_proc_locks(raw: &str) -> Vec<LockEntry> {
    raw.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // "N:" kind mode rw pid maj:min:inode start end
            if f.len() < 7 {
                return None;
            }
            // Rows start with an index ("3:"); blocked waiters are listed as a
            // continuation row ("3: -> POSIX …"). Skip both prefixes.
            let mut off = usize::from(f[0].ends_with(':'));
            if f.get(off) == Some(&"->") {
                off += 1;
            }
            let kind = f.get(off)?;
            let mode = f.get(off + 1)?;
            let rw = f.get(off + 2)?;
            let pid: i32 = f.get(off + 3)?.parse().ok()?;
            let inode: u64 = f.get(off + 4)?.rsplit(':').next()?.parse().ok()?;
            Some(LockEntry {
                kind: kind.to_string(),
                mode: mode.to_string(),
                rw: rw.to_string(),
                pid,
                inode,
            })
        })
        .collect()
}

/// The fields of `/proc/<pid>/stat` this probe uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i32,
    pub comm: String,
    /// R, S, D, Z, T, I…
    pub state: char,
    /// Field 22: process start time, in clock ticks since boot.
    pub starttime_ticks: u64,
}

/// Parse `/proc/<pid>/stat`.
///
/// The `comm` field is the process name **in parentheses and unescaped**, so it
/// can itself contain spaces and parentheses (`1234 (Web Content (x)) D 1 …`).
/// Splitting on whitespace naively — the classic bug — misreads the state of
/// exactly the processes you most want to see. Split on the *last* `)`.
pub fn parse_proc_stat(raw: &str) -> Option<ProcStat> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: i32 = raw[..open].trim().parse().ok()?;
    let comm = raw[open + 1..close].to_string();
    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state), so field N is rest[N - 3].
    let state = rest.first()?.chars().next()?;
    let starttime_ticks = rest.get(19).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    Some(ProcStat {
        pid,
        comm,
        state,
        starttime_ticks,
    })
}

/// Parse `/proc/uptime` ("1234567.89 9876543.21") into seconds.
pub fn parse_proc_uptime(raw: &str) -> Option<f64> {
    raw.split_whitespace().next()?.parse().ok()
}

/// How long a process has been alive, from its start time and system uptime.
pub fn process_age_secs(starttime_ticks: u64, uptime_secs: f64) -> Option<u64> {
    if starttime_ticks == 0 {
        return None;
    }
    let started_at = starttime_ticks as f64 / USER_HZ;
    let age = uptime_secs - started_at;
    (age >= 0.0).then_some(age as u64)
}

/// Parse `apt-mark showhold` (one package name per line).
pub fn parse_showhold(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' '))
        .map(str::to_string)
        .collect()
}

// ---- host reads ------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_unix(path: &str) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn read_cmdline(pid: i32) -> String {
    // /proc/<pid>/cmdline is NUL-separated and world-readable.
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => {
            let s = String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            s.trim().to_string()
        }
        Err(_) => String::new(),
    }
}

/// Walk `/proc`, returning (`ProcStat`, age) for every process passing `keep`.
fn scan_processes<F>(uptime: f64, keep: F) -> Vec<(ProcStat, Option<u64>)>
where
    F: Fn(&ProcStat) -> bool,
{
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // A process can exit between readdir and read; that is not an error.
        let Ok(raw) = std::fs::read_to_string(format!("/proc/{name}/stat")) else {
            continue;
        };
        let Some(stat) = parse_proc_stat(&raw) else {
            continue;
        };
        if keep(&stat) {
            let age = process_age_secs(stat.starttime_ticks, uptime);
            out.push((stat, age));
        }
    }
    out.sort_by_key(|(s, _)| s.pid);
    out
}

// ---- collection ------------------------------------------------------------

/// Collect apt currency + wedge signals. Never returns `Err`; a non-apt or
/// non-Linux host comes back with `apt_available: false`.
pub async fn collect(timeout: Option<Duration>) -> ToolResult<UpdateStatusResult> {
    let timeout = timeout.unwrap_or(exec::clamp_timeout(0));
    let mut res = UpdateStatusResult::default();

    if !cfg!(target_os = "linux") {
        res.errors.push(format!(
            "update.status is Debian/Ubuntu-only (host is {})",
            std::env::consts::OS
        ));
        return Ok(res);
    }

    // The D-state count and lock scan are /proc facts, not apt facts — they are
    // worth reporting on any Linux host, including non-apt ones.
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| parse_proc_uptime(&s))
        .unwrap_or(0.0);

    let dstate = scan_processes(uptime, |s| s.state == 'D');
    res.uninterruptible_count = dstate.len() as u32;
    res.uninterruptible_processes = dstate
        .into_iter()
        .map(|(s, age)| DStateProcess {
            comm: s.comm,
            cmdline: read_cmdline(s.pid),
            pid: s.pid,
            running_secs: age,
        })
        .collect();

    collect_lock_state(&mut res, uptime);

    res.apt_available = Path::new("/usr/bin/apt-get").exists() || Path::new("/usr/bin/apt").exists();
    if !res.apt_available {
        res.errors
            .push("apt is not installed on this host (non-Debian Linux)".to_string());
        return Ok(res);
    }

    // --- last successful apt update ---
    if let Some(ts) = mtime_unix(APT_UPDATE_STAMP) {
        res.last_update_unix = Some(ts);
        res.last_update_source = Some("update-success-stamp".to_string());
    } else if let Some(ts) = mtime_unix(APT_LISTS_DIR) {
        res.last_update_unix = Some(ts);
        res.last_update_source = Some("apt-lists-mtime".to_string());
    }
    res.last_update_age_secs = res
        .last_update_unix
        .map(|t| (now_unix() - t).max(0) as u64);

    // --- upgradable + security ---
    match exec::run("apt", &["list", "--upgradable"], timeout).await {
        Ok(out) => {
            let mut pkgs = parse_apt_list_upgradable(&out.stdout);
            res.upgradable_count = pkgs.len() as u32;
            res.security_count = pkgs.iter().filter(|p| p.security).count() as u32;
            if pkgs.len() > MAX_LISTED_PACKAGES {
                pkgs.truncate(MAX_LISTED_PACKAGES);
                res.upgradable_truncated = true;
            }
            res.upgradable = pkgs;
        }
        Err(e) => res.errors.push(format!("apt list --upgradable: {e}")),
    }

    // --- unattended-upgrades ---
    res.unattended_log_mtime_unix = mtime_unix(UNATTENDED_LOG);
    match std::fs::read_to_string(UNATTENDED_LOG) {
        Ok(log) => res.unattended_last_outcome = parse_unattended_log(&log),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => res
            .errors
            .push(format!("read {UNATTENDED_LOG}: {e}")),
    }

    // --- reboot required ---
    res.reboot_required = Path::new(REBOOT_REQUIRED).exists();
    if let Ok(pkgs) = std::fs::read_to_string(REBOOT_REQUIRED_PKGS) {
        let mut list: Vec<String> = pkgs
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        list.sort();
        list.dedup();
        res.reboot_required_packages = list;
    }

    // --- held packages (informational: GPU nodes pin nvidia/kernel on purpose) ---
    match exec::run("apt-mark", &["showhold"], timeout).await {
        Ok(out) => {
            res.held_packages = parse_showhold(&out.stdout);
            res.held_package_count = res.held_packages.len() as u32;
        }
        Err(e) if e.is_not_found() => {}
        Err(e) => res.errors.push(format!("apt-mark showhold: {e}")),
    }

    Ok(res)
}

/// Identify who holds the dpkg frontend lock, and for how long.
fn collect_lock_state(res: &mut UpdateStatusResult, uptime: f64) {
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(DPKG_LOCK_FRONTEND) else {
        // No lock file at all (non-dpkg host). Nothing to report, and the scan
        // did complete — there is genuinely nothing held.
        res.dpkg_lock_scan_complete = true;
        return;
    };
    let target_inode = meta.ino();

    let Ok(locks_raw) = std::fs::read_to_string(PROC_LOCKS) else {
        res.errors
            .push(format!("could not read {PROC_LOCKS}; lock state unknown"));
        return;
    };
    res.dpkg_lock_scan_complete = true;

    let holders: Vec<LockEntry> = parse_proc_locks(&locks_raw)
        .into_iter()
        .filter(|l| l.inode == target_inode && l.pid > 0)
        .collect();

    for entry in holders {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", entry.pid))
            .ok()
            .and_then(|s| parse_proc_stat(&s));
        let held_for_secs = stat
            .as_ref()
            .and_then(|s| process_age_secs(s.starttime_ticks, uptime));
        res.dpkg_lock_holders.push(LockHolder {
            pid: entry.pid,
            comm: stat.as_ref().map(|s| s.comm.clone()).unwrap_or_default(),
            cmdline: read_cmdline(entry.pid),
            state: stat.map(|s| s.state.to_string()).unwrap_or_default(),
            held_for_secs,
            lock_kind: entry.kind,
            lock_rw: entry.rw,
        });
    }

    res.dpkg_lock_held = !res.dpkg_lock_holders.is_empty();
    res.dpkg_lock_held_secs = res
        .dpkg_lock_holders
        .iter()
        .filter_map(|h| h.held_for_secs)
        .max();
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const APT_UPGRADABLE: &str = r#"
Listing...
WARNING: apt does not have a stable CLI interface. Use with caution in scripts.
libssl3/jammy-updates,jammy-security 3.0.2-0ubuntu1.15 amd64 [upgradable from: 3.0.2-0ubuntu1.12]
nginx-common/jammy-updates 1.18.0-6ubuntu14.4 all [upgradable from: 1.18.0-6ubuntu14.3]
curl/jammy-security 7.81.0-1ubuntu1.16 amd64 [upgradable from: 7.81.0-1ubuntu1.15]
docker-ce/jammy 5:26.1.4-1~ubuntu.22.04~jammy amd64 [upgradable from: 5:26.1.3-1~ubuntu.22.04~jammy]
"#;

    /// The end of a clean unattended run, followed by a later failed one.
    const UNATTENDED_LOG_TEXT: &str = r#"
2026-08-09 06:31:14,132 INFO Starting unattended upgrades script
2026-08-09 06:31:44,915 INFO Packages that will be upgraded: libssl3
2026-08-09 06:33:11,004 INFO All upgrades installed
2026-08-10 06:30:02,551 INFO Starting unattended upgrades script
2026-08-10 06:30:02,904 WARNING Could not figure out development release: None
2026-08-10 06:35:09,118 ERROR Installing the upgrades failed!
2026-08-10 06:35:09,119 ERROR error message: 'Could not get lock /var/lib/dpkg/lock-frontend'
"#;

    /// The 14h51m signature: dpkg's frontend lock inode (1310726) is held by a
    /// long-lived process, plus a D-state `zpool scrub -w`.
    const PROC_LOCKS_WEDGED: &str = r#"1: POSIX  ADVISORY  READ  1023 08:02:786434 128 128
2: FLOCK  ADVISORY  WRITE 1187 00:16:1052 0 EOF
3: POSIX  ADVISORY  WRITE 214251 08:02:1310726 0 EOF
4: POSIX  ADVISORY  WRITE 214251 08:02:1310727 0 EOF
"#;

    const PROC_LOCKS_IDLE: &str = r#"1: POSIX  ADVISORY  READ  1023 08:02:786434 128 128
2: FLOCK  ADVISORY  WRITE 1187 00:16:1052 0 EOF
"#;

    // ---- apt list ----------------------------------------------------------

    #[test]
    fn upgradable_parses_and_counts_security_separately() {
        let pkgs = parse_apt_list_upgradable(APT_UPGRADABLE);
        assert_eq!(pkgs.len(), 4, "the Listing.../WARNING preamble is dropped");

        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["libssl3", "nginx-common", "curl", "docker-ce"]);

        assert_eq!(pkgs[0].version, "3.0.2-0ubuntu1.15");
        assert_eq!(pkgs[0].suites, vec!["jammy-updates", "jammy-security"]);

        let security: Vec<&str> = pkgs
            .iter()
            .filter(|p| p.security)
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            security,
            vec!["libssl3", "curl"],
            "only packages from a -security suite count"
        );
        assert!(!pkgs[3].security, "a third-party repo is not a security suite");
    }

    #[test]
    fn empty_apt_output_yields_no_packages() {
        assert!(parse_apt_list_upgradable("Listing...\n").is_empty());
        assert!(parse_apt_list_upgradable("").is_empty());
    }

    // ---- unattended-upgrades ----------------------------------------------

    /// Two runs are in the log: a clean one on the 9th and a failed one on the
    /// 10th. The probe must report the *newest* run's outcome, and must report
    /// the outcome line itself rather than whatever trailing detail line
    /// happens to be last in the file.
    #[test]
    fn unattended_log_returns_the_most_recent_outcome_not_the_first() {
        let out = parse_unattended_log(UNATTENDED_LOG_TEXT).expect("an outcome");
        assert_eq!(out.at.as_deref(), Some("2026-08-10 06:35:09"));
        assert_eq!(out.level.as_deref(), Some("ERROR"));
        assert!(
            out.message.contains("Installing the upgrades failed"),
            "the newest run's outcome wins over the older clean run: {}",
            out.message
        );
        assert!(
            !out.message.contains("All upgrades installed"),
            "the stale success from the 9th must not be reported as current"
        );
    }

    #[test]
    fn unattended_log_reads_a_clean_run() {
        let clean = "2026-08-09 06:33:11,004 INFO All upgrades installed\n";
        let out = parse_unattended_log(clean).expect("an outcome");
        assert_eq!(out.level.as_deref(), Some("INFO"));
        assert_eq!(out.message, "All upgrades installed");
        assert_eq!(out.at.as_deref(), Some("2026-08-09 06:33:11"));
    }

    #[test]
    fn unattended_log_without_a_known_outcome_still_reports_something() {
        let odd = "2026-08-10 06:30:02,551 INFO Starting unattended upgrades script\n";
        let out = parse_unattended_log(odd).expect("falls back to the last line");
        assert!(out.message.contains("Starting"));
        assert!(parse_unattended_log("").is_none(), "empty log is None");
    }

    // ---- /proc/locks -------------------------------------------------------

    #[test]
    fn proc_locks_parses_and_matches_the_frontend_lock_by_inode() {
        let locks = parse_proc_locks(PROC_LOCKS_WEDGED);
        assert_eq!(locks.len(), 4);
        assert_eq!(locks[2].pid, 214251);
        assert_eq!(locks[2].inode, 1_310_726);
        assert_eq!(locks[2].kind, "POSIX");
        assert_eq!(locks[2].rw, "WRITE");

        let held: Vec<&LockEntry> = locks.iter().filter(|l| l.inode == 1_310_726).collect();
        assert_eq!(held.len(), 1, "exactly one holder of the frontend lock");
        assert_eq!(held[0].pid, 214251);

        // And the idle host holds nothing on that inode.
        let idle = parse_proc_locks(PROC_LOCKS_IDLE);
        assert!(idle.iter().all(|l| l.inode != 1_310_726));
    }

    #[test]
    fn proc_locks_garbage_is_skipped_not_fatal() {
        assert!(parse_proc_locks("").is_empty());
        assert!(parse_proc_locks("garbage line with no numbers at all").is_empty());
    }

    // ---- /proc/<pid>/stat --------------------------------------------------

    /// The classic parsing bug: `comm` is unescaped, so a process named
    /// `zpool scrub (tank)` breaks naive whitespace splitting — and D-state
    /// processes are exactly the ones you cannot afford to misread.
    #[test]
    fn proc_stat_survives_a_comm_containing_spaces_and_parens() {
        let raw = "214251 (zpool scrub (tank)) D 1 214251 214251 0 -1 4194304 1234 0 0 0 41 288 \
                   0 0 20 0 1 0 5347100 6086656 512 18446744073709551615 1 1 0 0 0 0 0 0 0";
        let s = parse_proc_stat(raw).expect("parses");
        assert_eq!(s.pid, 214251);
        assert_eq!(s.comm, "zpool scrub (tank)");
        assert_eq!(s.state, 'D', "uninterruptible sleep");
        assert_eq!(s.starttime_ticks, 5_347_100);
    }

    #[test]
    fn proc_stat_reads_an_ordinary_process() {
        let raw = "1187 (systemd) S 1 1187 1187 0 -1 4194560 900 0 0 0 3 7 0 0 20 0 1 0 \
                   410 12345678 900 18446744073709551615 1 1 0 0 0 0 0 0 0";
        let s = parse_proc_stat(raw).expect("parses");
        assert_eq!(s.pid, 1187);
        assert_eq!(s.comm, "systemd");
        assert_eq!(s.state, 'S');
        assert_eq!(s.starttime_ticks, 410);
        assert!(parse_proc_stat("total nonsense").is_none());
    }

    /// The number that would have raised the alarm: 14h51m of held lock.
    #[test]
    fn process_age_reproduces_the_14h51m_wedge() {
        let uptime = 120_000.0; // seconds since boot
        let started_ticks = (120_000.0 - 53_460.0) * USER_HZ; // 14h51m ago
        let age = process_age_secs(started_ticks as u64, uptime).expect("an age");
        assert_eq!(age, 53_460, "14h51m in seconds");
        assert!(age > 14 * 3600, "well past any plausible apt run");
    }

    #[test]
    fn process_age_is_none_when_starttime_is_unknown() {
        assert_eq!(process_age_secs(0, 1000.0), None);
        // A clock skew that would produce a negative age yields None, not a
        // wildly wrong u64 wrap.
        assert_eq!(process_age_secs(500_000, 10.0), None);
    }

    #[test]
    fn uptime_parses_the_first_field() {
        assert_eq!(parse_proc_uptime("120000.42 987654.21\n"), Some(120000.42));
        assert_eq!(parse_proc_uptime(""), None);
    }

    // ---- holds -------------------------------------------------------------

    #[test]
    fn showhold_lists_pinned_packages() {
        let raw = "nvidia-driver-550\nlinux-image-generic\nlinux-headers-generic\n";
        let held = parse_showhold(raw);
        assert_eq!(held.len(), 3);
        assert!(held.contains(&"nvidia-driver-550".to_string()));
        assert!(parse_showhold("").is_empty());
    }

    // ---- platform ----------------------------------------------------------

    #[tokio::test]
    async fn non_linux_reports_apt_unavailable_cleanly() {
        if cfg!(target_os = "linux") {
            return;
        }
        let res = collect(Some(Duration::from_secs(5)))
            .await
            .expect("never errors");
        assert!(!res.apt_available);
        assert_eq!(res.upgradable_count, 0);
        assert!(!res.errors.is_empty(), "says why");
    }
}
