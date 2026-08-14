// SPDX-License-Identifier: Apache-2.0

//! `zfs.status` — pool health, per-device error counters, and the SUSPENDED flag.
//!
//! # Why `SUSPENDED` gets its own boolean
//!
//! A suspended pool on bigboy caused `zfs-scrub@<pool>.service` to run
//! `zpool scrub -w` (wait for completion). Against a suspended pool that call
//! never returns: it entered uninterruptible **D-state**, held the dpkg
//! frontend lock, and silently killed *all* apt activity for **14h51m**.
//! Nothing noticed, because everything downstream only looked for `DEGRADED`.
//!
//! `SUSPENDED` is a distinct pool state from `DEGRADED` — a degraded pool still
//! serves I/O, a suspended pool has stopped and is waiting for an admin. So it
//! is surfaced as its own field, `suspended`, not folded into a health string
//! that a caller has to substring-match.
//!
//! # CRITICAL SAFETY: this tool is strictly READ-ONLY
//!
//! It invokes **only** `zpool list` and `zpool status`, and it **never** passes
//! `-w`. It must never invoke `zpool scrub`, `clear`, `online`, `offline`,
//! `replace`, `import`, or `export`.
//!
//! The reason is the incident above: `zpool scrub -w` against a suspended pool
//! is precisely what wedged the host for 15 hours. `-w` blocks in the kernel
//! until the scrub completes, which on a suspended pool is never; the process
//! is then unkillable and anything holding a lock behind it stops forever. A
//! health *probe* that can trigger that is a health hazard. The allowed argv
//! are pinned in the constants below and asserted by a unit test.

use std::time::Duration;

use serde::Serialize;

use crate::error::ToolResult;
use crate::exec::{self};

// ---- pinned, read-only invocations -----------------------------------------
//
// Every zpool argv this module may ever run. Read-only subcommands only, and
// never `-w` (see the module docs: `zpool scrub -w` is what caused the 14h51m
// D-state wedge). `zfs_argv_is_read_only` in the tests enforces this.

/// Pool inventory. `-p` gives exact byte values; older zfs still prints "9.09T",
/// which [`parse_size_field`] also handles.
const ZPOOL_LIST_ARGS: &[&str] = &[
    "list",
    "-H",
    "-p",
    "-o",
    "name,size,alloc,free,health,fragmentation,capacity",
];

/// Full status with vdev paths (`-P`). No `-w`, no scrub, no clear.
const ZPOOL_STATUS_ARGS: &[&str] = &["status", "-P"];

/// "Only pools with problems" summary line.
const ZPOOL_STATUS_X_ARGS: &[&str] = &["status", "-x"];

/// Subcommands and flags that would mutate a pool or block on it. Never used —
/// kept here next to the allowed argv, and asserted against them by
/// `zfs_argv_is_read_only` in the tests below.
#[cfg(test)]
const FORBIDDEN_TOKENS: &[&str] = &[
    "scrub", "clear", "online", "offline", "replace", "import", "export", "destroy", "attach",
    "detach", "remove", "split", "trim", "initialize", "resilver", "upgrade", "set", "-w",
];

// ---- result types ----------------------------------------------------------

/// One row of the `zpool status` config table (a vdev or a leaf device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZfsDevice {
    pub name: String,
    pub state: String,
    /// Nesting depth in the config tree: 0 = the pool row, 1 = a top-level
    /// vdev (mirror-0, raidz1-0), 2 = a leaf device.
    pub depth: u32,
    pub read_errors: u64,
    pub write_errors: u64,
    pub cksum_errors: u64,
    /// Trailing note on the row, e.g. "(resilvering)" or "too many errors".
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZfsPool {
    pub name: String,
    /// Raw health string: ONLINE / DEGRADED / SUSPENDED / FAULTED / UNAVAIL / …
    pub health: String,
    /// **The 14h51m field.** Distinct from `degraded`.
    pub suspended: bool,
    pub degraded: bool,
    pub faulted: bool,
    pub size_bytes: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub capacity_pct: Option<u64>,
    pub fragmentation_pct: Option<u64>,
    pub scrub_in_progress: bool,
    pub resilver_in_progress: bool,
    /// The whole `scan:` stanza, verbatim.
    pub scan_line: Option<String>,
    /// Date text of the last completed scrub, e.g. "Sun Aug 10 09:36:34 2026".
    pub last_scrub_at: Option<String>,
    /// e.g. "repaired 0B in 09:12:33 with 0 errors".
    pub last_scrub_result: Option<String>,
    pub last_scrub_errors: Option<u64>,
    /// The `status:` explanation, when zfs printed one.
    pub status_detail: Option<String>,
    /// The `action:` recommendation, when zfs printed one.
    pub action: Option<String>,
    /// The `errors:` line, e.g. "No known data errors".
    pub errors_line: Option<String>,
    pub devices: Vec<ZfsDevice>,
    pub total_read_errors: u64,
    pub total_write_errors: u64,
    pub total_cksum_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZfsResult {
    /// False when zfs is not installed. The result is still well-formed.
    pub zfs_available: bool,
    pub pools: Vec<ZfsPool>,
    /// True if **any** pool is suspended — the top-level alarm bit.
    pub any_suspended: bool,
    pub any_degraded: bool,
    /// Names of pools reporting non-zero read/write/cksum errors.
    pub pools_with_errors: Vec<String>,
    /// The `zpool status -x` one-liner ("all pools are healthy").
    pub status_summary: Option<String>,
    /// Set on timeout or command failure; the rest of the result is partial.
    pub error: Option<String>,
}

impl ZfsResult {
    fn unavailable(error: Option<String>) -> Self {
        Self {
            zfs_available: false,
            pools: Vec::new(),
            any_suspended: false,
            any_degraded: false,
            pools_with_errors: Vec::new(),
            status_summary: None,
            error,
        }
    }
}

// ---- parsing ---------------------------------------------------------------

/// Parse a zpool numeric field: `9998683942912`, `9.09T`, `59%`, or `-`.
pub fn parse_size_field(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches('%');
    if s.is_empty() || s == "-" {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let idx = s.find(|c: char| c.is_alphabetic())?;
    let (num, suffix) = s.split_at(idx);
    let num: f64 = num.trim().parse().ok()?;
    let mult: f64 = match suffix.trim().chars().next()?.to_ascii_uppercase() {
        'K' => 1024.0,
        'M' => 1024f64.powi(2),
        'G' => 1024f64.powi(3),
        'T' => 1024f64.powi(4),
        'P' => 1024f64.powi(5),
        'E' => 1024f64.powi(6),
        'B' => 1.0,
        _ => return None,
    };
    Some((num * mult) as u64)
}

/// A zfs error counter, which may be abbreviated ("2.78K" = 2780).
fn parse_error_count(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let idx = s.find(|c: char| c.is_alphabetic())?;
    let (num, suffix) = s.split_at(idx);
    let num: f64 = num.parse().ok()?;
    // Error counts use decimal SI abbreviations, not binary.
    let mult: f64 = match suffix.trim().chars().next()?.to_ascii_uppercase() {
        'K' => 1e3,
        'M' => 1e6,
        'G' => 1e9,
        _ => return None,
    };
    Some((num * mult) as u64)
}

/// A partially-filled pool row from `zpool list -H`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PoolListRow {
    pub name: String,
    pub health: String,
    pub size_bytes: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub fragmentation_pct: Option<u64>,
    pub capacity_pct: Option<u64>,
}

/// Parse `zpool list -H -p -o name,size,alloc,free,health,fragmentation,capacity`
/// (tab-separated, no header).
pub fn parse_zpool_list(raw: &str) -> Vec<PoolListRow> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            // -H separates with tabs, but tolerate any whitespace run.
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 {
                return None;
            }
            Some(PoolListRow {
                name: f[0].to_string(),
                size_bytes: parse_size_field(f[1]),
                allocated_bytes: parse_size_field(f[2]),
                free_bytes: parse_size_field(f[3]),
                health: f[4].to_string(),
                fragmentation_pct: f.get(5).and_then(|v| parse_size_field(v)),
                capacity_pct: f.get(6).and_then(|v| parse_size_field(v)),
            })
        })
        .collect()
}

/// Everything `zpool status -P` tells us about one pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PoolStatusBlock {
    pub name: String,
    pub state: String,
    pub status_detail: Option<String>,
    pub action: Option<String>,
    pub scan_line: Option<String>,
    pub errors_line: Option<String>,
    pub devices: Vec<ZfsDevice>,
}

/// Known vdev/device states, used to tell a config row from prose.
const ZFS_STATES: &[&str] = &[
    "ONLINE",
    "DEGRADED",
    "FAULTED",
    "OFFLINE",
    "UNAVAIL",
    "REMOVED",
    "SUSPENDED",
    "AVAIL",
    "INUSE",
    "SPLIT",
];

/// Parse `zpool status -P` into one block per pool.
///
/// The format is a set of `key: value` stanzas (with indented continuation
/// lines) followed by a `config:` table whose indentation encodes the vdev
/// tree, then an `errors:` line.
pub fn parse_zpool_status(raw: &str) -> Vec<PoolStatusBlock> {
    let mut blocks: Vec<PoolStatusBlock> = Vec::new();
    let mut cur: Option<PoolStatusBlock> = None;
    let mut in_config = false;
    // Which stanza a continuation line belongs to.
    let mut last_key = String::new();

    for raw_line in raw.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("pool:") {
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            cur = Some(PoolStatusBlock {
                name: rest.trim().to_string(),
                ..Default::default()
            });
            in_config = false;
            last_key = "pool".into();
            continue;
        }
        let Some(block) = cur.as_mut() else {
            continue;
        };

        if let Some(rest) = trimmed.strip_prefix("state:") {
            block.state = rest.trim().to_string();
            in_config = false;
            last_key = "state".into();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("status:") {
            block.status_detail = Some(rest.trim().to_string());
            in_config = false;
            last_key = "status".into();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("action:") {
            block.action = Some(rest.trim().to_string());
            in_config = false;
            last_key = "action".into();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("scan:") {
            block.scan_line = Some(rest.trim().to_string());
            in_config = false;
            last_key = "scan".into();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("errors:") {
            block.errors_line = Some(rest.trim().to_string());
            in_config = false;
            last_key = "errors".into();
            continue;
        }
        if trimmed.starts_with("see:") || trimmed.starts_with("remove:") {
            in_config = false;
            last_key = "see".into();
            continue;
        }
        if trimmed.starts_with("config:") {
            in_config = true;
            last_key = "config".into();
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }

        if in_config {
            if let Some(dev) = parse_config_row(line) {
                block.devices.push(dev);
            }
            continue;
        }

        // Indented continuation of the previous stanza (long status/scan text).
        if raw_line.starts_with(char::is_whitespace) {
            match last_key.as_str() {
                "status" => append_line(&mut block.status_detail, trimmed),
                "action" => append_line(&mut block.action, trimmed),
                "scan" => append_line(&mut block.scan_line, trimmed),
                "errors" => append_line(&mut block.errors_line, trimmed),
                _ => {}
            }
        }
    }
    if let Some(b) = cur {
        blocks.push(b);
    }
    blocks
}

fn append_line(slot: &mut Option<String>, text: &str) {
    match slot {
        Some(existing) => {
            existing.push(' ');
            existing.push_str(text);
        }
        None => *slot = Some(text.to_string()),
    }
}

/// Parse one `config:` row: `<indent><NAME> <STATE> <READ> <WRITE> <CKSUM> [note]`.
/// Rows that are not device rows (the `NAME STATE READ WRITE CKSUM` header,
/// section labels like `spares`) return `None`.
fn parse_config_row(line: &str) -> Option<ZfsDevice> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("NAME") {
        return None; // the table header
    }
    let f: Vec<&str> = trimmed.split_whitespace().collect();
    if f.len() < 5 {
        return None;
    }
    if !ZFS_STATES.contains(&f[1]) {
        return None;
    }
    let read = parse_error_count(f[2])?;
    let write = parse_error_count(f[3])?;
    let cksum = parse_error_count(f[4])?;

    // Indentation encodes the tree. `zpool status` indents the table by one tab
    // and then two spaces per level; normalize either.
    let indent = line.len() - trimmed.len();
    let indent = line[..indent].replace('\t', "  ");
    let depth = (indent.len().saturating_sub(2) / 2) as u32;

    let note = if f.len() > 5 {
        Some(f[5..].join(" "))
    } else {
        None
    };

    Some(ZfsDevice {
        name: f[0].to_string(),
        state: f[1].to_string(),
        depth,
        read_errors: read,
        write_errors: write,
        cksum_errors: cksum,
        note,
    })
}

/// Split a `scan:` stanza into (in-progress flags, last result, last date).
fn parse_scan(scan: &str) -> (bool, bool, Option<String>, Option<String>, Option<u64>) {
    let lower = scan.to_ascii_lowercase();
    let scrub_running = lower.contains("scrub in progress");
    let resilver_running = lower.contains("resilver in progress");

    let mut result = None;
    let mut at = None;
    let mut errors = None;

    // A running scrub prints its progress ("0B repaired, 16.15% done"), not a
    // previous result — do not mine it for a "last scrub".
    if !scrub_running && !resilver_running {
        // "scrub repaired 0B in 09:12:33 with 0 errors on Sun Aug 10 09:36:34 2026"
        if let Some(idx) = scan.find("repaired") {
            let tail = &scan[idx..];
            if let Some(on) = tail.rfind(" on ") {
                result = Some(tail[..on].trim().to_string());
                at = Some(tail[on + 4..].trim().to_string());
            } else {
                result = Some(tail.trim().to_string());
            }
        }
        if let Some(idx) = lower.find(" errors") {
            // the integer immediately before " errors"
            let head = &scan[..idx];
            if let Some(word) = head.split_whitespace().next_back() {
                errors = word.parse::<u64>().ok();
            }
        }
    }
    (scrub_running, resilver_running, result, at, errors)
}

/// Sum error counters over **leaf** devices only.
///
/// `zpool status` reports every vdev row as the aggregate of its children, and
/// the pool row as the aggregate of everything. Naively summing the column
/// would count the same errors two or three times, so only rows with no
/// children (the next row is not deeper) and depth > 0 are counted.
fn sum_leaf_errors(devices: &[ZfsDevice]) -> (u64, u64, u64) {
    let mut totals = (0u64, 0u64, 0u64);
    for (i, d) in devices.iter().enumerate() {
        if d.depth == 0 {
            continue; // the pool row
        }
        let has_children = devices.get(i + 1).map(|n| n.depth > d.depth).unwrap_or(false);
        if has_children {
            continue;
        }
        totals.0 += d.read_errors;
        totals.1 += d.write_errors;
        totals.2 += d.cksum_errors;
    }
    totals
}

/// Merge the `zpool list` rows with the `zpool status` blocks.
pub fn build_pools(list_raw: &str, status_raw: &str) -> Vec<ZfsPool> {
    let rows = parse_zpool_list(list_raw);
    let blocks = parse_zpool_status(status_raw);

    // Union of both sources, keyed by pool name, list order first.
    let mut names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
    for b in &blocks {
        if !names.contains(&b.name) {
            names.push(b.name.clone());
        }
    }

    names
        .into_iter()
        .map(|name| {
            let row = rows.iter().find(|r| r.name == name);
            let block = blocks.iter().find(|b| b.name == name);

            // `zpool status` state wins when present; it is the live state and
            // is where SUSPENDED shows up first.
            let health = block
                .map(|b| b.state.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| row.map(|r| r.health.clone()))
                .unwrap_or_default();
            let health_upper = health.to_ascii_uppercase();

            let (scrub_in_progress, resilver_in_progress, last_scrub_result, last_scrub_at, errs) =
                block
                    .and_then(|b| b.scan_line.as_deref())
                    .map(parse_scan)
                    .unwrap_or((false, false, None, None, None));

            let devices = block.map(|b| b.devices.clone()).unwrap_or_default();
            let (total_read, total_write, total_cksum) = sum_leaf_errors(&devices);

            ZfsPool {
                name,
                suspended: health_upper == "SUSPENDED",
                degraded: health_upper == "DEGRADED",
                faulted: health_upper == "FAULTED" || health_upper == "UNAVAIL",
                health,
                size_bytes: row.and_then(|r| r.size_bytes),
                allocated_bytes: row.and_then(|r| r.allocated_bytes),
                free_bytes: row.and_then(|r| r.free_bytes),
                capacity_pct: row.and_then(|r| r.capacity_pct),
                fragmentation_pct: row.and_then(|r| r.fragmentation_pct),
                scrub_in_progress,
                resilver_in_progress,
                scan_line: block.and_then(|b| b.scan_line.clone()),
                last_scrub_at,
                last_scrub_result,
                last_scrub_errors: errs,
                status_detail: block.and_then(|b| b.status_detail.clone()),
                action: block.and_then(|b| b.action.clone()),
                errors_line: block.and_then(|b| b.errors_line.clone()),
                devices,
                total_read_errors: total_read,
                total_write_errors: total_write,
                total_cksum_errors: total_cksum,
            }
        })
        .collect()
}

fn finish(pools: Vec<ZfsPool>, summary: Option<String>, error: Option<String>) -> ZfsResult {
    ZfsResult {
        zfs_available: true,
        any_suspended: pools.iter().any(|p| p.suspended),
        any_degraded: pools.iter().any(|p| p.degraded || p.faulted),
        pools_with_errors: pools
            .iter()
            .filter(|p| {
                p.total_read_errors > 0 || p.total_write_errors > 0 || p.total_cksum_errors > 0
            })
            .map(|p| p.name.clone())
            .collect(),
        status_summary: summary,
        pools,
        error,
    }
}

// ---- collection ------------------------------------------------------------

/// Read pool health. **Read-only** — see the module docs.
///
/// Never returns `Err` for "zfs not installed"; that is `zfs_available: false`.
/// A hung `zpool status` (the suspended-pool case) comes back as a partial
/// result with `error` set, never as a hang.
pub async fn collect(timeout: Option<Duration>) -> ToolResult<ZfsResult> {
    let timeout = timeout.unwrap_or(exec::clamp_timeout(0));

    let list = match exec::run("zpool", ZPOOL_LIST_ARGS, timeout).await {
        Ok(out) => out.stdout,
        Err(e) if e.is_not_found() => {
            return Ok(ZfsResult::unavailable(Some(
                "zpool is not installed (no ZFS on this host)".to_string(),
            )));
        }
        // A timeout here already means something is very wrong (a suspended
        // pool can block even `zpool list`). Report it; do not hang.
        Err(e) => return Ok(finish(Vec::new(), None, Some(e.to_string()))),
    };

    let mut error = None;
    let status = match exec::run("zpool", ZPOOL_STATUS_ARGS, timeout).await {
        Ok(out) => out.stdout,
        Err(e) => {
            error = Some(format!(
                "zpool status unavailable ({e}) — pool details are partial; \
                 a hang here is itself a symptom of a suspended pool"
            ));
            String::new()
        }
    };

    let summary = match exec::run("zpool", ZPOOL_STATUS_X_ARGS, timeout).await {
        Ok(out) => Some(out.stdout.trim().to_string()).filter(|s| !s.is_empty()),
        Err(_) => None,
    };

    Ok(finish(build_pools(&list, &status), summary, error))
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_PARSABLE: &str = "tank\t10000831348736\t5931641634816\t4069189713920\tDEGRADED\t5\t59\n\
scratch\t1000204886016\t120259084288\t879945801728\tONLINE\t2\t12\n";

    const LIST_HUMAN: &str = "tank\t9.09T\t5.39T\t3.70T\tSUSPENDED\t5%\t59%\n";

    /// bigboy's tank: the REDACTED_HARDWARE_ID leg of mirror-0 throwing checksum errors
    /// while the pool still says it has no known data errors.
    const STATUS_DEGRADED: &str = r#"
  pool: tank
 state: DEGRADED
status: One or more devices has experienced an unrecoverable error.  An
	attempt was made to correct the error.  Applications are unaffected.
action: Determine if the device needs to be replaced, and clear the errors
	using 'zpool clear' or replace the device with 'zpool replace'.
   see: https://openzfs.github.io/openzfs-docs/msg/ZFS-8000-9P
  scan: scrub repaired 0B in 09:12:33 with 0 errors on Sun Aug 10 09:36:34 2026
config:

	NAME                                     STATE     READ WRITE CKSUM
	tank                                     DEGRADED     0     0     0
	  mirror-0                               DEGRADED     0     0 2.78K
	    /dev/disk/by-id/ata-ST10000NM0086_REDACTED_HARDWARE_ID  ONLINE       0     0 2.78K
	    /dev/disk/by-id/ata-HGST_HUH721414ALE600_REDACTED_HARDWARE_ID  ONLINE       0     0     0

errors: No known data errors

  pool: scratch
 state: ONLINE
  scan: scrub repaired 0B in 00:41:02 with 0 errors on Sun Aug  3 01:05:11 2026
config:

	NAME        STATE     READ WRITE CKSUM
	scratch     ONLINE       0     0     0
	  sdd       ONLINE       0     0     0

errors: No known data errors
"#;

    /// The state that wedged apt for 14h51m.
    const STATUS_SUSPENDED: &str = r#"
  pool: tank
 state: SUSPENDED
status: One or more devices are faulted in response to IO failures.
action: Make sure the affected devices are connected, then run 'zpool clear'.
   see: https://openzfs.github.io/openzfs-docs/msg/ZFS-8000-HC
  scan: scrub in progress since Tue Aug 12 03:00:01 2026
	1.21T / 5.39T scanned at 214M/s, 892G / 5.39T issued at 158M/s
	0B repaired, 16.15% done, 08:12:44 to go
config:

	NAME                     STATE     READ WRITE CKSUM
	tank                     SUSPENDED    0     0     0
	  mirror-0               DEGRADED    12   408     0
	    /dev/sdc             FAULTED     12   408     0  too many errors
	    /dev/sdd             ONLINE       0     0     0

errors: 1 data errors, use '-v' for a list
"#;

    // ---- the safety invariant ---------------------------------------------

    /// The tool must never be able to invoke a mutating or blocking zpool
    /// subcommand. `zpool scrub -w` against a suspended pool is what put a
    /// process into unkillable D-state and stalled apt for 14h51m.
    #[test]
    fn zfs_argv_is_read_only() {
        for argv in [ZPOOL_LIST_ARGS, ZPOOL_STATUS_ARGS, ZPOOL_STATUS_X_ARGS] {
            assert!(
                matches!(argv[0], "list" | "status"),
                "only list/status subcommands are allowed, found {:?}",
                argv[0]
            );
            for token in argv {
                assert!(
                    !FORBIDDEN_TOKENS.contains(token),
                    "forbidden token {token:?} in argv {argv:?} — see the module docs"
                );
            }
            assert!(
                !argv.contains(&"-w"),
                "-w (wait) must never be passed: it is the D-state trap"
            );
        }
    }

    // ---- parsing -----------------------------------------------------------

    #[test]
    fn zpool_list_parses_exact_bytes() {
        let rows = parse_zpool_list(LIST_PARSABLE);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "tank");
        assert_eq!(rows[0].health, "DEGRADED");
        assert_eq!(rows[0].size_bytes, Some(10_000_831_348_736));
        assert_eq!(rows[0].allocated_bytes, Some(5_931_641_634_816));
        assert_eq!(rows[0].capacity_pct, Some(59));
        assert_eq!(rows[0].fragmentation_pct, Some(5));
        assert_eq!(rows[1].name, "scratch");
    }

    #[test]
    fn zpool_list_parses_human_sizes_and_percent_suffixes() {
        let rows = parse_zpool_list(LIST_HUMAN);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].health, "SUSPENDED");
        assert_eq!(rows[0].capacity_pct, Some(59), "59% -> 59");
        assert_eq!(rows[0].fragmentation_pct, Some(5));
        assert_eq!(rows[0].size_bytes, Some((9.09 * 1024f64.powi(4)) as u64));
    }

    #[test]
    fn status_blocks_split_per_pool_with_multiline_stanzas() {
        let blocks = parse_zpool_status(STATUS_DEGRADED);
        assert_eq!(blocks.len(), 2, "two pools");
        assert_eq!(blocks[0].name, "tank");
        assert_eq!(blocks[0].state, "DEGRADED");
        let detail = blocks[0].status_detail.as_deref().unwrap();
        assert!(
            detail.contains("unrecoverable error") && detail.contains("Applications are unaffected"),
            "the wrapped continuation line is folded in: {detail}"
        );
        assert_eq!(blocks[0].errors_line.as_deref(), Some("No known data errors"));
        assert_eq!(blocks[1].name, "scratch");
    }

    #[test]
    fn config_rows_carry_depth_and_abbreviated_error_counts() {
        let pools = build_pools(LIST_PARSABLE, STATUS_DEGRADED);
        let tank = &pools[0];
        assert_eq!(tank.devices.len(), 4, "pool + mirror-0 + two leaves");
        assert_eq!(tank.devices[0].name, "tank");
        assert_eq!(tank.devices[0].depth, 0);
        assert_eq!(tank.devices[1].name, "mirror-0");
        assert_eq!(tank.devices[1].depth, 1);
        assert_eq!(
            tank.devices[1].cksum_errors, 2780,
            "\"2.78K\" is an abbreviated count, not a string"
        );

        let bad = tank
            .devices
            .iter()
            .find(|d| d.name.contains("REDACTED_HARDWARE_ID"))
            .expect("the REDACTED_HARDWARE_ID leaf");
        assert_eq!(bad.depth, 2);
        assert_eq!(bad.state, "ONLINE", "the leaf still says ONLINE");
        assert_eq!(bad.cksum_errors, 2780);

        // Leaf-only summation: neither the pool row nor the mirror-0 row (both
        // of which aggregate their children) may be counted again.
        assert_eq!(
            tank.total_cksum_errors, 2780,
            "2.78K counted once, from the REDACTED_HARDWARE_ID leaf only"
        );
        assert_eq!(tank.total_read_errors, 0);
        assert!(pools.iter().any(|p| p.name == "tank"));
    }

    /// The 14h51m incident, encoded as an assertion: SUSPENDED must surface as
    /// its own boolean and must NOT be conflated with DEGRADED.
    #[test]
    fn suspended_is_its_own_flag_distinct_from_degraded() {
        let pools = build_pools(LIST_HUMAN, STATUS_SUSPENDED);
        let tank = pools.iter().find(|p| p.name == "tank").expect("tank");

        assert!(tank.suspended, "SUSPENDED must be surfaced explicitly");
        assert!(
            !tank.degraded,
            "SUSPENDED is not DEGRADED — a degraded pool still serves I/O"
        );
        assert_eq!(tank.health, "SUSPENDED");

        let res = finish(pools, None, None);
        assert!(res.any_suspended, "top-level alarm bit is set");
        assert!(!res.any_degraded);
        assert_eq!(res.pools_with_errors, vec!["tank".to_string()]);
    }

    #[test]
    fn a_degraded_pool_is_not_reported_as_suspended() {
        let pools = build_pools(LIST_PARSABLE, STATUS_DEGRADED);
        let tank = pools.iter().find(|p| p.name == "tank").unwrap();
        assert!(tank.degraded);
        assert!(!tank.suspended, "the two states must not be conflated");
    }

    #[test]
    fn scrub_in_progress_is_detected_and_last_scrub_is_parsed() {
        let running = build_pools(LIST_HUMAN, STATUS_SUSPENDED);
        let tank = &running[0];
        assert!(tank.scrub_in_progress);
        assert!(!tank.resilver_in_progress);
        assert!(
            tank.scan_line.as_deref().unwrap().contains("16.15% done"),
            "the wrapped scan stanza is captured whole: {:?}",
            tank.scan_line
        );

        let done = build_pools(LIST_PARSABLE, STATUS_DEGRADED);
        let tank = &done[0];
        assert!(!tank.scrub_in_progress);
        assert_eq!(
            tank.last_scrub_at.as_deref(),
            Some("Sun Aug 10 09:36:34 2026")
        );
        assert_eq!(
            tank.last_scrub_result.as_deref(),
            Some("repaired 0B in 09:12:33 with 0 errors")
        );
        assert_eq!(tank.last_scrub_errors, Some(0));
    }

    #[test]
    fn pool_present_in_status_but_missing_from_list_is_still_reported() {
        // A suspended pool can vanish from `zpool list` output entirely.
        let pools = build_pools("", STATUS_SUSPENDED);
        assert_eq!(pools.len(), 1);
        assert!(pools[0].suspended);
        assert_eq!(pools[0].size_bytes, None, "no list row, no sizes");
    }

    #[test]
    fn garbage_and_empty_input_never_panic() {
        assert!(parse_zpool_list("").is_empty());
        assert!(parse_zpool_status("").is_empty());
        assert!(build_pools("nonsense", "also nonsense").is_empty());
        assert_eq!(parse_size_field("-"), None);
        assert_eq!(parse_error_count("bogus"), None);
    }

    #[tokio::test]
    async fn missing_zfs_reports_unavailable_not_an_error() {
        // On a host without zpool (every dev mac), this is the real path.
        let res = collect(Some(Duration::from_secs(5)))
            .await
            .expect("never errors");
        if !res.zfs_available {
            assert!(res.pools.is_empty());
            assert!(res.error.is_some());
        }
    }
}
