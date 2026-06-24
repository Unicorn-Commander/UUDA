// SPDX-License-Identifier: Apache-2.0

//! On-device bridge to the macOS Messages app (iMessage + SMS-via-Continuity).
//!
//! Reads are sourced from the local `~/Library/Messages/chat.db`, opened
//! read-only and immutable so we never lock or mutate the live database.
//! Sends go out through AppleScript to Messages.app. macOS only; every entry
//! point returns a `Backend` error on other platforms so the crate still
//! compiles for headless Linux servers.
//!
//! `messages.send` is a mutating tool: callers pass `dry_run` (defaulted true
//! at the gateway) and the response carries a `ChangeSet` (`applied=false` on
//! a dry run). Reads are blocking SQLite calls — the MCP layer wraps them in
//! `spawn_blocking`.

use std::path::{Path, PathBuf};

use crate::error::ToolResult;

#[cfg(not(target_os = "macos"))]
use crate::error::ToolError;

const LIST_DEFAULT: u32 = 20;
const LIST_CAP: u32 = 200;
const THREAD_DEFAULT: u32 = 50;
const THREAD_CAP: u32 = 500;
const POLL_DEFAULT: u32 = 100;
const POLL_CAP: u32 = 1000;

/// Resolve the default Messages database for the invoking user. When the agent
/// runs as a non-root user (dev) this is correct; a root daemon should pass an
/// explicit path to the target user's home.
pub fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/root".to_string());
    Path::new(&home).join("Library/Messages/chat.db")
}

// ---- public entry points (cfg-split bodies; identical signatures per OS) ----

/// Most-recent conversations, newest first, each summarised by its last message.
pub fn list_conversations(db_path: &Path, limit: u32) -> ToolResult<uc_types::MessagesListResponse> {
    let limit = clamp(limit, LIST_DEFAULT, LIST_CAP);
    #[cfg(target_os = "macos")]
    {
        imp::list_conversations(db_path, limit)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (db_path, limit);
        Err(unsupported())
    }
}

/// One conversation's messages, oldest -> newest (the newest `limit` are kept).
pub fn read_thread(
    db_path: &Path,
    chat_identifier: &str,
    limit: u32,
) -> ToolResult<uc_types::MessagesThreadResponse> {
    let limit = clamp(limit, THREAD_DEFAULT, THREAD_CAP);
    #[cfg(target_os = "macos")]
    {
        imp::read_thread(db_path, chat_identifier, limit)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (db_path, chat_identifier, limit);
        Err(unsupported())
    }
}

/// Messages with ROWID greater than `since_rowid` (ascending). `since_rowid=0`
/// returns the recent tail so a fresh caller can establish a cursor.
pub fn poll_since(
    db_path: &Path,
    since_rowid: i64,
    limit: u32,
) -> ToolResult<uc_types::MessagesPollResponse> {
    let limit = clamp(limit, POLL_DEFAULT, POLL_CAP);
    #[cfg(target_os = "macos")]
    {
        imp::poll_since(db_path, since_rowid, limit)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (db_path, since_rowid, limit);
        Err(unsupported())
    }
}

/// Send a message via AppleScript. `dry_run` returns a ChangeSet without sending.
pub async fn send(
    to: &str,
    body: &str,
    service: &str,
    dry_run: bool,
) -> ToolResult<uc_types::MessageSendResponse> {
    #[cfg(target_os = "macos")]
    {
        imp::send(to, body, service, dry_run).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (to, body, service, dry_run);
        Err(unsupported())
    }
}

fn clamp(requested: u32, default: u32, cap: u32) -> u32 {
    if requested == 0 {
        default
    } else {
        requested.min(cap)
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> ToolError {
    ToolError::Backend("messages bridge is macOS-only (needs the local Messages app)".into())
}

// ---- pure helpers (platform-independent so they unit-test anywhere) ----

/// Convert a chat.db `date` to a protobuf Timestamp. Modern macOS stores
/// nanoseconds since the Apple epoch (2001-01-01); very old rows store seconds.
fn apple_date_to_timestamp(date: i64) -> pbjson_types::Timestamp {
    const APPLE_EPOCH_UNIX: i64 = 978_307_200;
    let (secs_since_2001, nanos) = if date.abs() > 1_000_000_000_000 {
        (date / 1_000_000_000, (date % 1_000_000_000) as i32)
    } else {
        (date, 0)
    };
    pbjson_types::Timestamp {
        seconds: secs_since_2001 + APPLE_EPOCH_UNIX,
        nanos,
    }
}

/// Resolve a message body. Modern macOS frequently leaves `text` NULL and keeps
/// the body in `attributedBody` (an NSAttributedString typedstream blob).
fn decode_body(text: Option<String>, attributed: Option<Vec<u8>>) -> String {
    if let Some(t) = text {
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(blob) = attributed {
        if let Some(s) = extract_typedstream_text(&blob) {
            return s;
        }
    }
    String::new()
}

/// Best-effort extraction of the body string from an `attributedBody` blob.
/// The text follows the `NSString` class marker as a length-prefixed UTF-8 run
/// introduced by a `0x2b` ('+') byte; lengths >= 128 use a `0x81 <u16-le>`
/// prefix. Returns None for attachment-only / unparseable blobs.
/// TODO: replace with a full typedstream parser for mentions/links fidelity.
fn extract_typedstream_text(blob: &[u8]) -> Option<String> {
    let marker = b"NSString";
    let after = find_subslice(blob, marker)? + marker.len();
    let window_end = (after + 16).min(blob.len());
    let plus_rel = blob[after..window_end].iter().position(|&b| b == 0x2b)?;
    let mut j = after + plus_rel + 1;
    if j >= blob.len() {
        return None;
    }
    let len = if blob[j] == 0x81 {
        if j + 3 > blob.len() {
            return None;
        }
        let l = u16::from_le_bytes([blob[j + 1], blob[j + 2]]) as usize;
        j += 3;
        l
    } else {
        let l = blob[j] as usize;
        j += 1;
        l
    };
    let end = (j + len).min(blob.len());
    let s = String::from_utf8_lossy(&blob[j..end]).into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---- macOS implementation ----

#[cfg(target_os = "macos")]
mod imp {
    use super::{apple_date_to_timestamp, decode_body};
    use crate::error::{ToolError, ToolResult};
    use rusqlite::{Connection, OpenFlags};
    use std::path::Path;

    fn open_ro(db_path: &Path) -> ToolResult<Connection> {
        let uri = format!("file:{}?immutable=1", db_path.display());
        Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ToolError::Backend(format!("open chat.db ({}): {e}", db_path.display())))
    }

    fn db_err(e: rusqlite::Error) -> ToolError {
        ToolError::Backend(format!("chat.db query: {e}"))
    }

    // Columns selected for a message row, in order, by every query below.
    const MSG_COLS: &str = "m.ROWID, m.date, m.is_from_me, m.text, m.attributedBody, \
                            m.service, m.cache_has_attachments, h.id";

    fn row_to_message(row: &rusqlite::Row) -> rusqlite::Result<uc_types::ChatMessage> {
        let rowid: i64 = row.get(0)?;
        let date: i64 = row.get(1)?;
        let is_from_me: i64 = row.get(2)?;
        let text: Option<String> = row.get(3)?;
        let attributed: Option<Vec<u8>> = row.get(4)?;
        let service: Option<String> = row.get(5)?;
        let has_attach: Option<i64> = row.get(6)?;
        let handle: Option<String> = row.get(7)?;
        Ok(uc_types::ChatMessage {
            rowid,
            at: Some(apple_date_to_timestamp(date)),
            from_me: is_from_me != 0,
            sender: handle.unwrap_or_default(),
            text: decode_body(text, attributed),
            service: service.unwrap_or_else(|| "iMessage".to_string()),
            has_attachments: has_attach.unwrap_or(0) != 0,
        })
    }

    pub fn list_conversations(
        db_path: &Path,
        limit: u32,
    ) -> ToolResult<uc_types::MessagesListResponse> {
        let conn = open_ro(db_path)?;

        // Newest activity per chat, capped.
        let mut top = conn
            .prepare(
                "SELECT cmj.chat_id, MAX(m.date) AS maxdate \
                 FROM chat_message_join cmj JOIN message m ON m.ROWID = cmj.message_id \
                 GROUP BY cmj.chat_id ORDER BY maxdate DESC LIMIT ?1",
            )
            .map_err(db_err)?;
        let chat_ids: Vec<i64> = top
            .query_map([limit], |r| r.get::<_, i64>(0))
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;

        let detail_sql = format!(
            "SELECT c.chat_identifier, c.display_name, c.service_name, c.style, {MSG_COLS} \
             FROM chat c \
             JOIN chat_message_join cmj ON cmj.chat_id = c.ROWID \
             JOIN message m ON m.ROWID = cmj.message_id \
             LEFT JOIN handle h ON h.ROWID = m.handle_id \
             WHERE c.ROWID = ?1 ORDER BY m.date DESC LIMIT 1"
        );
        let mut detail = conn.prepare(&detail_sql).map_err(db_err)?;

        let mut conversations = Vec::with_capacity(chat_ids.len());
        for chat_rowid in chat_ids {
            let conv = detail.query_row([chat_rowid], |row| {
                let chat_identifier: Option<String> = row.get(0)?;
                let display_name: Option<String> = row.get(1)?;
                let service_name: Option<String> = row.get(2)?;
                let style: Option<i64> = row.get(3)?;
                // message columns start at index 4
                let msg = uc_types::ChatMessage {
                    rowid: row.get(4)?,
                    at: Some(apple_date_to_timestamp(row.get(5)?)),
                    from_me: row.get::<_, i64>(6)? != 0,
                    sender: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                    text: decode_body(row.get(7)?, row.get(8)?),
                    service: row
                        .get::<_, Option<String>>(9)?
                        .unwrap_or_else(|| "iMessage".to_string()),
                    has_attachments: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
                };
                let last_activity = msg.at;
                Ok(uc_types::Conversation {
                    chat_rowid,
                    chat_identifier: chat_identifier.unwrap_or_default(),
                    display_name: display_name.unwrap_or_default(),
                    service: service_name.unwrap_or_else(|| "iMessage".to_string()),
                    is_group: style.unwrap_or(45) == 43,
                    last_message: Some(msg),
                    last_activity,
                })
            });
            match conv {
                Ok(c) => conversations.push(c),
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(e) => return Err(db_err(e)),
            }
        }

        Ok(uc_types::MessagesListResponse {
            conversations,
            error: None,
        })
    }

    pub fn read_thread(
        db_path: &Path,
        chat_identifier: &str,
        limit: u32,
    ) -> ToolResult<uc_types::MessagesThreadResponse> {
        if chat_identifier.is_empty() {
            return Err(ToolError::InvalidInput("chat_identifier is required".into()));
        }
        let conn = open_ro(db_path)?;
        let sql = format!(
            "SELECT {MSG_COLS} FROM message m \
             JOIN chat_message_join cmj ON cmj.message_id = m.ROWID \
             JOIN chat c ON c.ROWID = cmj.chat_id \
             LEFT JOIN handle h ON h.ROWID = m.handle_id \
             WHERE c.chat_identifier = ?1 ORDER BY m.date DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let mut messages: Vec<uc_types::ChatMessage> = stmt
            .query_map(rusqlite::params![chat_identifier, limit], row_to_message)
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        messages.reverse(); // newest-first query -> chronological
        Ok(uc_types::MessagesThreadResponse {
            messages,
            error: None,
        })
    }

    pub fn poll_since(
        db_path: &Path,
        since_rowid: i64,
        limit: u32,
    ) -> ToolResult<uc_types::MessagesPollResponse> {
        let conn = open_ro(db_path)?;
        let mut messages: Vec<uc_types::ChatMessage> = if since_rowid > 0 {
            let sql = format!(
                "SELECT {MSG_COLS} FROM message m \
                 LEFT JOIN handle h ON h.ROWID = m.handle_id \
                 WHERE m.ROWID > ?1 ORDER BY m.ROWID ASC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql).map_err(db_err)?;
            let v = stmt
                .query_map(rusqlite::params![since_rowid, limit], row_to_message)
                .map_err(db_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_err)?;
            v
        } else {
            // fresh caller: most-recent tail, returned ascending
            let sql = format!(
                "SELECT {MSG_COLS} FROM message m \
                 LEFT JOIN handle h ON h.ROWID = m.handle_id \
                 ORDER BY m.ROWID DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql).map_err(db_err)?;
            let mut v: Vec<uc_types::ChatMessage> = stmt
                .query_map([limit], row_to_message)
                .map_err(db_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_err)?;
            v.reverse();
            v
        };

        // Cursor: max ROWID returned, or the table max when nothing new.
        let max_rowid = match messages.iter().map(|m| m.rowid).max() {
            Some(m) => m,
            None => conn
                .query_row("SELECT COALESCE(MAX(ROWID), 0) FROM message", [], |r| r.get(0))
                .map_err(db_err)?,
        };
        messages.shrink_to_fit();

        Ok(uc_types::MessagesPollResponse {
            messages,
            max_rowid,
            error: None,
        })
    }

    pub async fn send(
        to: &str,
        body: &str,
        service: &str,
        dry_run: bool,
    ) -> ToolResult<uc_types::MessageSendResponse> {
        if to.trim().is_empty() {
            return Err(ToolError::InvalidInput("recipient `to` is required".into()));
        }
        if body.is_empty() {
            return Err(ToolError::InvalidInput("message `body` is required".into()));
        }
        let svc_label = if service.eq_ignore_ascii_case("sms") {
            "SMS"
        } else {
            "iMessage"
        };
        let change = uc_types::Change {
            kind: "messages.send".to_string(),
            target: to.to_string(),
            before: String::new(),
            after: format!("send {} chars via {svc_label}", body.chars().count()),
        };
        if dry_run {
            return Ok(uc_types::MessageSendResponse {
                change_set: Some(uc_types::ChangeSet {
                    changes: vec![change],
                    applied: false,
                }),
                service_used: svc_label.to_string(),
                error: None,
            });
        }

        let script = format!(
            "tell application \"Messages\"\n  \
             set svc to 1st account whose service type = iMessage\n  \
             set theBuddy to participant \"{to}\" of svc\n  \
             send \"{body}\" to theBuddy\nend tell",
            to = esc_applescript(to),
            body = esc_applescript(body),
        );
        let out = tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .await
            .map_err(|e| ToolError::Backend(format!("osascript: {e}")))?;
        if !out.status.success() {
            return Err(ToolError::Backend(format!(
                "osascript send failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(uc_types::MessageSendResponse {
            change_set: Some(uc_types::ChangeSet {
                changes: vec![change],
                applied: true,
            }),
            service_used: svc_label.to_string(),
            error: None,
        })
    }

    /// Escape a string for an AppleScript double-quoted literal. Newlines are
    /// flattened to spaces (a raw newline would terminate the literal).
    fn esc_applescript(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn lists_recent_conversations_from_real_db() {
        let db = default_db_path();
        if !db.exists() {
            eprintln!("skip: no chat.db at {}", db.display());
            return;
        }
        let resp = list_conversations(&db, 5).expect("list_conversations");
        assert!(
            !resp.conversations.is_empty(),
            "expected at least one conversation"
        );
        assert!(resp.conversations.len() <= 5);
    }

    #[test]
    fn polls_tail_and_returns_cursor() {
        let db = default_db_path();
        if !db.exists() {
            return;
        }
        let resp = poll_since(&db, 0, 10).expect("poll_since");
        assert!(resp.max_rowid > 0, "expected a positive cursor");
        assert!(resp.messages.len() <= 10);
    }

    #[test]
    fn send_dry_run_does_not_send() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt
            .block_on(send("+15555550123", "hello from a dry run", "iMessage", true))
            .expect("dry-run send");
        let cs = resp.change_set.expect("change_set");
        assert!(!cs.applied, "dry run must not apply");
        assert_eq!(cs.changes.len(), 1);
    }
}
