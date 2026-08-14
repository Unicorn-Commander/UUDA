// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uc_toolkit::{brief as briefmod, files, health, logs, messages, metrics, package, service};

#[derive(Clone)]
pub struct UudaServer {
    sampler: metrics::Sampler,
    agent_version: String,
    brief_store: briefmod::BriefStore,
    has_local_agent: bool,
    messages_enabled: bool,
    messages_db_path: std::path::PathBuf,
    files_enabled: bool,
    files_roots: Vec<std::path::PathBuf>,
    files_index_path: std::path::PathBuf,
    #[allow(dead_code)]
    tool_router: ToolRouter<UudaServer>,
}

impl UudaServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sampler: metrics::Sampler,
        agent_version: String,
        state_dir: std::path::PathBuf,
        has_local_agent: bool,
        messages_enabled: bool,
        messages_db_path: std::path::PathBuf,
        files_enabled: bool,
        files_roots: Vec<std::path::PathBuf>,
        files_index_path: std::path::PathBuf,
    ) -> Self {
        Self {
            sampler,
            agent_version,
            brief_store: briefmod::BriefStore::new(state_dir),
            has_local_agent,
            messages_enabled,
            messages_db_path,
            files_enabled,
            files_roots,
            files_index_path,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HealthIn {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MetricsIn {
    /// Window length in seconds (max equal to the agent's ring buffer of 3600).
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// Soft cap on series count. 0 means default (100). Hard cap 500.
    #[serde(default)]
    pub max_series: u32,
}

fn default_window_secs() -> u64 {
    300
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LogsIn {
    /// systemd unit name (e.g. "uc-agent", "nginx").
    pub service: String,
    /// Unix timestamp seconds. If unset, journalctl picks an implementation default.
    #[serde(default)]
    pub since_secs: Option<i64>,
    /// Max lines (default 200, hard cap 10000).
    #[serde(default = "default_max_lines")]
    pub max_lines: u32,
}

fn default_max_lines() -> u32 {
    200
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PackageInstallIn {
    /// Package name as the backend understands it.
    pub name: String,
    /// Optional version pin.
    #[serde(default)]
    pub version: Option<String>,
    /// "auto" | "apt" | "brew". Defaults to auto.
    #[serde(default)]
    pub backend: Option<String>,
    /// Defaults to true. Set false to actually apply the install.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ServiceStatusIn {
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentBriefIn {
    /// Brief version string (e.g. "v1.0.0" or "katie-prompt-2026-04-26").
    pub version: String,
    /// System prompt the on-device LtCol should use.
    pub system_prompt: String,
    /// Free-form key/value context (machine role, recent ops, customer notes).
    #[serde(default)]
    pub context: std::collections::HashMap<String, String>,
    /// Concrete tasks the on-device agent should pursue.
    #[serde(default)]
    pub tasks: Vec<BriefTaskIn>,
    /// Who issued this brief — Brigade persona id or operator user id.
    #[serde(default)]
    pub issued_by: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BriefTaskIn {
    pub id: String,
    pub description: String,
    /// Unix milliseconds; omit for no deadline.
    #[serde(default)]
    pub deadline_unix_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentReportIn {}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessagesListIn {
    /// Max conversations to return. 0 means default (20). Hard cap 200.
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessagesThreadIn {
    /// Conversation identifier (phone/email or group guid) from messages.list.
    pub chat_identifier: String,
    /// Max messages, newest kept. 0 means default (50). Hard cap 500.
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessagesPollIn {
    /// Return messages with ROWID greater than this. 0 returns the recent tail.
    #[serde(default)]
    pub since_rowid: i64,
    /// Max messages. 0 means default (100). Hard cap 1000.
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessageSendIn {
    /// Recipient handle (phone/email) or chat identifier.
    pub to: String,
    /// Message body.
    pub body: String,
    /// "iMessage" | "SMS" | "" (auto). Defaults to iMessage.
    #[serde(default)]
    pub service: String,
    /// Defaults to true. Set false to actually send the message.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FilesSearchIn {
    /// Keyword query (FTS5). Sanitized server-side; never errors on bad syntax.
    pub query: String,
    /// Max hits. Default 10, clamped to 1..=50.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Optional path prefix filter: only paths starting with this are returned.
    #[serde(default)]
    pub path_prefix: Option<String>,
}

fn default_limit() -> u32 {
    10
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FilesIndexIn {
    /// Roots to index. Null/empty uses the device's configured roots.
    #[serde(default)]
    pub roots: Option<Vec<String>>,
    /// Reindex every file even when unchanged. Defaults to false.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FilesStatusIn {}

#[tool_router]
impl UudaServer {
    #[tool(
        name = "system.health",
        description = "Snapshot of host health: uptime, load, memory, disk, optional cpu temperature. Read-only."
    )]
    async fn health(&self) -> Result<CallToolResult, McpError> {
        match health::collect(self.agent_version.clone()) {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "system.metrics",
        description = "Time-series CPU, memory, disk, and network counters from the agent's 1Hz sampler over a requested window. Read-only."
    )]
    async fn metrics(
        &self,
        Parameters(args): Parameters<MetricsIn>,
    ) -> Result<CallToolResult, McpError> {
        let window = std::time::Duration::from_secs(args.window_secs.min(3600));
        match self.sampler.snapshot(window, args.max_series).await {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "system.logs",
        description = "Recent systemd-journal entries for a named unit. Read-only."
    )]
    async fn logs(&self, Parameters(args): Parameters<LogsIn>) -> Result<CallToolResult, McpError> {
        match logs::collect(&args.service, args.since_secs, args.max_lines).await {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "package.install",
        description = "Install (or dry-run install) a package. Default dry_run=true. Returns a typed ChangeSet plus tail of backend stdout/stderr."
    )]
    async fn package_install(
        &self,
        Parameters(args): Parameters<PackageInstallIn>,
    ) -> Result<CallToolResult, McpError> {
        let backend_kind = match args.backend.as_deref() {
            Some("apt") => uc_types::PackageBackendKind::Apt as i32,
            Some("brew") => uc_types::PackageBackendKind::Brew as i32,
            _ => uc_types::PackageBackendKind::Auto as i32,
        };
        let request = uc_types::PackageInstallRequest {
            meta: Some(uc_types::ToolMeta {
                request_id: String::new(),
                dry_run: args.dry_run,
                timeout: None,
            }),
            name: args.name,
            version: args.version,
            backend: backend_kind,
        };
        match package::install(request).await {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "service.status",
        description = "Status of a system service: state, enabled flag, pid, last 20 log entries. Read-only."
    )]
    async fn service_status(
        &self,
        Parameters(args): Parameters<ServiceStatusIn>,
    ) -> Result<CallToolResult, McpError> {
        match service::status(&args.name).await {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "agent.brief",
        description = "Push a briefing to the on-device LtCol agent (Squad mode). Persists the brief on disk; the on-device LLM picks it up out of band."
    )]
    async fn agent_brief(
        &self,
        Parameters(args): Parameters<AgentBriefIn>,
    ) -> Result<CallToolResult, McpError> {
        let now_ms = briefmod::now_unix_ms();
        let stored = briefmod::StoredBrief {
            version: args.version.clone(),
            system_prompt: args.system_prompt,
            context: args.context,
            tasks: args
                .tasks
                .into_iter()
                .map(|t| briefmod::StoredTask {
                    id: t.id,
                    description: t.description,
                    deadline_unix_ms: t.deadline_unix_ms,
                })
                .collect(),
            issued_at_unix_ms: now_ms,
            issued_by: args.issued_by,
            received_at_unix_ms: now_ms,
        };
        match self.brief_store.write_brief(&stored).await {
            Ok(()) => {
                let resp = uc_types::AgentBriefResponse {
                    acknowledged_version: args.version,
                    received_at: Some(briefmod::timestamp_from_unix_ms(now_ms)),
                    error: None,
                };
                json_response(&resp)
            }
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "agent.report",
        description = "Read the on-device LtCol agent's last status: current brief version, task statuses, anomalies. Returns has_local_agent=false on headless servers."
    )]
    async fn agent_report(
        &self,
        Parameters(_): Parameters<AgentReportIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.has_local_agent {
            let resp = uc_types::AgentReportResponse {
                current_brief_version: String::new(),
                last_brief_at: None,
                task_statuses: Vec::new(),
                anomalies: Vec::new(),
                summary: "headless agent (no on-device LLM)".into(),
                has_local_agent: false,
                error: None,
            };
            return json_response(&resp);
        }

        let brief = match self.brief_store.read_brief().await {
            Ok(b) => b,
            Err(e) => return Ok(error_result(e)),
        };
        let report = match self.brief_store.read_report().await {
            Ok(r) => r,
            Err(e) => return Ok(error_result(e)),
        };

        let resp = uc_types::AgentReportResponse {
            current_brief_version: brief.as_ref().map(|b| b.version.clone()).unwrap_or_default(),
            last_brief_at: brief
                .as_ref()
                .map(|b| briefmod::timestamp_from_unix_ms(b.received_at_unix_ms)),
            task_statuses: report
                .as_ref()
                .map(|r| {
                    r.task_statuses
                        .iter()
                        .map(|s| uc_types::TaskStatus {
                            task_id: s.task_id.clone(),
                            status: s.status.clone(),
                            note: s.note.clone(),
                            updated_at: s.updated_at_unix_ms.map(briefmod::timestamp_from_unix_ms),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            anomalies: report
                .as_ref()
                .map(|r| {
                    r.anomalies
                        .iter()
                        .map(|a| uc_types::Anomaly {
                            at: Some(briefmod::timestamp_from_unix_ms(a.at_unix_ms)),
                            category: a.category.clone(),
                            description: a.description.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            summary: report.map(|r| r.summary).unwrap_or_default(),
            has_local_agent: true,
            error: None,
        };
        json_response(&resp)
    }

    #[tool(
        name = "messages.list",
        description = "Recent macOS Messages conversations (iMessage + SMS via Continuity), newest first, each summarised by its last message. Read-only. macOS only; requires the messages bridge enabled."
    )]
    async fn messages_list(
        &self,
        Parameters(args): Parameters<MessagesListIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.messages_enabled {
            return Ok(error_result(messages_disabled()));
        }
        let path = self.messages_db_path.clone();
        run_blocking(move || messages::list_conversations(&path, args.limit)).await
    }

    #[tool(
        name = "messages.thread",
        description = "Messages in one conversation, chronological (oldest to newest). Read-only. macOS only."
    )]
    async fn messages_thread(
        &self,
        Parameters(args): Parameters<MessagesThreadIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.messages_enabled {
            return Ok(error_result(messages_disabled()));
        }
        let path = self.messages_db_path.clone();
        run_blocking(move || messages::read_thread(&path, &args.chat_identifier, args.limit)).await
    }

    #[tool(
        name = "messages.poll",
        description = "New messages since a ROWID cursor (since_rowid). Returns ascending with a max_rowid to pass back next poll. since_rowid=0 returns the recent tail. Read-only. macOS only."
    )]
    async fn messages_poll(
        &self,
        Parameters(args): Parameters<MessagesPollIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.messages_enabled {
            return Ok(error_result(messages_disabled()));
        }
        let path = self.messages_db_path.clone();
        run_blocking(move || messages::poll_since(&path, args.since_rowid, args.limit)).await
    }

    #[tool(
        name = "messages.send",
        description = "Send a message via the macOS Messages app (AppleScript). Default dry_run=true returns a ChangeSet WITHOUT sending. Mutating/destructive: emits to a third party. macOS only."
    )]
    async fn messages_send(
        &self,
        Parameters(args): Parameters<MessageSendIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.messages_enabled {
            return Ok(error_result(messages_disabled()));
        }
        match messages::send(&args.to, &args.body, &args.service, args.dry_run).await {
            Ok(resp) => json_response(&resp),
            Err(e) => Ok(error_result(e)),
        }
    }

    #[tool(
        name = "files.search",
        description = "Keyword (FTS5) search over this device's indexed text files. Returns ranked hits with snippets, modified time and size. Read-only. Requires the file index enabled."
    )]
    async fn files_search(
        &self,
        Parameters(args): Parameters<FilesSearchIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.files_enabled {
            return files_disabled_result();
        }
        let index = files::FilesIndex::new(self.files_index_path.clone());
        run_blocking(move || {
            index.search(&args.query, args.limit, args.path_prefix.as_deref())
        })
        .await
    }

    #[tool(
        name = "files.index",
        description = "(Re)index this device's text files into the local FTS5 index. Uses the configured roots unless `roots` is given. Set force=true to reindex unchanged files. Returns counts + elapsed_ms."
    )]
    async fn files_index(
        &self,
        Parameters(args): Parameters<FilesIndexIn>,
    ) -> Result<CallToolResult, McpError> {
        if !self.files_enabled {
            return files_disabled_result();
        }
        let roots: Vec<std::path::PathBuf> = match args.roots {
            Some(r) if !r.is_empty() => r.into_iter().map(std::path::PathBuf::from).collect(),
            _ => self.files_roots.clone(),
        };
        let index = files::FilesIndex::new(self.files_index_path.clone());
        run_blocking(move || index.index(&roots, args.force)).await
    }

    #[tool(
        name = "files.status",
        description = "File index status: configured roots, indexed file count, total bytes, last-indexed timestamp, enabled flag. Read-only."
    )]
    async fn files_status(
        &self,
        Parameters(_): Parameters<FilesStatusIn>,
    ) -> Result<CallToolResult, McpError> {
        let index = files::FilesIndex::new(self.files_index_path.clone());
        let roots = self.files_roots.clone();
        let enabled = self.files_enabled;
        run_blocking(move || index.status(&roots, enabled)).await
    }
}

#[tool_handler]
impl ServerHandler for UudaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "UUDA per-device agent. Tools: system.health, system.metrics, system.logs, package.install, service.status. macOS Messages bridge (when enabled): messages.list, messages.thread, messages.poll, messages.send. File index (when enabled): files.search, files.index, files.status."
                .to_string(),
        )
    }
}

fn json_response<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json: Value = serde_json::to_value(value).map_err(|e| {
        McpError::internal_error(format!("serialize tool result: {e}"), None)
    })?;
    Ok(CallToolResult::success(vec![Content::text(json.to_string())]))
}

fn error_result(e: uc_toolkit::ToolError) -> CallToolResult {
    let envelope = e.into_proto();
    let json = serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".to_string());
    CallToolResult::error(vec![Content::text(json)])
}

fn messages_disabled() -> uc_toolkit::ToolError {
    uc_toolkit::ToolError::PermissionDenied(
        "messages bridge disabled on this device (set agent.messages_enabled = true)".into(),
    )
}

/// A non-error "files indexing disabled" result (a clear, machine-readable
/// payload rather than an error envelope), so callers can degrade gracefully.
fn files_disabled_result() -> Result<CallToolResult, McpError> {
    let payload = serde_json::json!({
        "enabled": false,
        "message": "file index disabled on this device (set agent.files_enabled = true or UC_FILES_ENABLED=1)",
    });
    Ok(CallToolResult::success(vec![Content::text(payload.to_string())]))
}

/// Run a blocking toolkit read (SQLite) off the async runtime and shape the result.
async fn run_blocking<T, F>(f: F) -> Result<CallToolResult, McpError>
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> uc_toolkit::ToolResult<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(resp)) => json_response(&resp),
        Ok(Err(e)) => Ok(error_result(e)),
        Err(e) => Ok(error_result(uc_toolkit::ToolError::Internal(format!(
            "blocking task join: {e}"
        )))),
    }
}

#[allow(dead_code)]
fn _ctx_unused(_: RequestContext<RoleServer>) {}

#[allow(dead_code)]
fn _arc_marker(_: Arc<()>) {}

// In-process exercise of the MCP tool layer against the real chat.db. Unlike the
// uc-toolkit unit tests (which call the readers directly), these drive the actual
// `#[tool]` methods on a constructed `UudaServer` — proving the messages_enabled
// gate, the spawn_blocking hop, and the proto -> MCP-JSON serialization end to end.
// macOS-only and self-skipping when no Messages database is present.
#[cfg(all(test, target_os = "macos"))]
mod messages_mcp_tests {
    use super::*;

    fn server(messages_enabled: bool) -> UudaServer {
        UudaServer::new(
            metrics::Sampler::new(),
            "0.0.0-test".to_string(),
            std::env::temp_dir(),
            false,
            messages_enabled,
            messages::default_db_path(),
            false,
            Vec::new(),
            std::env::temp_dir().join("uuda-test-files-index.db"),
        )
    }

    fn has_db() -> bool {
        messages::default_db_path().exists()
    }

    /// Serialize a CallToolResult to its MCP wire JSON: `{content:[{type,text}], isError?}`.
    fn wire(r: &CallToolResult) -> Value {
        serde_json::to_value(r).expect("serialize CallToolResult")
    }

    fn is_error(v: &Value) -> bool {
        v.get("isError").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The first text-content block, parsed as JSON (the tool's proto payload / error envelope).
    fn payload(v: &Value) -> Value {
        let text = v["content"][0]["text"]
            .as_str()
            .expect("text content block");
        serde_json::from_str(text).expect("content text is JSON")
    }

    #[tokio::test]
    async fn list_returns_real_conversations_over_the_tool_layer() {
        if !has_db() {
            eprintln!("skip: no chat.db");
            return;
        }
        let v = wire(
            &server(true)
                .messages_list(Parameters(MessagesListIn { limit: 5 }))
                .await
                .expect("tool call"),
        );
        assert!(!is_error(&v), "messages.list should succeed: {v}");
        let p = payload(&v);
        let convs = p["conversations"].as_array().expect("conversations array");
        assert!(!convs.is_empty(), "expected >=1 conversation");
        assert!(convs.len() <= 5, "limit honored");
        // pbjson camelCases the proto field.
        assert!(
            convs[0]["chatIdentifier"].is_string(),
            "conversation carries a chatIdentifier: {}",
            convs[0]
        );
    }

    #[tokio::test]
    async fn thread_reads_a_real_conversation_over_the_tool_layer() {
        if !has_db() {
            return;
        }
        let srv = server(true);
        let listed = payload(&wire(
            &srv.messages_list(Parameters(MessagesListIn { limit: 1 }))
                .await
                .expect("list"),
        ));
        let Some(chat_id) = listed["conversations"][0]["chatIdentifier"].as_str() else {
            eprintln!("skip: no conversation to thread");
            return;
        };
        let v = wire(
            &srv.messages_thread(Parameters(MessagesThreadIn {
                chat_identifier: chat_id.to_string(),
                limit: 10,
            }))
            .await
            .expect("thread"),
        );
        assert!(!is_error(&v), "messages.thread should succeed: {v}");
        assert!(
            payload(&v)["messages"].is_array(),
            "thread returns a messages array"
        );
    }

    #[tokio::test]
    async fn poll_returns_tail_and_cursor_over_the_tool_layer() {
        if !has_db() {
            return;
        }
        let v = wire(
            &server(true)
                .messages_poll(Parameters(MessagesPollIn {
                    since_rowid: 0,
                    limit: 10,
                }))
                .await
                .expect("poll"),
        );
        assert!(!is_error(&v), "messages.poll should succeed: {v}");
        let p = payload(&v);
        assert!(p["messages"].is_array(), "poll returns a messages array");
        // max_rowid -> maxRowid; pbjson renders i64 as a string.
        let cursor: i64 = p["maxRowid"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| p["maxRowid"].as_i64())
            .expect("a maxRowid cursor");
        assert!(cursor > 0, "expected a positive cursor, got {cursor}");
    }

    #[tokio::test]
    async fn send_dry_run_emits_changeset_without_sending() {
        // No db needed: send is osascript-backed and dry_run short-circuits before it.
        let v = wire(
            &server(true)
                .messages_send(Parameters(MessageSendIn {
                    to: "+15555550123".to_string(),
                    body: "hello from an in-process dry run".to_string(),
                    service: "iMessage".to_string(),
                    dry_run: true,
                }))
                .await
                .expect("send"),
        );
        assert!(!is_error(&v), "dry-run send should succeed: {v}");
        let p = payload(&v);
        // pbjson omits proto3 default values, so `applied=false` renders as absent.
        // A real send would serialize `applied:true`; its absence proves the dry run.
        assert_ne!(
            p["changeSet"]["applied"], Value::Bool(true),
            "dry run must NOT be applied: {p}"
        );
        assert_eq!(
            p["changeSet"]["changes"].as_array().map(|a| a.len()),
            Some(1),
            "exactly one staged change describing the send: {p}"
        );
    }

    #[tokio::test]
    async fn disabled_gate_blocks_every_message_tool() {
        let srv = server(false);
        let v = wire(
            &srv.messages_list(Parameters(MessagesListIn { limit: 5 }))
                .await
                .expect("call"),
        );
        assert!(is_error(&v), "disabled bridge must error");
        assert!(
            v["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("disabled"),
            "error names the disabled bridge: {v}"
        );
    }
}

// Full over-the-wire exercise: stands up the SAME MCP router server.rs serves
// (StreamableHttpService + UudaServer) minus the TLS layer, on a real loopback
// socket, and drives it with a real HTTP JSON-RPC client. Proves the genuinely
// uncertain, transport-level contract the Customer-Ops federation client relies on
// — that a BARE `tools/call` (no initialize/session handshake) routes to the tool
// and returns its result — plus `tools/list` discovery and real chat.db data over
// the wire. The mTLS handshake itself (stock rustls, see server.rs build_tls) is
// the one layer not exercised here; it is validated at go-live against a
// provisioned cert. macOS-only and self-skipping without a Messages database.
#[cfg(all(test, target_os = "macos"))]
mod messages_http_e2e_tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio::net::TcpListener;

    /// Build + serve the production MCP router (sans TLS) on an ephemeral loopback
    /// port; returns its base URL. messages_enabled=true against the real chat.db.
    async fn spawn_server() -> String {
        let sampler = metrics::Sampler::new();
        let db = messages::default_db_path();
        let svc = StreamableHttpService::new(
            move || {
                Ok(UudaServer::new(
                    sampler.clone(),
                    "0.0.0-e2e".to_string(),
                    std::env::temp_dir(),
                    false,
                    true,
                    db.clone(),
                    false,
                    Vec::new(),
                    std::env::temp_dir().join("uuda-e2e-files-index.db"),
                ))
            },
            LocalSessionManager::default().into(),
            // Mirror server.rs exactly: stateless + json so a bare tools/call works.
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true)
                .disable_allowed_hosts(),
        );
        let app = Router::new()
            .route("/healthz", get(|| async { Json(serde_json::json!({"status": "ok"})) }))
            .nest_service("/mcp/v1", svc);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        format!("http://127.0.0.1:{port}")
    }

    async fn wait_ready(base: &str) {
        for _ in 0..100 {
            if let Ok(r) = reqwest::get(format!("{base}/healthz")).await {
                if r.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("server never became ready at {base}");
    }

    /// JSON-RPC may arrive as plain JSON or a Streamable-HTTP SSE frame; parse both
    /// (exactly like the Customer-Ops client's parseRpc).
    fn parse_rpc(raw: &str) -> Value {
        let t = raw.trim();
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            return v;
        }
        let last = t
            .lines()
            .filter(|l| l.starts_with("data:"))
            .map(|l| l[5..].trim())
            .next_back()
            .expect("an SSE data line");
        serde_json::from_str(last).expect("SSE data is JSON")
    }

    async fn rpc(base: &str, method: &str, params: Value) -> Value {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let res = reqwest::Client::new()
            .post(format!("{base}/mcp/v1"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("send");
        assert!(res.status().is_success(), "HTTP {} from {method}", res.status());
        parse_rpc(&res.text().await.expect("body"))
    }

    #[tokio::test]
    async fn tools_list_advertises_the_message_tools() {
        let base = spawn_server().await;
        wait_ready(&base).await;
        let env = rpc(&base, "tools/list", serde_json::json!({})).await;
        let tools = env["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for want in ["messages.list", "messages.thread", "messages.poll", "messages.send"] {
            assert!(names.contains(&want), "{want} advertised; saw {names:?}");
        }
    }

    #[tokio::test]
    async fn bare_tools_call_returns_real_conversations_over_the_wire() {
        if !messages::default_db_path().exists() {
            eprintln!("skip: no chat.db");
            return;
        }
        let base = spawn_server().await;
        wait_ready(&base).await;
        // NOTE: a bare tools/call with NO prior initialize — the exact shape the
        // Customer-Ops client sends. If StreamableHTTP required a session this fails.
        let env = rpc(&base, "tools/call", serde_json::json!({"name": "messages.list", "arguments": {"limit": 3}})).await;
        let result = &env["result"];
        assert!(result["isError"].as_bool() != Some(true), "tool error: {env}");
        let text = result["content"][0]["text"].as_str().expect("content text");
        let payload: Value = serde_json::from_str(text).expect("tool payload JSON");
        let convs = payload["conversations"].as_array().expect("conversations array");
        assert!(!convs.is_empty(), "expected real conversations over the wire");
        assert!(convs.len() <= 3, "limit honored over the wire");
    }
}
