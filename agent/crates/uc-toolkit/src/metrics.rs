// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind, System};
use tokio::sync::RwLock;
use tokio::time::interval;

use crate::error::ToolResult;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const RING_LEN: usize = 3600;

#[derive(Clone)]
struct Sample {
    at: time::OffsetDateTime,
    cpu_user: f64,
    cpu_system: f64,
    cpu_idle: f64,
    mem_used: u64,
    mem_available: u64,
    swap_used: u64,
    disk_used: Vec<(String, u64)>,
    net_rx: Vec<(String, u64)>,
    net_tx: Vec<(String, u64)>,
}

#[derive(Clone)]
pub struct Sampler {
    inner: Arc<RwLock<VecDeque<Sample>>>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(VecDeque::with_capacity(RING_LEN))),
        }
    }

    pub fn start(self) {
        tokio::spawn(async move {
            let refresh = RefreshKind::new()
                .with_cpu(CpuRefreshKind::new().with_cpu_usage())
                .with_memory(MemoryRefreshKind::everything());
            let mut sys = System::new_with_specifics(refresh);
            let mut nets = Networks::new_with_refreshed_list();
            let mut disks = Disks::new_with_refreshed_list();
            let mut tick = interval(SAMPLE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let _started = Instant::now();
            loop {
                tick.tick().await;
                sys.refresh_specifics(refresh);
                nets.refresh();
                disks.refresh();
                let now = time::OffsetDateTime::now_utc();
                let cpu = global_cpu_breakdown(&sys);
                let mem_used = sys.used_memory();
                let mem_avail = sys.available_memory();
                let swap_used = sys.used_swap();
                let disk_used: Vec<(String, u64)> = disks
                    .iter()
                    .map(|d| {
                        (
                            d.mount_point().to_string_lossy().into_owned(),
                            d.total_space().saturating_sub(d.available_space()),
                        )
                    })
                    .collect();
                let net_rx: Vec<(String, u64)> = nets
                    .iter()
                    .map(|(name, data)| (name.clone(), data.total_received()))
                    .collect();
                let net_tx: Vec<(String, u64)> = nets
                    .iter()
                    .map(|(name, data)| (name.clone(), data.total_transmitted()))
                    .collect();
                let sample = Sample {
                    at: now,
                    cpu_user: cpu.0,
                    cpu_system: cpu.1,
                    cpu_idle: cpu.2,
                    mem_used,
                    mem_available: mem_avail,
                    swap_used,
                    disk_used,
                    net_rx,
                    net_tx,
                };
                let mut q = self.inner.write().await;
                if q.len() >= RING_LEN {
                    q.pop_front();
                }
                q.push_back(sample);
            }
        });
    }

    pub async fn snapshot(&self, window: Duration, max_series: u32) -> ToolResult<uc_types::MetricsResponse> {
        let cap = clamp_max_series(max_series);
        let now = time::OffsetDateTime::now_utc();
        let earliest = now - time::Duration::seconds(window.as_secs() as i64);
        let q = self.inner.read().await;
        let samples: Vec<&Sample> = q.iter().filter(|s| s.at >= earliest).collect();

        if samples.is_empty() {
            return Ok(uc_types::MetricsResponse {
                window_start: Some(timestamp(now)),
                window_end: Some(timestamp(now)),
                series: vec![],
                truncated: false,
                error: None,
            });
        }

        let mut series: Vec<uc_types::MetricSeries> = Vec::new();
        series.push(scalar_series("cpu.user", "ratio", &samples, |s| s.cpu_user));
        series.push(scalar_series("cpu.system", "ratio", &samples, |s| s.cpu_system));
        series.push(scalar_series("cpu.idle", "ratio", &samples, |s| s.cpu_idle));
        series.push(scalar_series("mem.used_bytes", "bytes", &samples, |s| s.mem_used as f64));
        series.push(scalar_series("mem.available_bytes", "bytes", &samples, |s| {
            s.mem_available as f64
        }));
        series.push(scalar_series("mem.swap_used_bytes", "bytes", &samples, |s| {
            s.swap_used as f64
        }));

        for mount in unique_keys(&samples, |s| s.disk_used.iter().map(|(m, _)| m.clone()).collect()) {
            series.push(labeled_series(
                "disk.used_bytes",
                "bytes",
                [("mount".to_string(), mount.clone())].into(),
                &samples,
                |s| {
                    s.disk_used
                        .iter()
                        .find(|(m, _)| m == &mount)
                        .map(|(_, v)| *v as f64)
                        .unwrap_or(f64::NAN)
                },
            ));
        }

        for nic in unique_keys(&samples, |s| s.net_rx.iter().map(|(n, _)| n.clone()).collect()) {
            series.push(labeled_series(
                "net.rx_bytes",
                "bytes",
                [("interface".to_string(), nic.clone())].into(),
                &samples,
                |s| {
                    s.net_rx
                        .iter()
                        .find(|(n, _)| n == &nic)
                        .map(|(_, v)| *v as f64)
                        .unwrap_or(f64::NAN)
                },
            ));
            series.push(labeled_series(
                "net.tx_bytes",
                "bytes",
                [("interface".to_string(), nic.clone())].into(),
                &samples,
                |s| {
                    s.net_tx
                        .iter()
                        .find(|(n, _)| n == &nic)
                        .map(|(_, v)| *v as f64)
                        .unwrap_or(f64::NAN)
                },
            ));
        }

        let total = series.len() as u32;
        let truncated = total > cap;
        if truncated {
            series.truncate(cap as usize);
        }

        Ok(uc_types::MetricsResponse {
            window_start: Some(timestamp(samples.first().unwrap().at)),
            window_end: Some(timestamp(samples.last().unwrap().at)),
            series,
            truncated,
            error: None,
        })
    }
}

fn clamp_max_series(requested: u32) -> u32 {
    let n = if requested == 0 { uc_types::MAX_SERIES_DEFAULT } else { requested };
    n.min(uc_types::MAX_SERIES_HARD)
}

fn global_cpu_breakdown(sys: &System) -> (f64, f64, f64) {
    let cpus = sys.cpus();
    if cpus.is_empty() {
        return (0.0, 0.0, 1.0);
    }
    let total = cpus.len() as f64;
    let user: f64 = cpus.iter().map(|c| c.cpu_usage() as f64 / 100.0).sum::<f64>() / total;
    let idle = (1.0 - user).clamp(0.0, 1.0);
    (user, 0.0, idle)
}

fn timestamp(t: time::OffsetDateTime) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

fn scalar_series<F>(name: &str, unit: &str, samples: &[&Sample], extract: F) -> uc_types::MetricSeries
where
    F: Fn(&Sample) -> f64,
{
    let points = samples
        .iter()
        .map(|s| uc_types::MetricPoint {
            at: Some(timestamp(s.at)),
            value: extract(s),
        })
        .collect();
    uc_types::MetricSeries {
        name: name.into(),
        unit: unit.into(),
        labels: Default::default(),
        points,
    }
}

fn labeled_series<F>(
    name: &str,
    unit: &str,
    labels: std::collections::HashMap<String, String>,
    samples: &[&Sample],
    extract: F,
) -> uc_types::MetricSeries
where
    F: Fn(&Sample) -> f64,
{
    let points = samples
        .iter()
        .map(|s| uc_types::MetricPoint {
            at: Some(timestamp(s.at)),
            value: extract(s),
        })
        .collect();
    uc_types::MetricSeries {
        name: name.into(),
        unit: unit.into(),
        labels,
        points,
    }
}

fn unique_keys<F>(samples: &[&Sample], extract: F) -> Vec<String>
where
    F: Fn(&Sample) -> Vec<String>,
{
    let mut seen = std::collections::BTreeSet::new();
    for s in samples {
        for k in extract(s) {
            seen.insert(k);
        }
    }
    seen.into_iter().collect()
}
