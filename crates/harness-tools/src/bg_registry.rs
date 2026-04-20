//! Background-job registry for `Bash(run_in_background=true)` (PLAN §3.2).
//!
//! Lifecycle:
//!   1. Bash spawns the child; hands it to `BgRegistry::insert(...)` which
//!      returns a short `shell_id`.
//!   2. A spawned tokio task drains the child's stdout / stderr into per-job
//!      head+tail-capped ring buffers and updates the job status when the
//!      child exits.
//!   3. `BashOutput { shell_id, filter? }` snapshots the new (since-last-poll)
//!      output via the consumer cursor.
//!   4. `KillShell { shell_id }` trips the cancel token; the drainer task
//!      then issues SIGTERM → SIGKILL via the existing `proc::*` helpers.
//!
//! The registry is **process-global** (`OnceLock`): background shells must
//! outlive a single tool call, and Harness is a single-session CLI process.

// Mutex-poison `expect("...mutex poisoned")` is idiomatic and the only
// recovery path is process abort; the workspace lints flag it as a
// would-be-bug. Tests use unwrap() per workspace convention.
#![allow(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

/// Per-stream cap (bytes). PLAN §3.2 / §4.1 — head 4KB + tail 4KB.
pub const HEAD_CAP: usize = 4 * 1024;
pub const TAIL_CAP: usize = 4 * 1024;
/// Hard ceiling for any single drain response, defensive.
pub const DRAIN_HARD_CAP: usize = HEAD_CAP + TAIL_CAP;

/// Status of a background job. Distinct from process exit status because we
/// also model "killed by user via KillShell".
#[derive(Debug, Clone)]
pub enum JobStatus {
    Running { pid: u32 },
    Exited { code: Option<i32>, at: SystemTime },
    Killed { at: SystemTime },
}

impl JobStatus {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running { .. })
    }
}

/// Public-facing handle metadata.
#[derive(Debug, Clone)]
pub struct JobHandle {
    pub shell_id: String,
    pub cmd: String,
    pub started_at: SystemTime,
    pub pid: u32,
}

/// Cheap snapshot returned to callers (no internal locks held).
#[derive(Debug, Clone)]
pub struct JobView {
    pub handle: JobHandle,
    pub status: JobStatus,
}

/// Bytes drained from a job since the consumer cursor was last advanced.
#[derive(Debug, Clone, Default)]
pub struct DrainedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated_bytes: u64,
    pub stderr_truncated_bytes: u64,
}

/// Append-only-ish ring buffer: keeps at most `HEAD_CAP` bytes from the
/// beginning of the stream and `TAIL_CAP` bytes from the live tail. Bytes
/// in between are accounted for in `truncated_middle` but not retained.
#[derive(Debug)]
pub(crate) struct RingBuffer {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    /// Total bytes ever written.
    written: u64,
    /// Bytes dropped between head and tail.
    truncated_middle: u64,
    /// Consumer cursor (what BashOutput has already returned).
    consumed: u64,
}

impl RingBuffer {
    pub(crate) fn new() -> Self {
        Self {
            head: Vec::with_capacity(HEAD_CAP),
            tail: std::collections::VecDeque::with_capacity(TAIL_CAP),
            written: 0,
            truncated_middle: 0,
            consumed: 0,
        }
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.written = self.written.saturating_add(bytes.len() as u64);

        // Fill head first (immutable once full).
        let head_room = HEAD_CAP.saturating_sub(self.head.len());
        let (to_head, rest) = if head_room == 0 {
            (&[][..], bytes)
        } else {
            let n = head_room.min(bytes.len());
            (&bytes[..n], &bytes[n..])
        };
        if !to_head.is_empty() {
            self.head.extend_from_slice(to_head);
        }

        // Push the rest into tail; if tail overflows, count overflow into
        // truncated_middle and pop from the front.
        for &b in rest {
            if self.tail.len() == TAIL_CAP {
                // pop_front never panics here.
                let _ = self.tail.pop_front();
                self.truncated_middle = self.truncated_middle.saturating_add(1);
            }
            self.tail.push_back(b);
        }
    }

    /// Bytes currently retained (head + tail).
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.head.len() + self.tail.len() + 64);
        out.extend_from_slice(&self.head);
        if self.truncated_middle > 0 {
            out.extend_from_slice(
                format!("\n... [{} bytes truncated] ...\n", self.truncated_middle).as_bytes(),
            );
        }
        out.extend(self.tail.iter().copied());
        out
    }

    /// Drain new bytes since the last `drain` call. Marks the consumer
    /// cursor at `written`. If the consumer fell behind so far that some
    /// new bytes were already evicted into `truncated_middle`, prepend a
    /// truncation marker accounting for the lost middle.
    pub(crate) fn drain(&mut self) -> (Vec<u8>, u64) {
        if self.consumed >= self.written {
            self.consumed = self.written;
            return (Vec::new(), 0);
        }

        let total_snapshot = self.snapshot();
        // The snapshot covers bytes [0, head.len()) ∪ [written - tail.len(), written).
        // For a consumer that has seen `consumed` bytes, the new bytes are
        // those in the snapshot that lie at positions ≥ consumed. To keep
        // this simple and correct for both small and overflowing buffers,
        // we just return the trailing portion that wasn't previously
        // consumed, computed from `written` boundaries.
        let new_count = self.written - self.consumed;
        let lost = if new_count as usize > total_snapshot.len() {
            new_count as usize - total_snapshot.len()
        } else {
            0
        };
        let take = (new_count as usize).min(total_snapshot.len());
        let start = total_snapshot.len() - take;
        let mut out = Vec::with_capacity(take + 64);
        if lost > 0 {
            out.extend_from_slice(
                format!("... [consumer lagged; {lost} bytes truncated] ...\n").as_bytes(),
            );
        }
        out.extend_from_slice(&total_snapshot[start..]);
        self.consumed = self.written;
        (out, lost as u64)
    }

    #[cfg(test)]
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}

#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) handle: JobHandle,
    pub(crate) status: Arc<Mutex<JobStatus>>,
    pub(crate) stdout: Arc<Mutex<RingBuffer>>,
    pub(crate) stderr: Arc<Mutex<RingBuffer>>,
    pub(crate) cancel: CancellationToken,
}

#[derive(Debug, Default)]
pub struct BgRegistry {
    inner: Mutex<HashMap<String, Arc<Job>>>,
}

static GLOBAL: OnceLock<BgRegistry> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("unknown shell_id: {0}")]
    Unknown(String),
}

#[derive(Debug, Error)]
pub enum KillError {
    #[error("unknown shell_id: {0}")]
    Unknown(String),
}

/// Generate a short, monotonic-ish shell id (e.g. `bash_a1b2c3`).
pub fn fresh_shell_id() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    // Mix sequence + nanos for collision-resistance across process restarts
    // within the same run. Rendered as 12-hex-digit suffix.
    let mixed = u128::from(n).wrapping_add(nanos);
    format!("bash_{:012x}", (mixed as u64) ^ ((mixed >> 64) as u64))
}

impl BgRegistry {
    #[must_use]
    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(BgRegistry::default)
    }

    /// Register a freshly spawned background job. Spawns the drainer task
    /// internally. Returns the shell_id assigned to the job.
    pub fn register(
        &self,
        cmd_text: String,
        mut child: Child,
        cancel: CancellationToken,
    ) -> String {
        let shell_id = fresh_shell_id();
        let pid = child.id().unwrap_or(0);
        let started_at = SystemTime::now();
        let handle = JobHandle {
            shell_id: shell_id.clone(),
            cmd: cmd_text,
            started_at,
            pid,
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let job = Arc::new(Job {
            handle: handle.clone(),
            status: Arc::new(Mutex::new(JobStatus::Running { pid })),
            stdout: Arc::new(Mutex::new(RingBuffer::new())),
            stderr: Arc::new(Mutex::new(RingBuffer::new())),
            cancel: cancel.clone(),
        });

        // Insert before spawning the drainer so observers can find it
        // immediately.
        {
            let mut g = self.inner.lock().expect("bg registry mutex poisoned");
            g.insert(shell_id.clone(), job.clone());
        }

        // Drainer task — owns the Child + pipes for the job's lifetime.
        let job_for_task = job.clone();
        tokio::spawn(async move {
            let cancel = job_for_task.cancel.clone();
            let stdout_buf = job_for_task.stdout.clone();
            let stderr_buf = job_for_task.stderr.clone();

            let stdout_task = stdout.map(|s| {
                let buf = stdout_buf.clone();
                tokio::spawn(drain_into(s, buf))
            });
            let stderr_task = stderr.map(|s| {
                let buf = stderr_buf.clone();
                tokio::spawn(drain_into(s, buf))
            });

            // Wait for the child to exit OR the cancel token to fire.
            let exit = tokio::select! {
                res = child.wait() => Some(res),
                () = cancel.cancelled() => None,
            };

            match exit {
                Some(Ok(status)) => {
                    let mut g = job_for_task
                        .status
                        .lock()
                        .expect("job status mutex poisoned");
                    *g = JobStatus::Exited {
                        code: status.code(),
                        at: SystemTime::now(),
                    };
                }
                Some(Err(_)) => {
                    let mut g = job_for_task
                        .status
                        .lock()
                        .expect("job status mutex poisoned");
                    *g = JobStatus::Exited {
                        code: None,
                        at: SystemTime::now(),
                    };
                }
                None => {
                    // Cancellation path — graceful kill via pgid (== pid for
                    // a setsid'd child). Falls back to child.kill() if the
                    // pgid path fails (e.g. non-unix or platform stubs).
                    if pid != 0 {
                        // pid_t is i32 on every platform we target.
                        #[allow(clippy::cast_possible_wrap)]
                        let pid_i = pid as i32;
                        crate::proc::graceful_kill_pgid(pid_i).await;
                    }
                    // Make sure the child is reaped even if killpg missed it.
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let mut g = job_for_task
                        .status
                        .lock()
                        .expect("job status mutex poisoned");
                    *g = JobStatus::Killed {
                        at: SystemTime::now(),
                    };
                }
            }

            // Drain remaining stdout/stderr bytes after the process exits.
            if let Some(t) = stdout_task {
                let _ = tokio::time::timeout(Duration::from_secs(1), t).await;
            }
            if let Some(t) = stderr_task {
                let _ = tokio::time::timeout(Duration::from_secs(1), t).await;
            }
        });

        shell_id
    }

    pub fn get(&self, id: &str) -> Option<JobView> {
        let g = self.inner.lock().expect("bg registry mutex poisoned");
        let job = g.get(id)?;
        let status = job
            .status
            .lock()
            .expect("job status mutex poisoned")
            .clone();
        Some(JobView {
            handle: job.handle.clone(),
            status,
        })
    }

    pub fn list(&self) -> Vec<JobView> {
        let g = self.inner.lock().expect("bg registry mutex poisoned");
        g.values()
            .map(|job| JobView {
                handle: job.handle.clone(),
                status: job
                    .status
                    .lock()
                    .expect("job status mutex poisoned")
                    .clone(),
            })
            .collect()
    }

    /// Drain new stdout/stderr bytes since the consumer's last poll, plus
    /// the current status. `Err(Unknown)` if the id does not exist.
    pub fn drain_new_output(&self, id: &str) -> Result<(DrainedOutput, JobStatus), RegistryError> {
        let job = {
            let g = self.inner.lock().expect("bg registry mutex poisoned");
            g.get(id).cloned()
        };
        let job = job.ok_or_else(|| RegistryError::Unknown(id.to_string()))?;

        let (out_bytes, out_lost) = job
            .stdout
            .lock()
            .expect("job stdout mutex poisoned")
            .drain();
        let (err_bytes, err_lost) = job
            .stderr
            .lock()
            .expect("job stderr mutex poisoned")
            .drain();
        let status = job
            .status
            .lock()
            .expect("job status mutex poisoned")
            .clone();

        Ok((
            DrainedOutput {
                stdout: out_bytes,
                stderr: err_bytes,
                stdout_truncated_bytes: out_lost,
                stderr_truncated_bytes: err_lost,
            },
            status,
        ))
    }

    /// Trip the cancel token for the given job. The drainer task observes
    /// it and runs the SIGTERM → SIGKILL escalation. Idempotent.
    pub fn kill(&self, id: &str) -> Result<(), KillError> {
        let job = {
            let g = self.inner.lock().expect("bg registry mutex poisoned");
            g.get(id).cloned()
        };
        let job = job.ok_or_else(|| KillError::Unknown(id.to_string()))?;
        job.cancel.cancel();
        Ok(())
    }

    /// Drop the entry. Status remains observable via prior `JobView` clones,
    /// but `get` / `drain_new_output` will return Unknown afterwards. Mainly
    /// used by tests to keep the registry tidy.
    pub fn purge(&self, id: &str) -> bool {
        let mut g = self.inner.lock().expect("bg registry mutex poisoned");
        g.remove(id).is_some()
    }
}

async fn drain_into<R: AsyncRead + Unpin>(mut r: R, buf: Arc<Mutex<RingBuffer>>) {
    let mut tmp = [0u8; 4096];
    loop {
        match r.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut g) = buf.lock() {
                    g.append(&tmp[..n]);
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_under_cap_is_lossless() {
        let mut rb = RingBuffer::new();
        rb.append(b"hello world");
        let snap = rb.snapshot();
        assert_eq!(snap, b"hello world");
        let (drain, lost) = rb.drain();
        assert_eq!(drain, b"hello world");
        assert_eq!(lost, 0);
        // Second drain returns nothing.
        let (drain2, _) = rb.drain();
        assert!(drain2.is_empty());
    }

    #[test]
    fn ring_buffer_truncates_middle_above_cap() {
        let mut rb = RingBuffer::new();
        // Fill head fully + push past tail to force truncation.
        let big = vec![b'A'; HEAD_CAP];
        rb.append(&big);
        let mid = vec![b'B'; TAIL_CAP];
        rb.append(&mid); // fills tail
        let extra = vec![b'C'; 1024]; // forces truncation
        rb.append(&extra);
        let snap = rb.snapshot();
        // Must contain head A's, marker, and end with C's.
        assert!(snap.starts_with(&[b'A'; 16][..]));
        assert!(snap.ends_with(&[b'C'; 16][..]));
        let marker = String::from_utf8_lossy(&snap);
        assert!(marker.contains("bytes truncated"));
        assert_eq!(rb.written(), (HEAD_CAP + TAIL_CAP + 1024) as u64);
    }

    #[test]
    fn fresh_shell_id_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            assert!(seen.insert(fresh_shell_id()));
        }
    }
}
