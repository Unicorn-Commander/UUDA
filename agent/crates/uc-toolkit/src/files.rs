// SPDX-License-Identifier: Apache-2.0

//! On-device text-file index + search, exposed as the `files.*` MCP tools.
//!
//! The device indexes its own text files into a local SQLite **FTS5** index so
//! Center Deep can federate keyword file search across devices. There is no
//! on-device embedder — FTS5 keyword search only (BM25 ranked).
//!
//! Storage is a single SQLite database in the agent state dir:
//!   * `files_fts(path UNINDEXED, content)` — FTS5 virtual table (the search index)
//!   * `files_meta(path PRIMARY KEY, mtime_unix, size_bytes)` — per-file metadata
//!   * `files_index_state(key, value)` — small key/value (last indexed unix ts)
//!
//! The connection is opened inside each call (never held across an `.await`);
//! the MCP layer wraps these blocking calls in `spawn_blocking`. rusqlite (with
//! bundled SQLite) is a cross-platform dependency, so the index works on every
//! device in the fleet — the Linux fleet included, not just macOS.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::ToolResult;

/// Files larger than this are skipped (text index, not a blob store).
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// Directory names we never descend into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    "dist",
    "build",
    ".next",
];

/// File extensions we treat as text and index.
const TEXT_EXTS: &[&str] = &[
    "md", "txt", "markdown", "rst", "org", "csv", "tsv", "json", "yaml", "yml", "toml", "ini",
    "cfg", "conf", "xml", "html", "htm", "log", "rs", "py", "js", "ts", "tsx", "jsx", "go", "java",
    "c", "h", "cpp", "hpp", "cs", "rb", "php", "sh", "sql", "tex",
];

// ---- response types (plain serde, NOT proto) -------------------------------

#[derive(Debug, Serialize)]
pub struct FileHit {
    pub path: String,
    pub snippet: String,
    pub score: f64,
    pub modified_unix: Option<i64>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FilesSearchResult {
    pub hits: Vec<FileHit>,
    pub total_matched: i64,
    pub indexed_files: i64,
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct FilesIndexResult {
    pub indexed_files: i64,
    pub skipped: i64,
    pub roots: Vec<String>,
    pub elapsed_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct FilesStatusResult {
    pub roots: Vec<String>,
    pub indexed_files: i64,
    pub total_bytes: i64,
    pub last_indexed_unix: Option<i64>,
    pub enabled: bool,
}

/// A local FTS5-backed file index. Owns nothing across calls but its db path;
/// each method opens a fresh connection (cheap; SQLite file).
#[derive(Debug, Clone)]
pub struct FilesIndex {
    db_path: PathBuf,
}

impl FilesIndex {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Recursively index every text file under `roots`. Files whose
    /// `(mtime, size)` are unchanged are skipped unless `force`.
    pub fn index(&self, roots: &[PathBuf], force: bool) -> ToolResult<FilesIndexResult> {
        imp::index(&self.db_path, roots, force)
    }

    /// FTS5 MATCH search, BM25-ranked, optional `path_prefix` filter.
    pub fn search(
        &self,
        query: &str,
        limit: u32,
        path_prefix: Option<&str>,
    ) -> ToolResult<FilesSearchResult> {
        let limit = clamp_limit(limit);
        imp::search(&self.db_path, query, limit, path_prefix)
    }

    /// Index status: file count, total bytes, last-indexed timestamp.
    pub fn status(&self, roots: &[PathBuf], enabled: bool) -> ToolResult<FilesStatusResult> {
        imp::status(&self.db_path, roots, enabled)
    }
}

/// Clamp a requested limit into 1..=50 (0 -> default 10).
fn clamp_limit(requested: u32) -> u32 {
    match requested {
        0 => 10,
        n => n.clamp(1, 50),
    }
}

/// Sanitize raw user input into a safe FTS5 MATCH expression.
///
/// FTS5 MATCH has its own query syntax (`AND`, `OR`, `NEAR`, `(`, `*`, `:`,
/// `-`, `"` …); raw input can be a syntax error. We split on whitespace, strip
/// FTS5-special characters from each term, wrap the survivors in double quotes
/// (so each is a literal phrase token), and join with spaces (implicit AND).
/// Returns `None` when nothing usable remains — the caller then short-circuits
/// to an empty result instead of running a malformed MATCH.
fn sanitize_fts_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !cleaned.is_empty() {
            terms.push(format!("\"{cleaned}\""));
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

// ---- implementation --------------------------------------------------------

mod imp {
    use super::{
        sanitize_fts_query, FileHit, FilesIndexResult, FilesSearchResult, FilesStatusResult,
        MAX_FILE_BYTES, SKIP_DIRS, TEXT_EXTS,
    };
    use crate::error::{ToolError, ToolResult};
    use rusqlite::{Connection, OptionalExtension};
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn db_err(e: rusqlite::Error) -> ToolError {
        ToolError::Backend(format!("files index db: {e}"))
    }

    /// Open (create if missing) the index db and ensure the schema exists.
    fn open(db_path: &Path) -> ToolResult<Connection> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Backend(format!("create state dir: {e}")))?;
        }
        let conn = Connection::open(db_path)
            .map_err(|e| ToolError::Backend(format!("open files index ({}): {e}", db_path.display())))?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(path UNINDEXED, content);
             CREATE TABLE IF NOT EXISTS files_meta (
                 path TEXT PRIMARY KEY,
                 mtime_unix INTEGER,
                 size_bytes INTEGER
             );
             CREATE TABLE IF NOT EXISTS files_index_state (
                 key TEXT PRIMARY KEY,
                 value TEXT
             );",
        )
        .map_err(db_err)?;
        Ok(conn)
    }

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn is_text_ext(path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let lower = ext.to_ascii_lowercase();
                TEXT_EXTS.contains(&lower.as_str())
            }
            None => false,
        }
    }

    /// Recursive, symlink-safe walker. Collects regular text files (by
    /// extension) under `root`, skipping hidden dirs / known noise dirs and
    /// anything over the size cap. Symlinks are not followed (loop-safe).
    fn collect_files(root: &Path, out: &mut Vec<(PathBuf, i64, i64)>) {
        // Bounded recursion via an explicit stack.
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue, // unreadable dir — skip, don't fail the whole index
            };
            for entry in entries.flatten() {
                let path = entry.path();
                // symlink_metadata so we never follow symlinks (loop-safe).
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.file_type().is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if meta.is_dir() {
                    if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                        continue;
                    }
                    stack.push(path);
                } else if meta.is_file() {
                    if name.starts_with('.') {
                        continue; // hidden files
                    }
                    if !is_text_ext(&path) {
                        continue;
                    }
                    let size = meta.len();
                    if size > MAX_FILE_BYTES {
                        continue;
                    }
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    out.push((path, mtime, size as i64));
                }
            }
        }
    }

    pub fn index(db_path: &Path, roots: &[PathBuf], force: bool) -> ToolResult<FilesIndexResult> {
        let start = Instant::now();
        let mut conn = open(db_path)?;

        let mut candidates: Vec<(PathBuf, i64, i64)> = Vec::new();
        for root in roots {
            if root.is_dir() {
                collect_files(root, &mut candidates);
            }
        }

        let mut indexed_files: i64 = 0;
        let mut skipped: i64 = 0;

        let tx = conn.transaction().map_err(db_err)?;
        for (path, mtime, size) in &candidates {
            let path_str = path.display().to_string();

            if !force {
                let unchanged: Option<bool> = tx
                    .query_row(
                        "SELECT mtime_unix = ?2 AND size_bytes = ?3 FROM files_meta WHERE path = ?1",
                        rusqlite::params![path_str, mtime, size],
                        |r| r.get::<_, i64>(0).map(|v| v != 0),
                    )
                    .optional()
                    .map_err(db_err)?;
                if unchanged == Some(true) {
                    skipped += 1;
                    continue;
                }
            }

            let content = match std::fs::read(path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };

            // Upsert: drop any prior FTS rows for this path, then reinsert.
            tx.execute("DELETE FROM files_fts WHERE path = ?1", rusqlite::params![path_str])
                .map_err(db_err)?;
            tx.execute(
                "INSERT INTO files_fts (path, content) VALUES (?1, ?2)",
                rusqlite::params![path_str, content],
            )
            .map_err(db_err)?;
            tx.execute(
                "INSERT INTO files_meta (path, mtime_unix, size_bytes) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(path) DO UPDATE SET mtime_unix = ?2, size_bytes = ?3",
                rusqlite::params![path_str, mtime, size],
            )
            .map_err(db_err)?;
            indexed_files += 1;
        }

        tx.execute(
            "INSERT INTO files_index_state (key, value) VALUES ('last_indexed_unix', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            rusqlite::params![now_unix().to_string()],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;

        Ok(FilesIndexResult {
            indexed_files,
            skipped,
            roots: roots.iter().map(|p| p.display().to_string()).collect(),
            elapsed_ms: start.elapsed().as_millis() as i64,
        })
    }

    pub fn search(
        db_path: &Path,
        query: &str,
        limit: u32,
        path_prefix: Option<&str>,
    ) -> ToolResult<FilesSearchResult> {
        let conn = open(db_path)?;
        let indexed_files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files_meta", [], |r| r.get(0))
            .map_err(db_err)?;

        // Sanitize. A malformed / empty query yields zero hits, never an error.
        let Some(match_expr) = sanitize_fts_query(query) else {
            return Ok(FilesSearchResult {
                hits: Vec::new(),
                total_matched: 0,
                indexed_files,
                query: query.to_string(),
            });
        };

        // bm25(): lower is better. We expose score = -bm25 so higher = better.
        // snippet(files_fts, 1, ...) snippets the `content` column (index 1).
        let sql = "SELECT f.path, \
                          snippet(files_fts, 1, '', '', ' … ', 12) AS snip, \
                          -bm25(files_fts) AS score, \
                          m.mtime_unix, m.size_bytes \
                   FROM files_fts f \
                   LEFT JOIN files_meta m ON m.path = f.path \
                   WHERE files_fts MATCH ?1 \
                     AND (?2 IS NULL OR f.path LIKE ?2 || '%') \
                   ORDER BY score DESC \
                   LIMIT ?3";

        let mut stmt = conn.prepare(sql).map_err(db_err)?;
        let prefix: Option<&str> = path_prefix.filter(|p| !p.is_empty());
        let rows = stmt
            .query_map(
                rusqlite::params![match_expr, prefix, limit],
                |row| {
                    Ok(FileHit {
                        path: row.get(0)?,
                        snippet: row.get(1)?,
                        score: row.get(2)?,
                        modified_unix: row.get(3)?,
                        size_bytes: row.get(4)?,
                    })
                },
            )
            .map_err(db_err)?;

        let mut hits: Vec<FileHit> = Vec::new();
        for r in rows {
            hits.push(r.map_err(db_err)?);
        }
        let total_matched = hits.len() as i64;

        Ok(FilesSearchResult {
            hits,
            total_matched,
            indexed_files,
            query: query.to_string(),
        })
    }

    pub fn status(
        db_path: &Path,
        roots: &[PathBuf],
        enabled: bool,
    ) -> ToolResult<FilesStatusResult> {
        let conn = open(db_path)?;
        let indexed_files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files_meta", [], |r| r.get(0))
            .map_err(db_err)?;
        let total_bytes: i64 = conn
            .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM files_meta", [], |r| r.get(0))
            .map_err(db_err)?;
        let last_indexed_unix: Option<i64> = conn
            .query_row(
                "SELECT value FROM files_index_state WHERE key = 'last_indexed_unix'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_err)?
            .and_then(|s| s.parse::<i64>().ok());

        Ok(FilesStatusResult {
            roots: roots.iter().map(|p| p.display().to_string()).collect(),
            indexed_files,
            total_bytes,
            last_indexed_unix,
            enabled,
        })
    }
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique temp dir under the OS temp dir (no external tempfile dep needed).
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("uuda-files-test-{tag}-{nanos}"));
            fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }
        fn path(&self) -> &PathBuf {
            &self.0
        }
        fn db(&self) -> PathBuf {
            self.0.join("index.db")
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn index_and_search_finds_the_right_file() {
        let tmp = TempRoot::new("search");
        let root = tmp.path().join("docs");
        write(&root.join("a.md"), "the sovereign privacy moat is our edge");
        write(&root.join("b.md"), "completely unrelated content about pizza");
        write(&root.join("c.txt"), "another note on supply chains");

        let idx = FilesIndex::new(tmp.db());
        let res = idx.index(&[root.clone()], false).expect("index");
        assert_eq!(res.indexed_files, 3, "all three text files indexed");

        let out = idx.search("sovereign", 10, None).expect("search");
        assert_eq!(out.indexed_files, 3);
        assert_eq!(out.hits.len(), 1, "exactly one file matches");
        let hit = &out.hits[0];
        assert!(hit.path.ends_with("a.md"), "matched a.md, got {}", hit.path);
        assert!(
            hit.snippet.to_lowercase().contains("sovereign"),
            "snippet contains the term: {}",
            hit.snippet
        );
        assert!(hit.score > 0.0, "score is positive, got {}", hit.score);
        assert!(hit.size_bytes.unwrap() > 0);
    }

    #[test]
    fn path_prefix_narrows_to_subtree() {
        let tmp = TempRoot::new("prefix");
        let root = tmp.path().join("tree");
        let alpha = root.join("alpha");
        let beta = root.join("beta");
        write(&alpha.join("one.md"), "shared keyword apple here");
        write(&beta.join("two.md"), "shared keyword apple there");

        let idx = FilesIndex::new(tmp.db());
        idx.index(&[root.clone()], false).expect("index");

        let all = idx.search("apple", 10, None).expect("search all");
        assert_eq!(all.hits.len(), 2, "both files match without a prefix");

        let prefix = alpha.display().to_string();
        let scoped = idx.search("apple", 10, Some(&prefix)).expect("search scoped");
        assert_eq!(scoped.hits.len(), 1, "prefix narrows to one subtree");
        assert!(scoped.hits[0].path.contains("alpha"));
    }

    #[test]
    fn malformed_query_does_not_error_or_panic() {
        let tmp = TempRoot::new("malformed");
        let root = tmp.path().join("docs");
        write(&root.join("a.md"), "harmless content");
        let idx = FilesIndex::new(tmp.db());
        idx.index(&[root], false).expect("index");

        // FTS5 special syntax that would be a raw MATCH error if unsanitized.
        for q in ["a AND ( b", "\"unterminated", "* NEAR/", "-:^", "   "] {
            let res = idx.search(q, 10, None);
            assert!(res.is_ok(), "query {q:?} must not error: {res:?}");
        }
    }

    #[test]
    fn status_reports_counts() {
        let tmp = TempRoot::new("status");
        let root = tmp.path().join("docs");
        write(&root.join("a.md"), "one");
        write(&root.join("b.md"), "two");
        write(&root.join("c.md"), "three");

        let idx = FilesIndex::new(tmp.db());
        idx.index(&[root.clone()], false).expect("index");

        let st = idx.status(&[root], true).expect("status");
        assert_eq!(st.indexed_files, 3);
        assert!(st.total_bytes > 0, "total bytes accumulated");
        assert!(st.last_indexed_unix.is_some(), "last_indexed recorded");
        assert!(st.enabled);
    }

    #[test]
    fn reindex_without_force_skips_unchanged() {
        let tmp = TempRoot::new("reindex");
        let root = tmp.path().join("docs");
        write(&root.join("a.md"), "stable content one");
        write(&root.join("b.md"), "stable content two");

        let idx = FilesIndex::new(tmp.db());
        let first = idx.index(&[root.clone()], false).expect("index 1");
        assert_eq!(first.indexed_files, 2);
        assert_eq!(first.skipped, 0);

        let second = idx.index(&[root], false).expect("index 2");
        assert_eq!(second.indexed_files, 0, "nothing changed, nothing reindexed");
        assert_eq!(second.skipped, 2, "both unchanged files skipped");
    }
}
