// SPDX-License-Identifier: Apache-2.0

//! `disk.smart` — per-physical-disk SMART **raw counters**.
//!
//! # The rule this module exists to enforce
//!
//! Seagate ST10000NM0086 serial `REDACTED_HARDWARE_ID`, sitting in a live ZFS mirror on
//! bigboy, carried **2,776 `Current_Pending_Sector`** and **2,776
//! `Offline_Uncorrectable`** while `smartctl -H` still reported **PASSED** — no
//! attribute had crossed its vendor threshold, so the overall verdict stayed
//! green while the disk was actively losing sectors.
//!
//! Therefore: **this module reads RAW attribute values and never branches on
//! the overall PASSED/FAILED verdict.** The verdict is carried through as
//! `smart_overall_passed` so a human can see the contradiction, and is
//! deliberately *not* an input to [`classify`]. A probe that trusts the verdict
//! is worse than no probe, because it manufactures confidence.
//!
//! The raw numbers are emitted alongside the computed `status` so a drift
//! engine can assert on `disks[].pending_sectors` directly rather than trusting
//! our thresholds.
//!
//! # Read-only
//!
//! Only `smartctl --scan`, `smartctl -i -H -A` and `lsblk` are invoked. No
//! self-test (`-t`), no SET command, no `--set`. Every invocation is bounded by
//! [`crate::exec`].

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::error::ToolResult;
use crate::exec;

/// Computed severity. `UNKNOWN` means we could not read the disk at all
/// (usually: smartctl needs root) — it is explicitly *not* "OK".
pub const STATUS_OK: &str = "OK";
pub const STATUS_WARN: &str = "WARN";
pub const STATUS_CRIT: &str = "CRIT";
pub const STATUS_UNKNOWN: &str = "UNKNOWN";

/// NVMe wear percentage above which we warn.
const NVME_WEAR_WARN_PCT: u64 = 85;

// ---- result types (plain serde, NOT proto) ---------------------------------

/// Raw SMART readings for one device, independent of how they were obtained
/// (JSON or the text fallback). Every field is `Option` because different
/// device classes expose different subsets — absence is not zero.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SmartReadings {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size_bytes: Option<u64>,
    /// 0 means solid state; >0 is the spindle speed.
    pub rotation_rate: Option<u64>,
    /// "ATA" | "NVMe" | "SCSI" as smartctl reports it.
    pub protocol: Option<String>,

    // --- SATA / SAS raw attribute values -----------------------------------
    /// Attr 5.
    pub reallocated_sectors: Option<u64>,
    /// Attr 197 — the REDACTED_HARDWARE_ID counter.
    pub pending_sectors: Option<u64>,
    /// Attr 198 — the other REDACTED_HARDWARE_ID counter.
    pub offline_uncorrectable: Option<u64>,
    /// Attr 199.
    pub crc_errors: Option<u64>,
    /// Attr 9.
    pub power_on_hours: Option<u64>,

    // --- NVMe health log ----------------------------------------------------
    pub nvme_percentage_used: Option<u64>,
    pub nvme_media_errors: Option<u64>,
    pub nvme_data_units_written: Option<u64>,
    pub nvme_critical_warning: Option<u64>,

    /// The overall `smartctl -H` verdict. **Reported, never branched on.**
    /// REDACTED_HARDWARE_ID was `true` here with 2,776 pending sectors.
    pub smart_overall_passed: Option<bool>,
}

/// One physical disk in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmartDisk {
    pub device: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: Option<u64>,
    /// True for spinning rust, false for SSD/NVMe, None when unknown.
    pub rotational: Option<bool>,
    /// "hdd" | "ssd" | "nvme" | "unknown".
    pub kind: String,
    /// lsblk TRAN: "sata" | "nvme" | "sas" | "usb" …
    pub transport: Option<String>,

    pub reallocated_sectors: Option<u64>,
    pub pending_sectors: Option<u64>,
    pub offline_uncorrectable: Option<u64>,
    pub crc_errors: Option<u64>,
    pub power_on_hours: Option<u64>,

    pub nvme_percentage_used: Option<u64>,
    pub nvme_media_errors: Option<u64>,
    pub nvme_data_units_written: Option<u64>,
    pub nvme_critical_warning: Option<u64>,

    pub smart_overall_passed: Option<bool>,

    /// "OK" | "WARN" | "CRIT" | "UNKNOWN" — computed from RAW counters only.
    pub status: String,
    /// Human-readable justifications for `status`, one per triggering counter.
    pub reasons: Vec<String>,
    /// Per-disk failure (timeout, permission denied, unparseable output).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmartResult {
    /// False when smartctl is not installed. The result is still well-formed.
    pub smartctl_available: bool,
    pub disks: Vec<SmartDisk>,
    pub crit_count: u32,
    pub warn_count: u32,
    /// Top-level failure (enumeration timed out, platform unsupported…).
    pub error: Option<String>,
}

impl SmartResult {
    fn unavailable(error: Option<String>) -> Self {
        Self {
            smartctl_available: false,
            disks: Vec::new(),
            crit_count: 0,
            warn_count: 0,
            error,
        }
    }
}

/// Options for [`collect`].
#[derive(Debug, Clone, Default)]
pub struct SmartOptions {
    /// Explicit device list; empty means "enumerate".
    pub devices: Vec<String>,
    pub timeout: Option<Duration>,
}

// ---- classification --------------------------------------------------------

/// Compute severity **from raw counters only**.
///
/// * `CRIT` — pending sectors > 0, offline-uncorrectable > 0, or an NVMe
///   `critical_warning` bitfield that is non-zero.
/// * `WARN` — reallocated sectors > 0, UDMA CRC errors > 0, or NVMe wear
///   above [`NVME_WEAR_WARN_PCT`].
/// * `OK` — everything readable and clean.
/// * `UNKNOWN` — nothing readable at all.
///
/// `smart_overall_passed` is never consulted. That is the entire point.
pub fn classify(r: &SmartReadings) -> (String, Vec<String>) {
    let mut reasons = Vec::new();
    let mut crit = false;
    let mut warn = false;

    if let Some(n) = r.pending_sectors.filter(|n| *n > 0) {
        crit = true;
        reasons.push(format!("{n} pending sectors (attr 197)"));
    }
    if let Some(n) = r.offline_uncorrectable.filter(|n| *n > 0) {
        crit = true;
        reasons.push(format!("{n} offline-uncorrectable sectors (attr 198)"));
    }
    if let Some(n) = r.nvme_critical_warning.filter(|n| *n != 0) {
        crit = true;
        reasons.push(format!("nvme critical_warning = 0x{n:x}"));
    }
    if let Some(n) = r.reallocated_sectors.filter(|n| *n > 0) {
        warn = true;
        reasons.push(format!("{n} reallocated sectors (attr 5)"));
    }
    if let Some(n) = r.crc_errors.filter(|n| *n > 0) {
        warn = true;
        reasons.push(format!("{n} UDMA CRC errors (attr 199) — check the cable"));
    }
    if let Some(n) = r.nvme_media_errors.filter(|n| *n > 0) {
        warn = true;
        reasons.push(format!("{n} nvme media errors"));
    }
    if let Some(n) = r.nvme_percentage_used.filter(|n| *n > NVME_WEAR_WARN_PCT) {
        warn = true;
        reasons.push(format!("nvme wear at {n}% of rated endurance"));
    }

    // Note the contradiction explicitly when the vendor verdict disagrees with
    // the raw counters — this is the REDACTED_HARDWARE_ID signature.
    if crit && r.smart_overall_passed == Some(true) {
        reasons.push(
            "smartctl overall-health says PASSED despite the raw counters above; \
             the verdict is not used to compute this status"
                .to_string(),
        );
    }

    let status = if crit {
        STATUS_CRIT
    } else if warn {
        STATUS_WARN
    } else if is_blank(r) {
        STATUS_UNKNOWN
    } else {
        STATUS_OK
    };
    (status.to_string(), reasons)
}

/// True when we obtained no health counter at all (so "OK" would be a lie).
fn is_blank(r: &SmartReadings) -> bool {
    r.reallocated_sectors.is_none()
        && r.pending_sectors.is_none()
        && r.offline_uncorrectable.is_none()
        && r.crc_errors.is_none()
        && r.power_on_hours.is_none()
        && r.nvme_percentage_used.is_none()
        && r.nvme_media_errors.is_none()
        && r.nvme_critical_warning.is_none()
}

// ---- smartctl JSON parsing -------------------------------------------------

/// Known SMART attribute ids we care about.
const ATTR_REALLOCATED: u64 = 5;
const ATTR_POWER_ON_HOURS: u64 = 9;
const ATTR_PENDING: u64 = 197;
const ATTR_OFFLINE_UNCORRECTABLE: u64 = 198;
const ATTR_CRC: u64 = 199;

/// Map an attribute (id first, vendor name as fallback) onto one of our slots.
///
/// Vendors rename attributes freely (`Current_Pending_Sector` vs
/// `Current_Pending_Sector_Count`, `Offline_Uncorrectable` vs
/// `Uncorrectable_Sector_Ct`), so the id is authoritative and the name only
/// resolves ids we don't recognise.
fn attr_slot(id: u64, name: &str) -> Option<u64> {
    match id {
        ATTR_REALLOCATED | ATTR_POWER_ON_HOURS | ATTR_PENDING | ATTR_OFFLINE_UNCORRECTABLE
        | ATTR_CRC => return Some(id),
        _ => {}
    }
    let n = name.to_ascii_lowercase();
    if n.contains("current_pending") {
        Some(ATTR_PENDING)
    } else if n.contains("offline_uncorrectable") || n.contains("uncorrectable_sector") {
        Some(ATTR_OFFLINE_UNCORRECTABLE)
    } else if n.contains("reallocated_sector") {
        Some(ATTR_REALLOCATED)
    } else if n.contains("crc_error") {
        Some(ATTR_CRC)
    } else if n.contains("power_on_hours") {
        Some(ATTR_POWER_ON_HOURS)
    } else {
        None
    }
}

fn assign(r: &mut SmartReadings, slot: u64, value: u64) {
    match slot {
        ATTR_REALLOCATED => r.reallocated_sectors = Some(value),
        ATTR_POWER_ON_HOURS => r.power_on_hours = Some(value),
        ATTR_PENDING => r.pending_sectors = Some(value),
        ATTR_OFFLINE_UNCORRECTABLE => r.offline_uncorrectable = Some(value),
        ATTR_CRC => r.crc_errors = Some(value),
        _ => {}
    }
}

fn as_u64(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => leading_u64(s),
        _ => None,
    }
}

fn as_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Parse the leading integer of a raw-value string like `"2776"` or
/// `"30 (Min/Max 20/45)"` or `"41821h+12m"`.
fn leading_u64(s: &str) -> Option<u64> {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Parse `smartctl -i -H -A -j` output. Returns `None` when the payload is not
/// JSON at all (old smartctl without `-j`), which triggers the text fallback.
pub fn parse_smartctl_json(raw: &str) -> Option<SmartReadings> {
    let v: Value = serde_json::from_str(raw.trim()).ok()?;
    let mut r = SmartReadings {
        model: as_string(v.get("model_name")).or_else(|| as_string(v.get("device_model"))),
        serial: as_string(v.get("serial_number")),
        size_bytes: as_u64(v.get("user_capacity").and_then(|c| c.get("bytes"))),
        rotation_rate: as_u64(v.get("rotation_rate")),
        protocol: as_string(v.get("device").and_then(|d| d.get("protocol"))),
        // Recorded for humans. NOT an input to classify().
        smart_overall_passed: v
            .get("smart_status")
            .and_then(|s| s.get("passed"))
            .and_then(Value::as_bool),
        ..Default::default()
    };

    if let Some(table) = v
        .get("ata_smart_attributes")
        .and_then(|a| a.get("table"))
        .and_then(Value::as_array)
    {
        for entry in table {
            let id = as_u64(entry.get("id")).unwrap_or(0);
            let name = as_string(entry.get("name")).unwrap_or_default();
            let Some(slot) = attr_slot(id, &name) else {
                continue;
            };
            // RAW value — never `value`/`worst`/`thresh`, which are the
            // normalized numbers that kept REDACTED_HARDWARE_ID "PASSED".
            let raw = entry.get("raw");
            // Seagate (and others) pack minutes/seconds into the high bits of
            // the Power_On_Hours raw integer: REDACTED_HARDWARE_ID reports
            // raw.value = 8857098737736293 alongside
            // raw.string = "55909h+34m+22.204s". Taking the integer verbatim
            // printed power-on times of ~10^15 hours. For that attribute the
            // rendered string's leading number is the trustworthy one; for
            // every other attribute the integer stays authoritative.
            let val = if slot == ATTR_POWER_ON_HOURS {
                as_u64(raw.and_then(|r| r.get("string")))
                    .or_else(|| as_u64(raw.and_then(|r| r.get("value"))))
            } else {
                as_u64(raw.and_then(|r| r.get("value")))
                    .or_else(|| as_u64(raw.and_then(|r| r.get("string"))))
            };
            if let Some(val) = val {
                assign(&mut r, slot, val);
            }
        }
    }

    if let Some(log) = v.get("nvme_smart_health_information_log") {
        r.nvme_percentage_used = as_u64(log.get("percentage_used"));
        r.nvme_media_errors = as_u64(log.get("media_errors"));
        r.nvme_data_units_written = as_u64(log.get("data_units_written"));
        r.nvme_critical_warning = as_u64(log.get("critical_warning"));
        if r.power_on_hours.is_none() {
            r.power_on_hours = as_u64(log.get("power_on_hours"));
        }
        if r.protocol.is_none() {
            r.protocol = Some("NVMe".to_string());
        }
    }

    Some(r)
}

/// Text fallback for smartctl builds without JSON support.
///
/// Handles the classic attribute table:
/// ```text
/// ID# ATTRIBUTE_NAME          FLAG     VALUE WORST THRESH TYPE      UPDATED  WHEN_FAILED RAW_VALUE
/// 197 Current_Pending_Sector  0x0012   100   100   000    Old_age   Always       -       2776
/// ```
/// plus the `Device Model:` / `Serial Number:` identity block and the
/// `SMART overall-health self-assessment test result: PASSED` line.
pub fn parse_smartctl_text(raw: &str) -> SmartReadings {
    let mut r = SmartReadings::default();
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Device Model:") {
            r.model = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Model Number:") {
            r.model = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Serial Number:") {
            r.serial = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Rotation Rate:") {
            let rest = rest.trim();
            r.rotation_rate = if rest.contains("Solid State") {
                Some(0)
            } else {
                leading_u64(rest)
            };
            continue;
        }
        if let Some(rest) =
            trimmed.strip_prefix("SMART overall-health self-assessment test result:")
        {
            // Recorded only. See the module docs.
            r.smart_overall_passed = Some(rest.trim().eq_ignore_ascii_case("PASSED"));
            continue;
        }

        // Attribute rows: "<id> <NAME> <flag> <val> <worst> <thresh> <type> <updated> <when> <raw…>"
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let Some(id) = fields[0].parse::<u64>().ok().filter(|n| *n > 0) else {
            continue;
        };
        let Some(slot) = attr_slot(id, fields[1]) else {
            continue;
        };
        // The raw value is field 9 and may carry a parenthetical suffix.
        if let Some(val) = leading_u64(fields[9]) {
            assign(&mut r, slot, val);
        }
    }
    r
}

// ---- device enumeration ----------------------------------------------------

/// One block device as reported by `lsblk`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BlockDevice {
    pub path: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size_bytes: Option<u64>,
    pub rotational: Option<bool>,
    pub transport: Option<String>,
}

/// Parse a human or exact size: `10000831348736`, `"10000831348736"`, `"9.1T"`.
pub fn parse_size(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_size_str(s),
        _ => None,
    }
}

fn parse_size_str(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "-" {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let (num, suffix) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let num: f64 = num.trim().parse().ok()?;
    // ZFS and lsblk both use binary multiples.
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

fn parse_bool_ish(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_u64()? != 0),
        // lsblk historically emits "1"/"0" as strings.
        Value::String(s) => match s.trim() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Parse `lsblk -J -b -p -o NAME,SERIAL,MODEL,SIZE,ROTA,TRAN,TYPE`.
/// Only `type == "disk"` entries are returned (no partitions, no LVM, no zd*).
pub fn parse_lsblk_json(raw: &str) -> Vec<BlockDevice> {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return Vec::new();
    };
    let Some(list) = v.get("blockdevices").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter(|d| {
            as_string(d.get("type"))
                .map(|t| t.eq_ignore_ascii_case("disk"))
                .unwrap_or(false)
        })
        .filter_map(|d| {
            let name = as_string(d.get("name"))?;
            let path = if name.starts_with('/') {
                name
            } else {
                format!("/dev/{name}")
            };
            Some(BlockDevice {
                path,
                model: as_string(d.get("model")),
                serial: as_string(d.get("serial")),
                size_bytes: parse_size(d.get("size")),
                rotational: parse_bool_ish(d.get("rota")),
                transport: as_string(d.get("tran")),
            })
        })
        .collect()
}

/// A device from `smartctl --scan -j`: path plus the `-d` type smartctl wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScannedDevice {
    pub path: String,
    pub dev_type: Option<String>,
}

/// Parse `smartctl --scan -j` (`{"devices":[{"name":"/dev/sda","type":"sat"}]}`).
pub fn parse_smartctl_scan(raw: &str) -> Vec<ScannedDevice> {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return Vec::new();
    };
    v.get("devices")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| {
                    Some(ScannedDevice {
                        path: as_string(d.get("name"))?,
                        dev_type: as_string(d.get("type")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fold readings + lsblk identity into the emitted disk record.
pub fn build_disk(
    device: &str,
    readings: &SmartReadings,
    block: Option<&BlockDevice>,
    error: Option<String>,
) -> SmartDisk {
    let (status, reasons) = classify(readings);
    let rotational = readings
        .rotation_rate
        .map(|r| r > 0)
        .or_else(|| block.and_then(|b| b.rotational));
    let transport = block.and_then(|b| b.transport.clone());
    let is_nvme = readings
        .protocol
        .as_deref()
        .map(|p| p.eq_ignore_ascii_case("nvme"))
        .unwrap_or(false)
        || transport.as_deref() == Some("nvme")
        || device.contains("nvme");
    let kind = if is_nvme {
        "nvme"
    } else {
        match rotational {
            Some(true) => "hdd",
            Some(false) => "ssd",
            None => "unknown",
        }
    };

    SmartDisk {
        device: device.to_string(),
        model: readings
            .model
            .clone()
            .or_else(|| block.and_then(|b| b.model.clone()))
            .unwrap_or_default(),
        serial: readings
            .serial
            .clone()
            .or_else(|| block.and_then(|b| b.serial.clone()))
            .unwrap_or_default(),
        size_bytes: readings
            .size_bytes
            .or_else(|| block.and_then(|b| b.size_bytes)),
        rotational,
        kind: kind.to_string(),
        transport,
        reallocated_sectors: readings.reallocated_sectors,
        pending_sectors: readings.pending_sectors,
        offline_uncorrectable: readings.offline_uncorrectable,
        crc_errors: readings.crc_errors,
        power_on_hours: readings.power_on_hours,
        nvme_percentage_used: readings.nvme_percentage_used,
        nvme_media_errors: readings.nvme_media_errors,
        nvme_data_units_written: readings.nvme_data_units_written,
        nvme_critical_warning: readings.nvme_critical_warning,
        smart_overall_passed: readings.smart_overall_passed,
        status,
        reasons,
        error,
    }
}

// ---- collection ------------------------------------------------------------

/// Collect SMART raw counters for every physical disk on this host.
///
/// Never returns `Err` for the ordinary "tool missing / no permission" cases —
/// those come back as `smartctl_available: false` or a per-disk `error`.
pub async fn collect(opts: SmartOptions) -> ToolResult<SmartResult> {
    // SMART is read through smartctl + lsblk, both Linux-fleet tools. On macOS
    // we report cleanly rather than shelling out to something that isn't there.
    if !cfg!(target_os = "linux") {
        return Ok(SmartResult::unavailable(Some(format!(
            "disk.smart is Linux-only (host is {})",
            std::env::consts::OS
        ))));
    }

    let timeout = opts.timeout.unwrap_or(exec::clamp_timeout(0));

    // lsblk identity (best-effort; a missing lsblk is not fatal).
    let blocks: BTreeMap<String, BlockDevice> = match exec::run(
        "lsblk",
        &["-J", "-b", "-p", "-o", "NAME,SERIAL,MODEL,SIZE,ROTA,TRAN,TYPE"],
        timeout,
    )
    .await
    {
        Ok(out) => parse_lsblk_json(&out.stdout)
            .into_iter()
            .map(|b| (b.path.clone(), b))
            .collect(),
        Err(_) => BTreeMap::new(),
    };

    // Device list: explicit > smartctl --scan > lsblk.
    let scanned: Vec<ScannedDevice> = if !opts.devices.is_empty() {
        opts.devices
            .iter()
            .map(|d| ScannedDevice {
                path: d.clone(),
                dev_type: None,
            })
            .collect()
    } else {
        match exec::run("smartctl", &["--scan", "-j"], timeout).await {
            Ok(out) => {
                let mut found = parse_smartctl_scan(&out.stdout);
                if found.is_empty() {
                    found = blocks
                        .keys()
                        .map(|p| ScannedDevice {
                            path: p.clone(),
                            dev_type: None,
                        })
                        .collect();
                }
                found
            }
            Err(e) if e.is_not_found() => {
                return Ok(SmartResult::unavailable(Some(
                    "smartctl is not installed (apt install smartmontools)".to_string(),
                )));
            }
            Err(e) => {
                return Ok(SmartResult {
                    smartctl_available: true,
                    disks: Vec::new(),
                    crit_count: 0,
                    warn_count: 0,
                    error: Some(format!("device enumeration failed: {e}")),
                });
            }
        }
    };

    let mut disks = Vec::with_capacity(scanned.len());
    for dev in &scanned {
        disks.push(read_one(dev, blocks.get(&dev.path), timeout).await);
    }

    let crit_count = disks.iter().filter(|d| d.status == STATUS_CRIT).count() as u32;
    let warn_count = disks.iter().filter(|d| d.status == STATUS_WARN).count() as u32;

    Ok(SmartResult {
        smartctl_available: true,
        disks,
        crit_count,
        warn_count,
        error: None,
    })
}

/// One `smartctl -i -H -A [-d TYPE] -j` attempt. `None` means the output was
/// not parseable as smartctl JSON at all (no JSON support in this build).
async fn read_with_type(
    dev: &ScannedDevice,
    dev_type: Option<&str>,
    timeout: Duration,
) -> Option<SmartReadings> {
    let mut args: Vec<&str> = vec!["-i", "-H", "-A"];
    if let Some(t) = dev_type {
        args.push("-d");
        args.push(t);
    }
    args.push("-j");
    args.push(&dev.path);
    match exec::run("smartctl", &args, timeout).await {
        Ok(out) => parse_smartctl_json(&out.stdout),
        Err(_) => None,
    }
}

/// Read one device: JSON first, text fallback second.
async fn read_one(dev: &ScannedDevice, block: Option<&BlockDevice>, timeout: Duration) -> SmartDisk {
    // Try smartctl's own auto-detection first, and only then the type that
    // `--scan` suggested.
    //
    // On any host where SATA disks hang off a SAS HBA or expander, `--scan`
    // reports `type=scsi`. Passing `-d scsi` makes smartctl read SCSI mode
    // pages, and `ata_smart_attributes` disappears completely -- so every raw
    // counter we care about (pending, uncorrectable, reallocated, CRC) comes
    // back null and the disk reports UNKNOWN. Auto-detect resolves the SAT
    // translation and returns the real ATA attributes. Verified on rackboy1,
    // where all four SSDs scan as `scsi`; bigboy's pool disks are likewise
    // expander-attached.
    let mut readings = read_with_type(dev, None, timeout).await;
    if readings.as_ref().map(is_blank).unwrap_or(true) {
        if let Some(t) = dev.dev_type.as_deref() {
            if let Some(r) = read_with_type(dev, Some(t), timeout).await {
                if !is_blank(&r) {
                    readings = Some(r);
                }
            }
        }
    }
    if let Some(r) = readings {
        if !is_blank(&r) {
            return build_disk(&dev.path, &r, block, None);
        }
    }

    // Both attempts were blank: fall through to the original path so the
    // failure hint and text-mode fallback still apply.
    let mut args: Vec<&str> = vec!["-i", "-H", "-A"];
    if let Some(t) = dev.dev_type.as_deref() {
        args.push("-d");
        args.push(t);
    }
    let mut json_args = args.clone();
    json_args.push("-j");
    json_args.push(&dev.path);

    match exec::run("smartctl", &json_args, timeout).await {
        Ok(out) => {
            // smartctl exits non-zero for informational bits too, so parse
            // regardless of exit status and only fall back on unusable output.
            if let Some(readings) = parse_smartctl_json(&out.stdout) {
                let err = (is_blank(&readings) && !out.success)
                    .then(|| smartctl_failure_hint(&out.stdout, &out.stderr));
                return build_disk(&dev.path, &readings, block, err);
            }
            // No JSON support — retry in text mode.
            let mut text_args = args.clone();
            text_args.push(&dev.path);
            match exec::run("smartctl", &text_args, timeout).await {
                Ok(t) => {
                    let readings = parse_smartctl_text(&t.stdout);
                    let err = (is_blank(&readings))
                        .then(|| smartctl_failure_hint(&t.stdout, &t.stderr));
                    build_disk(&dev.path, &readings, block, err)
                }
                Err(e) => build_disk(
                    &dev.path,
                    &SmartReadings::default(),
                    block,
                    Some(e.to_string()),
                ),
            }
        }
        Err(e) => build_disk(
            &dev.path,
            &SmartReadings::default(),
            block,
            Some(e.to_string()),
        ),
    }
}

/// Turn smartctl's chatter into one actionable line (usually: needs root).
fn smartctl_failure_hint(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    for line in combined.lines() {
        let l = line.trim();
        if l.contains("Permission denied")
            || l.contains("requires root")
            || l.contains("Operation not permitted")
        {
            return "smartctl could not open the device (needs root)".to_string();
        }
        if l.starts_with("Smartctl: ") || l.contains("Unable to detect device type") {
            return l.to_string();
        }
    }
    "smartctl returned no usable attribute data".to_string()
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The real bigboy disk: Seagate ST10000NM0086 serial REDACTED_HARDWARE_ID, in a live
    /// ZFS mirror, with 2,776 pending + 2,776 offline-uncorrectable sectors and
    /// an overall-health verdict of **PASSED**.
    const REDACTED_HARDWARE_ID_JSON: &str = r#"
{
  "json_format_version": [1, 0],
  "smartctl": { "version": [7, 2], "exit_status": 0 },
  "device": { "name": "/dev/sdc", "info_name": "/dev/sdc [SAT]", "type": "sat", "protocol": "ATA" },
  "model_family": "Seagate Enterprise Capacity 3.5 HDD",
  "model_name": "ST10000NM0086-2AA101",
  "serial_number": "REDACTED_HARDWARE_ID",
  "user_capacity": { "blocks": 19532873728, "bytes": 10000831348736 },
  "rotation_rate": 7200,
  "smart_status": { "passed": true },
  "ata_smart_attributes": {
    "revision": 10,
    "table": [
      { "id": 1, "name": "Raw_Read_Error_Rate", "value": 83, "worst": 64, "thresh": 44,
        "when_failed": "", "flags": { "value": 15, "string": "POSR-- " },
        "raw": { "value": 214748364, "string": "214748364" } },
      { "id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 10,
        "when_failed": "", "flags": { "value": 51, "string": "PO--CK " },
        "raw": { "value": 0, "string": "0" } },
      { "id": 9, "name": "Power_On_Hours", "value": 60, "worst": 60, "thresh": 0,
        "when_failed": "", "flags": { "value": 50, "string": "-O--CK " },
        "raw": { "value": 35528, "string": "35528" } },
      { "id": 187, "name": "Reported_Uncorrect", "value": 46, "worst": 46, "thresh": 0,
        "when_failed": "", "flags": { "value": 50, "string": "-O--CK " },
        "raw": { "value": 54, "string": "54" } },
      { "id": 197, "name": "Current_Pending_Sector", "value": 100, "worst": 100, "thresh": 0,
        "when_failed": "", "flags": { "value": 18, "string": "-O--C- " },
        "raw": { "value": 2776, "string": "2776" } },
      { "id": 198, "name": "Offline_Uncorrectable", "value": 100, "worst": 100, "thresh": 0,
        "when_failed": "", "flags": { "value": 16, "string": "----C- " },
        "raw": { "value": 2776, "string": "2776" } },
      { "id": 199, "name": "UDMA_CRC_Error_Count", "value": 200, "worst": 200, "thresh": 0,
        "when_failed": "", "flags": { "value": 62, "string": "-OSRCK " },
        "raw": { "value": 0, "string": "0" } }
    ]
  }
}
"#;

    /// A genuinely healthy SATA SSD.
    const HEALTHY_SSD_JSON: &str = r#"
{
  "device": { "name": "/dev/sda", "type": "sat", "protocol": "ATA" },
  "model_name": "Samsung SSD 870 EVO 1TB",
  "serial_number": "REDACTED_HARDWARE_ID",
  "user_capacity": { "bytes": 1000204886016 },
  "rotation_rate": 0,
  "smart_status": { "passed": true },
  "ata_smart_attributes": { "table": [
    { "id": 5,   "name": "Reallocated_Sector_Ct",  "raw": { "value": 0, "string": "0" } },
    { "id": 9,   "name": "Power_On_Hours",          "raw": { "value": 9123, "string": "9123" } },
    { "id": 197, "name": "Current_Pending_Sector",  "raw": { "value": 0, "string": "0" } },
    { "id": 198, "name": "Offline_Uncorrectable",   "raw": { "value": 0, "string": "0" } },
    { "id": 199, "name": "UDMA_CRC_Error_Count",    "raw": { "value": 0, "string": "0" } }
  ] }
}
"#;

    const NVME_JSON: &str = r#"
{
  "device": { "name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe" },
  "model_name": "Samsung SSD 990 PRO 2TB",
  "serial_number": "REDACTED_HARDWARE_ID",
  "user_capacity": { "bytes": 2000398934016 },
  "smart_status": { "passed": true },
  "nvme_smart_health_information_log": {
    "critical_warning": 4,
    "temperature": 44,
    "available_spare": 100,
    "percentage_used": 91,
    "data_units_read": 118293011,
    "data_units_written": 233781004,
    "media_errors": 0,
    "power_on_hours": 14211
  }
}
"#;

    const SMARTCTL_TEXT: &str = r#"
smartctl 7.2 2020-12-30 r5155 [x86_64-linux-5.15.0-88-generic] (local build)
Copyright (C) 2002-20, Bruce Allen, Christian Franke, www.smartmontools.org

=== START OF INFORMATION SECTION ===
Device Model:     ST10000NM0086-2AA101
Serial Number:    REDACTED_HARDWARE_ID
LU WWN Device Id: 5 000c50 0a2b1c3d4
Firmware Version: SNB1
User Capacity:    10,000,831,348,736 bytes [10.0 TB]
Rotation Rate:    7200 rpm

=== START OF READ SMART DATA SECTION ===
SMART overall-health self-assessment test result: PASSED

SMART Attributes Data Structure revision number: 10
Vendor Specific SMART Attributes with Thresholds:
ID# ATTRIBUTE_NAME          FLAG     VALUE WORST THRESH TYPE      UPDATED  WHEN_FAILED RAW_VALUE
  1 Raw_Read_Error_Rate     0x000f   083   064   044    Pre-fail  Always       -       214748364
  5 Reallocated_Sector_Ct   0x0033   100   100   010    Pre-fail  Always       -       0
  9 Power_On_Hours          0x0032   060   060   000    Old_age   Always       -       35528
190 Airflow_Temperature_Cel 0x0022   065   050   045    Old_age   Always       -       35 (Min/Max 25/44)
197 Current_Pending_Sector  0x0012   100   100   000    Old_age   Always       -       2776
198 Offline_Uncorrectable   0x0010   100   100   000    Old_age   Offline      -       2776
199 UDMA_CRC_Error_Count    0x003e   200   200   000    Old_age   Always       -       0
"#;

    const LSBLK_JSON: &str = r#"
{
   "blockdevices": [
      {"name":"/dev/sda","serial":"REDACTED_HARDWARE_ID","model":"Samsung SSD 870 EVO 1TB","size":1000204886016,"rota":false,"tran":"sata","type":"disk"},
      {"name":"/dev/sda1","serial":null,"model":null,"size":1000203837440,"rota":false,"tran":"sata","type":"part"},
      {"name":"/dev/sdc","serial":"REDACTED_HARDWARE_ID","model":"ST10000NM0086-2AA101","size":10000831348736,"rota":true,"tran":"sata","type":"disk"},
      {"name":"/dev/nvme0n1","serial":"REDACTED_HARDWARE_ID","model":"Samsung SSD 990 PRO 2TB","size":2000398934016,"rota":false,"tran":"nvme","type":"disk"},
      {"name":"/dev/zd0","serial":null,"model":null,"size":8589934592,"rota":false,"tran":null,"type":"disk"}
   ]
}
"#;

    /// Older lsblk emits every column as a string, including booleans.
    const LSBLK_JSON_STRINGY: &str = r#"
{
   "blockdevices": [
      {"name":"sdb","serial":"REDACTED_HARDWARE_ID","model":"HGST HUH721414ALE600","size":"12.7T","rota":"1","tran":"sata","type":"disk"}
   ]
}
"#;

    const SCAN_JSON: &str = r#"
{
  "json_format_version": [1,0],
  "devices": [
    {"name":"/dev/sda","info_name":"/dev/sda","type":"sat","protocol":"ATA"},
    {"name":"/dev/sdc","info_name":"/dev/sdc","type":"sat","protocol":"ATA"},
    {"name":"/dev/nvme0","info_name":"/dev/nvme0","type":"nvme","protocol":"NVMe"}
  ]
}
"#;

    // ---- THE test. --------------------------------------------------------

    /// The single most important assertion in this crate: a disk whose vendor
    /// verdict is PASSED, but whose raw counters show 2,776 pending and 2,776
    /// offline-uncorrectable sectors, must come back **CRIT**.
    #[test]
    /// Seagate packs minutes and seconds into the high bits of the
    /// Power_On_Hours raw integer. Taking `raw.value` verbatim printed
    /// power-on times around 10^15 hours for four drives on bigboy. The
    /// rendered string carries the real figure.
    #[test]
    fn seagate_packed_power_on_hours_uses_the_rendered_string() {
        let json = r#"{
          "model_name": "ST8000DM004-2CX188",
          "serial_number": "REDACTED_HARDWARE_ID",
          "smart_status": {"passed": true},
          "ata_smart_attributes": {"table": [
            {"id": 9, "name": "Power_On_Hours",
             "raw": {"value": 8857098737736293, "string": "55909h+34m+22.204s"}},
            {"id": 197, "name": "Current_Pending_Sector",
             "raw": {"value": 104, "string": "104"}}
          ]}
        }"#;
        let r = parse_smartctl_json(json).expect("valid smartctl JSON");
        assert_eq!(r.power_on_hours, Some(55909), "must not take the packed integer");
        // Every other attribute still trusts the integer.
        assert_eq!(r.pending_sectors, Some(104));
    }

    #[test]
    fn za215mpq_reports_crit_despite_smart_saying_passed() {
        let r = parse_smartctl_json(REDACTED_HARDWARE_ID_JSON).expect("valid smartctl JSON");

        // The verdict really does say PASSED — that is the trap.
        assert_eq!(
            r.smart_overall_passed,
            Some(true),
            "fixture must reproduce the PASSED verdict"
        );

        // The raw counters are surfaced verbatim for the drift engine.
        assert_eq!(r.pending_sectors, Some(2776));
        assert_eq!(r.offline_uncorrectable, Some(2776));
        assert_eq!(r.reallocated_sectors, Some(0));
        assert_eq!(r.crc_errors, Some(0));
        assert_eq!(r.power_on_hours, Some(35528));
        assert_eq!(r.serial.as_deref(), Some("REDACTED_HARDWARE_ID"));
        assert_eq!(r.model.as_deref(), Some("ST10000NM0086-2AA101"));
        assert_eq!(r.size_bytes, Some(10_000_831_348_736));

        let (status, reasons) = classify(&r);
        assert_eq!(status, STATUS_CRIT, "PASSED must not suppress CRIT");
        assert!(
            reasons.iter().any(|s| s.contains("2776 pending sectors")),
            "reasons name the pending sectors: {reasons:?}"
        );
        assert!(
            reasons.iter().any(|s| s.contains("2776 offline-uncorrectable")),
            "reasons name the uncorrectables: {reasons:?}"
        );
        assert!(
            reasons.iter().any(|s| s.contains("PASSED")),
            "reasons call out the contradiction with the verdict: {reasons:?}"
        );

        // And end to end through the emitted record.
        let blocks = parse_lsblk_json(LSBLK_JSON);
        let block = blocks.iter().find(|b| b.path == "/dev/sdc");
        let disk = build_disk("/dev/sdc", &r, block, None);
        assert_eq!(disk.status, STATUS_CRIT);
        assert_eq!(disk.pending_sectors, Some(2776));
        assert_eq!(disk.offline_uncorrectable, Some(2776));
        assert_eq!(disk.smart_overall_passed, Some(true));
        assert_eq!(disk.serial, "REDACTED_HARDWARE_ID");
        assert_eq!(disk.kind, "hdd");
        assert_eq!(disk.rotational, Some(true));
        assert_eq!(disk.transport.as_deref(), Some("sata"));
    }

    /// The same disk through the *text* parser (smartctl without JSON support)
    /// must reach the identical verdict — the fallback path cannot be softer.
    #[test]
    fn za215mpq_text_fallback_also_reports_crit() {
        let r = parse_smartctl_text(SMARTCTL_TEXT);
        assert_eq!(r.serial.as_deref(), Some("REDACTED_HARDWARE_ID"));
        assert_eq!(r.model.as_deref(), Some("ST10000NM0086-2AA101"));
        assert_eq!(r.rotation_rate, Some(7200));
        assert_eq!(r.smart_overall_passed, Some(true));
        assert_eq!(r.pending_sectors, Some(2776));
        assert_eq!(r.offline_uncorrectable, Some(2776));
        assert_eq!(r.reallocated_sectors, Some(0));
        assert_eq!(r.power_on_hours, Some(35528));
        assert_eq!(r.crc_errors, Some(0));
        assert_eq!(classify(&r).0, STATUS_CRIT);
    }

    /// A parenthetical raw value (`35 (Min/Max 25/44)`) must not poison parsing.
    #[test]
    fn text_parser_takes_the_leading_integer_of_a_decorated_raw_value() {
        assert_eq!(leading_u64("35 (Min/Max 25/44)"), Some(35));
        assert_eq!(leading_u64("2776"), Some(2776));
        assert_eq!(leading_u64("-"), None);
        assert_eq!(leading_u64(""), None);
    }

    #[test]
    fn healthy_ssd_is_ok() {
        let r = parse_smartctl_json(HEALTHY_SSD_JSON).expect("json");
        assert_eq!(classify(&r).0, STATUS_OK);
        let disk = build_disk("/dev/sda", &r, None, None);
        assert_eq!(disk.kind, "ssd", "rotation_rate 0 means solid state");
        assert!(disk.reasons.is_empty());
    }

    #[test]
    fn reallocated_or_crc_alone_is_warn_not_crit() {
        let mut r = parse_smartctl_json(HEALTHY_SSD_JSON).expect("json");
        r.reallocated_sectors = Some(4);
        assert_eq!(classify(&r).0, STATUS_WARN);

        let mut r = parse_smartctl_json(HEALTHY_SSD_JSON).expect("json");
        r.crc_errors = Some(17);
        let (status, reasons) = classify(&r);
        assert_eq!(status, STATUS_WARN);
        assert!(reasons.iter().any(|s| s.contains("cable")));
    }

    /// A FAILED verdict with clean raw counters is still not CRIT — we branch on
    /// counters in both directions, deliberately and symmetrically.
    #[test]
    fn a_failed_verdict_with_clean_counters_does_not_drive_status() {
        let mut r = parse_smartctl_json(HEALTHY_SSD_JSON).expect("json");
        r.smart_overall_passed = Some(false);
        assert_eq!(
            classify(&r).0,
            STATUS_OK,
            "status is computed from raw counters only, in both directions"
        );
    }

    #[test]
    fn nvme_critical_warning_is_crit_and_wear_is_reported() {
        let r = parse_smartctl_json(NVME_JSON).expect("json");
        assert_eq!(r.nvme_critical_warning, Some(4));
        assert_eq!(r.nvme_percentage_used, Some(91));
        assert_eq!(r.nvme_media_errors, Some(0));
        assert_eq!(r.nvme_data_units_written, Some(233_781_004));
        assert_eq!(r.power_on_hours, Some(14211), "falls back to the nvme log");

        let (status, reasons) = classify(&r);
        assert_eq!(status, STATUS_CRIT);
        assert!(reasons.iter().any(|s| s.contains("critical_warning")));
        assert!(reasons.iter().any(|s| s.contains("wear at 91%")));

        let disk = build_disk("/dev/nvme0", &r, None, None);
        assert_eq!(disk.kind, "nvme");
    }

    #[test]
    fn nvme_wear_alone_is_warn() {
        let mut r = parse_smartctl_json(NVME_JSON).expect("json");
        r.nvme_critical_warning = Some(0);
        assert_eq!(classify(&r).0, STATUS_WARN);
        r.nvme_percentage_used = Some(85);
        assert_eq!(classify(&r).0, STATUS_OK, "85 is the boundary, not over it");
    }

    /// No readable counters at all is UNKNOWN — never a falsely reassuring OK.
    #[test]
    fn unreadable_disk_is_unknown_not_ok() {
        let r = SmartReadings {
            model: Some("Some Disk".into()),
            ..Default::default()
        };
        assert_eq!(classify(&r).0, STATUS_UNKNOWN);
        let disk = build_disk("/dev/sdz", &r, None, Some("needs root".into()));
        assert_eq!(disk.status, STATUS_UNKNOWN);
        assert_eq!(disk.error.as_deref(), Some("needs root"));
    }

    /// Vendors rename attributes; the id is authoritative and the name resolves
    /// the rest.
    #[test]
    fn attribute_slots_resolve_by_id_then_by_vendor_name() {
        assert_eq!(attr_slot(197, "Whatever_Vendor_Called_It"), Some(ATTR_PENDING));
        assert_eq!(
            attr_slot(0, "Current_Pending_Sector_Count"),
            Some(ATTR_PENDING)
        );
        assert_eq!(
            attr_slot(0, "Uncorrectable_Sector_Ct"),
            Some(ATTR_OFFLINE_UNCORRECTABLE)
        );
        assert_eq!(attr_slot(0, "Reallocated_Sector_Ct"), Some(ATTR_REALLOCATED));
        assert_eq!(attr_slot(0, "UDMA_CRC_Error_Count"), Some(ATTR_CRC));
        // 187 Reported_Uncorrect is a different counter and must not be mapped
        // onto 198 by a sloppy substring match.
        assert_eq!(attr_slot(187, "Reported_Uncorrect"), None);
        assert_eq!(attr_slot(194, "Temperature_Celsius"), None);
    }

    #[test]
    fn lsblk_json_yields_disks_only() {
        let devs = parse_lsblk_json(LSBLK_JSON);
        let paths: Vec<&str> = devs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/dev/sda", "/dev/sdc", "/dev/nvme0n1", "/dev/zd0"],
            "partitions dropped, disks kept"
        );
        let sdc = devs.iter().find(|d| d.path == "/dev/sdc").unwrap();
        assert_eq!(sdc.serial.as_deref(), Some("REDACTED_HARDWARE_ID"));
        assert_eq!(sdc.size_bytes, Some(10_000_831_348_736));
        assert_eq!(sdc.rotational, Some(true));
    }

    #[test]
    fn lsblk_stringy_columns_and_bare_names_are_handled() {
        let devs = parse_lsblk_json(LSBLK_JSON_STRINGY);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].path, "/dev/sdb", "a bare name gains the /dev prefix");
        assert_eq!(devs[0].rotational, Some(true), "\"1\" is true");
        assert_eq!(
            devs[0].size_bytes,
            Some((12.7 * 1024f64.powi(4)) as u64),
            "human sizes are binary multiples"
        );
    }

    #[test]
    fn scan_json_gives_paths_and_device_types() {
        let devs = parse_smartctl_scan(SCAN_JSON);
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[0].path, "/dev/sda");
        assert_eq!(devs[0].dev_type.as_deref(), Some("sat"));
        assert_eq!(devs[2].dev_type.as_deref(), Some("nvme"));
    }

    #[test]
    fn garbage_output_never_panics() {
        assert!(parse_smartctl_json("not json at all").is_none());
        assert!(parse_smartctl_json("").is_none());
        assert_eq!(parse_smartctl_scan("<html>nope</html>").len(), 0);
        assert_eq!(parse_lsblk_json("{}").len(), 0);
        assert_eq!(parse_smartctl_text("").pending_sectors, None);
    }

    #[tokio::test]
    async fn non_linux_hosts_report_unavailable_cleanly() {
        if cfg!(target_os = "linux") {
            return;
        }
        let res = collect(SmartOptions::default()).await.expect("never errors");
        assert!(!res.smartctl_available);
        assert!(res.disks.is_empty());
        assert!(res.error.is_some(), "says why it is unavailable");
    }
}
