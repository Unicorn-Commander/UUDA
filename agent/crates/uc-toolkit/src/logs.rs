// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use tokio::process::Command;

use crate::error::{ToolError, ToolResult};

pub async fn collect(
    service: &str,
    since_secs: Option<i64>,
    max_lines: u32,
) -> ToolResult<uc_types::LogsResponse> {
    if service.is_empty() {
        return Err(ToolError::InvalidInput("service name is required".into()));
    }
    let cap = max_lines.min(uc_types::MAX_LOG_LINES_HARD);

    let mut entries: Vec<uc_types::LogEntry> = Vec::with_capacity(cap as usize);
    let mut truncated = false;

    let raw = backend_fetch(service, since_secs, cap).await?;
    let stream = BufReader::new(raw.as_slice());
    let mut lines = stream.lines();
    while let Some(line) = lines.next_line().await? {
        if entries.len() as u32 >= cap {
            truncated = true;
            break;
        }
        if let Some(entry) = parse_line_for_platform(&line) {
            entries.push(entry);
        }
    }

    Ok(uc_types::LogsResponse {
        entries,
        truncated,
        error: None,
    })
}

#[cfg(target_os = "linux")]
async fn backend_fetch(service: &str, since_secs: Option<i64>, max_lines: u32) -> ToolResult<Vec<u8>> {
    let mut cmd = Command::new("journalctl");
    cmd.arg("-u").arg(service);
    cmd.arg("-o").arg("json");
    cmd.arg("--no-pager");
    cmd.arg("-n").arg(max_lines.to_string());
    if let Some(since) = since_secs {
        cmd.arg("--since").arg(format!("@{since}"));
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Backend(format!("journalctl: {e}")))?;
    if !output.status.success() {
        return Err(ToolError::Backend(format!(
            "journalctl exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

#[cfg(target_os = "macos")]
use std::process::Stdio as MacStdio;
#[cfg(target_os = "macos")]
use tokio::process::Command as MacCommand;

#[cfg(target_os = "macos")]
async fn backend_fetch(service: &str, since_secs: Option<i64>, max_lines: u32) -> ToolResult<Vec<u8>> {
    let predicate = format!("subsystem == \"{}\"", service.replace('"', "\\\""));
    let mut cmd = MacCommand::new("log");
    cmd.arg("show");
    cmd.arg("--predicate").arg(&predicate);
    cmd.arg("--style").arg("ndjson");
    cmd.arg("--info");
    if let Some(since) = since_secs {
        let when = time::OffsetDateTime::from_unix_timestamp(since).map_err(|e| {
            ToolError::InvalidInput(format!("since_secs={since} unparseable: {e}"))
        })?;
        let format = time::format_description::parse(
            "[year]-[month]-[day] [hour]:[minute]:[second]",
        )
        .map_err(|e| ToolError::Internal(format!("time format: {e}")))?;
        let formatted = when
            .format(&format)
            .map_err(|e| ToolError::Internal(format!("time format: {e}")))?;
        cmd.arg("--start").arg(formatted);
    }
    cmd.stdout(MacStdio::piped()).stderr(MacStdio::piped());
    let _ = max_lines;
    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Backend(format!("log show: {e}")))?;
    if !output.status.success() {
        return Err(ToolError::Backend(format!(
            "log show exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn backend_fetch(_service: &str, _since: Option<i64>, _max: u32) -> ToolResult<Vec<u8>> {
    Err(ToolError::Backend(
        "no log backend on this platform".into(),
    ))
}

fn parse_line_for_platform(line: &str) -> Option<uc_types::LogEntry> {
    #[cfg(target_os = "linux")]
    {
        return parse_journalctl_line(line);
    }
    #[cfg(target_os = "macos")]
    {
        return parse_log_show_line(line);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = line;
        None
    }
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct JournalctlEntry {
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    unit: Option<String>,
    #[serde(rename = "_PID")]
    pid: Option<String>,
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_id: Option<String>,
}

#[cfg(target_os = "linux")]
fn parse_journalctl_line(line: &str) -> Option<uc_types::LogEntry> {
    let entry: JournalctlEntry = serde_json::from_str(line).ok()?;
    let micros: i64 = entry.realtime.as_deref()?.parse().ok()?;
    let secs = micros / 1_000_000;
    let nanos = ((micros % 1_000_000) * 1_000) as i32;
    let at = pbjson_types::Timestamp { seconds: secs, nanos };

    let message = match entry.message {
        Some(serde_json::Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    };

    let level = match entry.priority.as_deref() {
        Some("0") | Some("1") | Some("2") | Some("3") => "error",
        Some("4") => "warn",
        Some("5") | Some("6") => "info",
        Some("7") => "debug",
        _ => "info",
    }
    .to_string();

    let mut fields = std::collections::HashMap::new();
    if let Some(u) = entry.unit {
        fields.insert("unit".into(), u);
    }
    if let Some(p) = entry.pid {
        fields.insert("pid".into(), p);
    }
    if let Some(s) = entry.syslog_id {
        fields.insert("syslog_id".into(), s);
    }

    Some(uc_types::LogEntry {
        at: Some(at),
        level,
        message,
        fields,
    })
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct LogShowEntry {
    timestamp: Option<String>,
    #[serde(rename = "eventMessage")]
    event_message: Option<String>,
    subsystem: Option<String>,
    category: Option<String>,
    level: Option<String>,
    #[serde(rename = "processID")]
    process_id: Option<i64>,
    #[serde(rename = "processImagePath")]
    process_image_path: Option<String>,
}

#[cfg(target_os = "macos")]
fn parse_log_show_line(line: &str) -> Option<uc_types::LogEntry> {
    let entry: LogShowEntry = serde_json::from_str(line).ok()?;
    let at = entry
        .timestamp
        .as_deref()
        .and_then(parse_log_show_timestamp);

    let level = match entry.level.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("error") | Some("fault") => "error",
        Some("default") | Some("info") => "info",
        Some("debug") => "debug",
        _ => "info",
    }
    .to_string();

    let mut fields = std::collections::HashMap::new();
    if let Some(s) = entry.subsystem {
        fields.insert("subsystem".into(), s);
    }
    if let Some(c) = entry.category {
        fields.insert("category".into(), c);
    }
    if let Some(p) = entry.process_id {
        fields.insert("pid".into(), p.to_string());
    }
    if let Some(p) = entry.process_image_path {
        fields.insert("process".into(), p);
    }

    Some(uc_types::LogEntry {
        at,
        level,
        message: entry.event_message.unwrap_or_default(),
        fields,
    })
}

#[cfg(target_os = "macos")]
fn parse_log_show_timestamp(raw: &str) -> Option<pbjson_types::Timestamp> {
    let format = time::format_description::parse(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6][offset_hour sign:mandatory][offset_minute]",
    )
    .ok()?;
    let parsed = time::OffsetDateTime::parse(raw, &format).ok()?;
    Some(pbjson_types::Timestamp {
        seconds: parsed.unix_timestamp(),
        nanos: parsed.nanosecond() as i32,
    })
}
