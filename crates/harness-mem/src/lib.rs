//! Session JSONL + kv meta storage. PLAN §3.1 / §5.11 / §8.2.
//!
//! Layout: `$XDG_STATE_HOME/harness/sessions/<session_id>.jsonl`.
//!   - `{"v":1,"schema":"harness.session",...}` header line.
//!   - Subsequent lines: `{"type":"message",...}` / `{"type":"meta",...}`.
//!   - `fs4` advisory exclusive lock for append, released on drop.
//!   - File perms `0600`, directory `0700` (Unix).
//!   - Resume: file size ≤ 100 MiB, line count ≤ 200_000 as DoS cap.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use harness_proto::{Message, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SESSION_FORMAT_VERSION: u32 = 1;
pub const MAX_SESSION_SIZE_BYTES: u64 = 100 * 1024 * 1024;
/// Deserializer depth cap — serde_json itself has no config for this, so we
/// treat it as a header-validation cap via manual inspection on load.
pub const MAX_SESSION_DEPTH: u8 = 64;
pub const MAX_SESSION_LINES: usize = 200_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub v: u32,
    #[serde(default)]
    pub schema: String,
    pub id: SessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub model: String,
}

impl SessionHeader {
    pub fn new(id: SessionId, model: impl Into<String>) -> Self {
        Self {
            v: SESSION_FORMAT_VERSION,
            schema: "harness.session".into(),
            id,
            created_at: chrono::Utc::now(),
            model: model.into(),
        }
    }
}

/// Line-record discriminator. Messages + structured meta events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Message(Message),
    Meta(Meta),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub event: String,
    #[serde(default)]
    pub detail: serde_json::Value,
}

/// Root state directory per PLAN §13 / §8.2 — `$XDG_STATE_HOME/harness/`.
#[must_use]
pub fn state_dir() -> PathBuf {
    use etcetera::BaseStrategy;
    etcetera::choose_base_strategy()
        .ok()
        .map_or_else(
            || PathBuf::from(".").join(".harness"),
            |s| s.data_dir().join("harness"),
        )
}

/// Sessions directory — `<state>/sessions/`.
#[must_use]
pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

/// Resolve a session file path by id.
#[must_use]
pub fn session_path(id: &SessionId) -> PathBuf {
    sessions_dir().join(format!("{id}.jsonl"))
}

/// Initialize sessions dir with mode 0700, write header if absent.
pub async fn init(path: &Path, header: &SessionHeader) -> Result<(), MemError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        set_mode(parent, 0o700)?;
    }
    if path.exists() {
        return Ok(());
    }
    let line = serde_json::to_string(header)?;
    let mut payload = line.into_bytes();
    payload.push(b'\n');

    write_atomic(path, &payload).await?;
    #[cfg(unix)]
    set_mode(path, 0o600)?;
    Ok(())
}

/// Append one record (message or meta) under an advisory exclusive lock.
pub async fn append(path: &Path, record: &Record) -> Result<(), MemError> {
    let line = serde_json::to_string(record)?;
    let mut payload = line.into_bytes();
    payload.push(b'\n');

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || append_locked(&path, &payload))
        .await
        .map_err(|e| MemError::Io(std::io::Error::other(e.to_string())))??;
    Ok(())
}

fn append_locked(path: &Path, payload: &[u8]) -> Result<(), MemError> {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;
    use std::io::Write;

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.lock_exclusive()?;
    let res: std::io::Result<()> = (|| {
        f.write_all(payload)?;
        f.flush()?;
        Ok(())
    })();
    // Always release the lock — unlock errors eaten in favour of op result.
    let _ = FileExt::unlock(&f);
    res?;
    Ok(())
}

async fn write_atomic(path: &Path, payload: &[u8]) -> Result<(), MemError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session"),
        std::process::id()
    ));
    tokio::fs::write(&tmp, payload).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Load every message record in order, rejecting corrupt / oversized files.
pub async fn load(path: &Path) -> Result<LoadedSession, MemError> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.len() > MAX_SESSION_SIZE_BYTES {
        return Err(MemError::TooLarge(meta.len()));
    }

    let bytes = tokio::fs::read(path).await?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| MemError::Io(std::io::Error::other(format!("utf8: {e}"))))?;

    let mut lines = text.split('\n').filter(|l| !l.is_empty());
    let header_line = lines.next().ok_or(MemError::Empty)?;
    let header: SessionHeader = serde_json::from_str(header_line)?;
    if header.v != SESSION_FORMAT_VERSION {
        return Err(MemError::VersionMismatch {
            expected: SESSION_FORMAT_VERSION,
            found: header.v,
        });
    }

    let mut messages = Vec::new();
    for (i, line) in lines.enumerate() {
        if i >= MAX_SESSION_LINES {
            return Err(MemError::TooManyLines(i));
        }
        if depth_exceeds(line, MAX_SESSION_DEPTH) {
            return Err(MemError::DepthCap);
        }
        let rec: Record = serde_json::from_str(line)?;
        if let Record::Message(m) = rec {
            messages.push(m);
        }
    }
    Ok(LoadedSession { header, messages })
}

/// Cheap pre-parse depth check — counts nested `{`/`[` runs on a line.
fn depth_exceeds(line: &str, cap: u8) -> bool {
    let cap = cap as usize;
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for b in line.bytes() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    max = depth;
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    max > cap
}

#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub header: SessionHeader,
    pub messages: Vec<Message>,
}

/// List known session ids (stem of `.jsonl` files).
pub async fn list_sessions() -> Result<Vec<SessionId>, MemError> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push(SessionId::new(stem));
        }
    }
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(mode);
    std::fs::set_permissions(path, perm)
}

/// Standard event name for the turn-cancelled sidecar `Meta` record. PLAN §3.2
/// (TaskStop). The marker is appended **after** the partial assistant `Message`
/// has been written, so a session reader can pair them by line adjacency: any
/// cancelled-message is the line immediately preceding a `cancelled` Meta record.
pub const META_EVENT_CANCELLED: &str = "cancelled";

/// User-facing reason strings written into the cancelled Meta detail. Mirrors
/// `harness_core::CancelReason`. We keep the mapping here so `harness-mem` does
/// not depend on `harness-core`.
pub const CANCEL_REASON_USER_INTERRUPT: &str = "user_interrupt";
pub const CANCEL_REASON_TIMEOUT: &str = "timeout";
pub const CANCEL_REASON_INTERNAL: &str = "internal";

/// Append the partial assistant message + a `cancelled` sidecar Meta record.
/// PLAN §3.2 — a sidecar is used instead of mutating `Message` so the
/// `harness-proto` wire schema does not need to bump. Resume readers detect a
/// cancellation by matching a `cancelled` Meta line to its preceding Message.
pub async fn append_cancelled_turn(
    path: &Path,
    partial_assistant: Option<&Message>,
    reason: &str,
) -> Result<(), MemError> {
    if let Some(msg) = partial_assistant {
        append(path, &Record::Message(msg.clone())).await?;
    }
    append(
        path,
        &Record::Meta(Meta {
            event: META_EVENT_CANCELLED.into(),
            detail: serde_json::json!({
                "reason": reason,
                "ts": chrono::Utc::now().to_rfc3339(),
            }),
        }),
    )
    .await?;
    Ok(())
}

/// Mint a fresh session id — ISO8601 + 8 hex chars of process-local entropy.
#[must_use]
pub fn new_session_id() -> SessionId {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let rand = fastrand_u32();
    SessionId::new(format!("{ts}-{rand:08x}"))
}

/// PRNG derived from std, avoids bringing in `rand`.
fn fastrand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    h.finish() as u32
}

#[derive(Debug, Error)]
pub enum MemError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("version mismatch: expected {expected}, got {found}")]
    VersionMismatch { expected: u32, found: u32 },
    #[error("size cap exceeded: {0} bytes (max {MAX_SESSION_SIZE_BYTES})")]
    TooLarge(u64),
    #[error("too many lines: {0} (max {MAX_SESSION_LINES})")]
    TooManyLines(usize),
    #[error("nested structure exceeds depth cap {MAX_SESSION_DEPTH}")]
    DepthCap,
    #[error("empty session file")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::Role;
    use tempfile::tempdir;

    #[tokio::test]
    async fn roundtrip_append_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let id = SessionId::new("s-001");
        let header = SessionHeader::new(id.clone(), "claude-opus-4-7");
        init(&path, &header).await.unwrap();

        let msg = Message::user("hello");
        append(&path, &Record::Message(msg.clone())).await.unwrap();
        append(
            &path,
            &Record::Meta(Meta {
                event: "hook_fire".into(),
                detail: serde_json::json!({"name":"PreToolUse"}),
            }),
        )
        .await
        .unwrap();

        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.header.id, id);
        assert_eq!(loaded.messages.len(), 1);
        assert!(matches!(loaded.messages[0].role, Role::User));
    }

    #[tokio::test]
    async fn version_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        tokio::fs::write(
            &path,
            b"{\"v\":9,\"schema\":\"harness.session\",\"id\":\"x\",\"created_at\":\"2026-04-20T00:00:00Z\",\"model\":\"m\"}\n",
        )
        .await
        .unwrap();
        let err = load(&path).await.unwrap_err();
        assert!(matches!(err, MemError::VersionMismatch { found: 9, .. }));
    }

    #[test]
    fn depth_cap_catches_nesting() {
        let shallow = r#"{"a":{"b":{"c":1}}}"#;
        assert!(!depth_exceeds(shallow, 64));
        let deep = "{".repeat(100) + &"}".repeat(100);
        assert!(depth_exceeds(&deep, 64));
    }

    #[tokio::test]
    async fn cancelled_marker_appended_after_partial_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let id = SessionId::new("s-cancel");
        let header = SessionHeader::new(id, "claude-opus-4-7");
        init(&path, &header).await.unwrap();

        let partial = Message {
            role: harness_proto::Role::Assistant,
            content: vec![harness_proto::ContentBlock::Text {
                text: "Partial reply".into(),
                cache_control: None,
            }],
            usage: None,
        };
        append_cancelled_turn(&path, Some(&partial), CANCEL_REASON_USER_INTERRUPT)
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "expected header + message + cancelled meta");
        assert!(lines[1].contains("\"role\":\"assistant\""));
        assert!(lines[1].contains("Partial reply"));
        assert!(lines[2].contains("\"event\":\"cancelled\""));
        assert!(lines[2].contains("user_interrupt"));

        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[tokio::test]
    async fn cancelled_marker_without_partial_only_writes_meta() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let id = SessionId::new("s-cancel-empty");
        let header = SessionHeader::new(id, "claude-opus-4-7");
        init(&path, &header).await.unwrap();

        append_cancelled_turn(&path, None, CANCEL_REASON_USER_INTERRUPT)
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("\"event\":\"cancelled\""));
    }
}
