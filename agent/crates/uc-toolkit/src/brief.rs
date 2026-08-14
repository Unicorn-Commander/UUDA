// SPDX-License-Identifier: Apache-2.0
//
// Squad-mode brief/report relay. uc-agent doesn't run an LLM; this module
// just persists what Brigade pushes (`agent.brief`) and serves whatever
// the on-device LLM has reported back (`agent.report`). Phase 1.5 reads
// and writes JSON to `state_dir/brief.json` and `state_dir/report.json`.
// Phase 2 connects the on-device agent (Squad) to those files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::error::{ToolError, ToolResult};

const BRIEF_FILE: &str = "brief.json";
const REPORT_FILE: &str = "report.json";

/// Snapshot of the current brief and report on disk.
#[derive(Clone)]
pub struct BriefStore {
    state_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredBrief {
    pub version: String,
    pub system_prompt: String,
    pub context: std::collections::HashMap<String, String>,
    pub tasks: Vec<StoredTask>,
    pub issued_at_unix_ms: i64,
    pub issued_by: String,
    pub received_at_unix_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredTask {
    pub id: String,
    pub description: String,
    pub deadline_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredReport {
    pub version: String,
    pub generated_at_unix_ms: i64,
    pub task_statuses: Vec<StoredTaskStatus>,
    pub anomalies: Vec<StoredAnomaly>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredTaskStatus {
    pub task_id: String,
    pub status: String,
    pub note: String,
    pub updated_at_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredAnomaly {
    pub at_unix_ms: i64,
    pub category: String,
    pub description: String,
}

impl BriefStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            state_dir: state_dir.as_ref().to_path_buf(),
        }
    }

    fn brief_path(&self) -> PathBuf {
        self.state_dir.join(BRIEF_FILE)
    }

    fn report_path(&self) -> PathBuf {
        self.state_dir.join(REPORT_FILE)
    }

    pub async fn write_brief(&self, brief: &StoredBrief) -> ToolResult<()> {
        fs::create_dir_all(&self.state_dir).await?;
        let raw = serde_json::to_vec_pretty(brief)
            .map_err(|e| ToolError::Internal(format!("serialize brief: {e}")))?;
        let tmp = self.state_dir.join(format!("{BRIEF_FILE}.tmp"));
        fs::write(&tmp, &raw).await?;
        fs::rename(&tmp, self.brief_path()).await?;
        Ok(())
    }

    pub async fn read_brief(&self) -> ToolResult<Option<StoredBrief>> {
        match fs::read(self.brief_path()).await {
            Ok(bytes) => {
                let parsed: StoredBrief = serde_json::from_slice(&bytes)
                    .map_err(|e| ToolError::Internal(format!("parse brief.json: {e}")))?;
                Ok(Some(parsed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn read_report(&self) -> ToolResult<Option<StoredReport>> {
        match fs::read(self.report_path()).await {
            Ok(bytes) => {
                let parsed: StoredReport = serde_json::from_slice(&bytes)
                    .map_err(|e| ToolError::Internal(format!("parse report.json: {e}")))?;
                Ok(Some(parsed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn timestamp_from_unix_ms(ms: i64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: ms / 1000,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}
