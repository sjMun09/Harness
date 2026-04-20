//! Session JSONL + kv meta storage. PLAN §3.1 / §5.11 / §8.2.
//!
//! Layout (skeleton): `$XDG_STATE_HOME/harness/sessions/<session_id>.jsonl` with
//! a `{"v":1}` header line; `fs4` advisory lock; file perms `0600`, directory
//! `0700`. Resume size cap 100 MiB, deserializer depth cap 64.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use harness_proto::{Message, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// JSONL version header line. PLAN §5.11.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Hard cap on resume size. §8.2.
pub const MAX_SESSION_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// Deserializer depth cap. §8.2.
pub const MAX_SESSION_DEPTH: u8 = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub v: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub model: String,
}

/// Append a single message to the session JSONL. Iter 1 wiring.
pub async fn append(_path: &Path, _msg: &Message) -> Result<(), MemError> {
    Err(MemError::NotImplemented("append"))
}

/// Load a session transcript with size + depth caps.
pub async fn load(_path: &Path) -> Result<Vec<Message>, MemError> {
    Err(MemError::NotImplemented("load"))
}

/// Resolve the sessions directory per PLAN §13.1.
pub fn sessions_dir() -> PathBuf {
    // Iter 1 body: etcetera::BaseStrategy::state_dir() + /harness/sessions/.
    PathBuf::from("./.harness-sessions")
}

#[derive(Debug, Error)]
pub enum MemError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("version mismatch: expected {expected}, got {found}")]
    VersionMismatch { expected: u32, found: u32 },
    #[error("size cap exceeded: {0} > {MAX_SESSION_SIZE_BYTES}")]
    TooLarge(u64),
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
}
