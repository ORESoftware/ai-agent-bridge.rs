//! Backward compatibility with the retired `ai-agent-bridge-rs` claude-inbox
//! bridge. This service is a superset, but keeps that bridge's exact wire
//! contract — `GET /health`, `POST /claude` (Bearer), and the `inbox.jsonl` line
//! format — so existing senders and the Claude-side watcher keep working. The
//! route handlers live in [`crate::http`]; this module is the inbox file I/O.

use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One `inbox.jsonl` line. A typed struct (not a `serde_json::Value`) so the key
/// order is exactly `id, ts, from, topic, prompt` — a `Value` serializes with
/// sorted keys by default, which would silently change the legacy line format.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InboxLine {
    pub id: u64,
    pub ts: String,
    pub from: String,
    pub topic: String,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxFile {
    Inbox,
    Outbox,
}

impl BoxFile {
    fn file_name(self) -> &'static str {
        match self {
            BoxFile::Inbox => "inbox.jsonl",
            BoxFile::Outbox => "outbox.jsonl",
        }
    }
}

pub fn inbox_path(dir: &Path) -> PathBuf {
    box_path(dir, BoxFile::Inbox)
}

pub fn box_path(dir: &Path, file: BoxFile) -> PathBuf {
    dir.join(file.file_name())
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
    append_box(dir, BoxFile::Inbox, msg)
}

pub fn append_box<T: Serialize>(dir: &Path, file: BoxFile, msg: &T) -> std::io::Result<()> {
    create_dir_all(dir)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(box_path(dir, file))?;
    let mut line = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    f.write_all(line.as_bytes())
}

/// Read JSONL messages with `id > since`. Malformed lines are skipped rather
/// than making the bridge unreadable after one bad manual append.
pub fn read_box_since(
    dir: &Path,
    file: BoxFile,
    since: Option<u64>,
) -> std::io::Result<Vec<InboxLine>> {
    let path = box_path(dir, file);
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let since = since.unwrap_or(0);
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if let Ok(msg) = serde_json::from_str::<InboxLine>(&line) {
            if msg.id > since {
                out.push(msg);
            }
        }
    }
    Ok(out)
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
