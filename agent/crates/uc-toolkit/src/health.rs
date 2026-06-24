// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use sysinfo::{Disks, System};

use crate::error::{ToolError, ToolResult};

pub fn collect(agent_version: String) -> ToolResult<uc_types::HealthResponse> {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();

    let load = System::load_average();
    let disks = Disks::new_with_refreshed_list();

    let uptime_secs = System::uptime();
    let kernel = System::kernel_version().unwrap_or_default();
    let os = System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string());

    let memory = uc_types::Memory {
        total_bytes: sys.total_memory(),
        used_bytes: sys.used_memory(),
        available_bytes: sys.available_memory(),
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
    };

    let disk_list: Vec<uc_types::DiskUsage> = disks
        .iter()
        .map(|d| {
            let total = d.total_space();
            let avail = d.available_space();
            uc_types::DiskUsage {
                mountpoint: d.mount_point().to_string_lossy().into_owned(),
                filesystem: d.file_system().to_string_lossy().into_owned(),
                total_bytes: total,
                used_bytes: total.saturating_sub(avail),
                available_bytes: avail,
            }
        })
        .collect();

    let cpu_temp = read_cpu_temp();

    Ok(uc_types::HealthResponse {
        uptime: Some(duration_to_proto(Duration::from_secs(uptime_secs))),
        load: Some(uc_types::Load {
            one: load.one,
            five: load.five,
            fifteen: load.fifteen,
        }),
        memory: Some(memory),
        disks: disk_list,
        cpu_temp_celsius: cpu_temp,
        agent_version,
        os,
        kernel,
        error: None,
    })
}

#[cfg(target_os = "linux")]
fn read_cpu_temp() -> Option<f64> {
    use std::fs;
    let entries = fs::read_dir("/sys/class/thermal").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let typ_path = path.join("type");
        let temp_path = path.join("temp");
        let typ = fs::read_to_string(&typ_path).ok()?;
        if !typ.starts_with("x86_pkg_temp") && !typ.starts_with("cpu") && !typ.starts_with("coretemp") {
            continue;
        }
        let raw = fs::read_to_string(&temp_path).ok()?;
        let milli: f64 = raw.trim().parse().ok()?;
        return Some(milli / 1000.0);
    }
    None
}

#[cfg(target_os = "macos")]
fn read_cpu_temp() -> Option<f64> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_cpu_temp() -> Option<f64> {
    None
}

fn duration_to_proto(d: Duration) -> pbjson_types::Duration {
    pbjson_types::Duration {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

// Re-exported for parity with other tools that need it.
#[allow(dead_code)]
pub(crate) fn ensure_no_input(_dry_run: bool) -> ToolResult<()> {
    Ok(())
}

#[allow(dead_code)]
fn _unused_marker(_e: ToolError) {}
