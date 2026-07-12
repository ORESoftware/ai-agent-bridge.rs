//! Backward compatibility with the retired `ai-agent-bridge-rs` claude-inbox
//! bridge. This service is a superset, but keeps that bridge's exact wire
//! contract — `GET /health`, `POST /claude` (Bearer), and the `inbox.jsonl` line
//! format — so existing senders and the Claude-side watcher keep working. The
//! route handlers live in [`crate::http`]; this module is the inbox file I/O.

use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// One `inbox.jsonl` line. A typed struct (not a `serde_json::Value`) so the key
/// order is exactly `id, ts, from, topic, prompt` — a `Value` serializes with
/// sorted keys by default, which would silently change the legacy line format.
#[derive(Serialize)]
pub struct InboxLine {
    pub id: u64,
    pub ts: String,
    pub from: String,
    pub topic: String,
    pub prompt: String,
}

pub fn inbox_path(dir: &Path) -> PathBuf {
    dir.join("inbox.jsonl")
}

/// Number of lines (messages) in `inbox.jsonl`; 0 if it does not exist yet.
pub fn inbox_count(dir: &Path) -> usize {
    match File::open(inbox_path(dir)) {
        Ok(f) => BufReader::new(f).lines().count(),
        Err(_) => 0,
    }
}

/// Append one JSON line to `inbox.jsonl`, matching the legacy format.
pub fn append_inbox<T: Serialize>(dir: &Path, msg: &T) -> std::io::Result<()> {
    create_dir_all(dir)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path(dir))?;
    let mut line = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    f.write_all(line.as_bytes())
}

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC, seconds) — the legacy `inbox.jsonl` timestamp.
pub fn iso8601_secs() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Millisecond epoch — the legacy message-id basis.
pub fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Take a string field with a default and a max char length (legacy semantics).
pub fn field(data: &Value, key: &str, default: &str, max: usize) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .chars()
        .take(max)
        .collect()
}
