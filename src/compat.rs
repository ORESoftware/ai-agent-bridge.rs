//! Backward compatibility with the retired `ai-agent-bridge-rs` claude-inbox
//! bridge. This service is a superset, but keeps that bridge's exact wire
//! contract — `GET /health`, `POST /claude` (Bearer), and the `inbox.jsonl` line
//! format — so existing senders and the Claude-side watcher keep working. The
//! route handlers live in [`crate::http`]; this module is the inbox file I/O.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::fs::{DirBuilder, File, OpenOptions};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

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

/// Create and validate the private compatibility state before the server starts.
/// Unsafe pre-existing filesystem objects make startup fail closed.
#[cfg(unix)]
pub fn prepare_inbox(dir: &Path) -> std::io::Result<()> {
    let directory = secure_directory(dir)?;
    let _ = open_box_at(
        directory.as_raw_fd(),
        BoxFile::Inbox,
        libc::O_RDWR | libc::O_CREAT,
    )?;
    Ok(())
}

#[cfg(not(unix))]
pub fn prepare_inbox(_dir: &Path) -> std::io::Result<()> {
    unsupported_platform()
}

/// Number of lines (messages) in `inbox.jsonl`.
#[cfg(unix)]
pub fn inbox_count(dir: &Path) -> std::io::Result<usize> {
    let directory = secure_directory(dir)?;
    let file = open_box_at(directory.as_raw_fd(), BoxFile::Inbox, libc::O_RDONLY)?;
    BufReader::new(file)
        .lines()
        .try_fold(0usize, |count, line| {
            line?;
            Ok(count.saturating_add(1))
        })
}

#[cfg(not(unix))]
pub fn inbox_count(_dir: &Path) -> std::io::Result<usize> {
    unsupported_platform()
}

/// Append one JSON line to `inbox.jsonl`, matching the legacy format.
#[cfg(unix)]
pub fn append_inbox<T: Serialize>(dir: &Path, msg: &T) -> std::io::Result<()> {
    append_box(dir, BoxFile::Inbox, msg)
}

#[cfg(not(unix))]
pub fn append_inbox<T: Serialize>(_dir: &Path, _msg: &T) -> std::io::Result<()> {
    unsupported_platform()
}

/// Append one JSON line to the given box file through the same no-follow,
/// owner-checked descriptor path as the inbox; serialization failures are
/// errors, never silently-written placeholders.
#[cfg(unix)]
pub fn append_box<T: Serialize>(dir: &Path, file: BoxFile, msg: &T) -> std::io::Result<()> {
    let directory = secure_directory(dir)?;
    let mut f = open_box_at(
        directory.as_raw_fd(),
        file,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
    )?;
    let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    line.push('\n');
    f.write_all(line.as_bytes())
}

#[cfg(not(unix))]
pub fn append_box<T: Serialize>(_dir: &Path, _file: BoxFile, _msg: &T) -> std::io::Result<()> {
    unsupported_platform()
}

#[cfg(unix)]
fn secure_directory(dir: &Path) -> std::io::Result<File> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(invalid_state("compat inbox path is not a directory"));
    }
    require_current_owner(&metadata, "compat inbox directory")?;
    set_mode(directory.as_raw_fd(), 0o700)?;
    Ok(directory)
}

#[cfg(unix)]
fn open_box_at(directory: RawFd, file: BoxFile, flags: libc::c_int) -> std::io::Result<File> {
    let name: &CStr = match file {
        BoxFile::Inbox => c"inbox.jsonl",
        BoxFile::Outbox => c"outbox.jsonl",
    };
    // SAFETY: `directory` is an open directory fd, `name` is NUL-terminated, and
    // the returned descriptor is immediately owned by `File` on success.
    let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_state("compat box file is not a regular file"));
    }
    if metadata.nlink() != 1 {
        return Err(invalid_state("compat box file must not be hard-linked"));
    }
    require_current_owner(&metadata, "compat box file")?;
    set_mode(file.as_raw_fd(), 0o600)?;
    Ok(file)
}

#[cfg(unix)]
fn require_current_owner(metadata: &std::fs::Metadata, label: &str) -> std::io::Result<()> {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let current_uid = unsafe { libc::geteuid() };
    if metadata.uid() != current_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{label} is not owned by the current user"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(fd: RawFd, mode: libc::mode_t) -> std::io::Result<()> {
    // SAFETY: fd is open for the duration of this call and fchmod accepts any
    // valid mode_t bit pattern.
    if unsafe { libc::fchmod(fd, mode) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn invalid_state(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(not(unix))]
fn unsupported_platform<T>() -> std::io::Result<T> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure compatibility inbox storage requires Unix no-follow filesystem APIs",
    ))
}

/// Read JSONL messages with `id > since`, through the same no-follow,
/// owner-checked descriptor path as the writers. A missing box file is an
/// empty box. Malformed lines are skipped rather than making the bridge
/// unreadable after one bad manual append.
#[cfg(unix)]
pub fn read_box_since(
    dir: &Path,
    file: BoxFile,
    since: Option<u64>,
) -> std::io::Result<Vec<InboxLine>> {
    let directory = secure_directory(dir)?;
    let f = match open_box_at(directory.as_raw_fd(), file, libc::O_RDONLY) {
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

#[cfg(not(unix))]
pub fn read_box_since(
    _dir: &Path,
    _file: BoxFile,
    _since: Option<u64>,
) -> std::io::Result<Vec<InboxLine>> {
    unsupported_platform()
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("aab-compat-{label}-{nanos}-{count}"))
    }

    #[test]
    fn inbox_directory_and_file_are_private() {
        let dir = unique_path("modes");
        append_inbox(&dir, &serde_json::json!({"message":"private"})).unwrap();

        let directory_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(inbox_path(&dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert_eq!(inbox_count(&dir).unwrap(), 1);
    }

    #[test]
    fn symlinked_directory_is_refused() {
        let root = unique_path("directory-symlink");
        std::fs::create_dir_all(&root).unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();

        assert!(prepare_inbox(&link).is_err());
    }

    #[test]
    fn symlinked_or_non_regular_inbox_is_refused() {
        let symlink_dir = unique_path("file-symlink");
        let _ = secure_directory(&symlink_dir).unwrap();
        symlink(symlink_dir.join("target"), inbox_path(&symlink_dir)).unwrap();
        assert!(append_inbox(&symlink_dir, &serde_json::json!({})).is_err());

        let directory_dir = unique_path("file-directory");
        let _ = secure_directory(&directory_dir).unwrap();
        std::fs::create_dir(inbox_path(&directory_dir)).unwrap();
        assert!(append_inbox(&directory_dir, &serde_json::json!({})).is_err());

        let fifo_dir = unique_path("file-fifo");
        let _ = secure_directory(&fifo_dir).unwrap();
        let fifo = CString::new(inbox_path(&fifo_dir).as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a valid NUL-terminated path in a private test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(append_inbox(&fifo_dir, &serde_json::json!({})).is_err());
    }
}
