//! Rolling per-session log file.
//!
//! Path: `$XDG_STATE_HOME/harness/logs/<id>.log`, where `<id>` is a
//! timestamp plus PID so we don't clash across parallel invocations. The
//! file is opened append + 0600 (Unix) and wrapped in a `Mutex<File>` so
//! the tracing layer can write from multiple threads.
//!
//! Deliberately does **not** depend on `tracing_appender`: the workspace
//! doesn't pull it in and we don't need rotation for the CLI use case (each
//! invocation gets its own file). Keeping the dep surface small matters more
//! than rolling by size.
//!
//! Used by `main::init_tracing` — the stderr writer is tee'd with this file
//! writer so `--quiet` silences stderr while the file still gets events.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Session-log id: `<UTC-timestamp>-<pid>`. Stable across a single process
/// invocation; regenerated on every `main` entry. Not the same as the
/// storage-layer `SessionId` — logs can outlive sessions (e.g. `doctor` has
/// no session) and resumed sessions still get a fresh log file.
pub fn log_id() -> String {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let pid = std::process::id();
    format!("{ts}-{pid}")
}

/// Resolve the log file path for a given `id`. Parent dir is
/// `<state_dir>/logs/`; callers must ensure it exists before opening.
pub fn log_path(id: &str) -> PathBuf {
    harness_mem::state_dir()
        .join("logs")
        .join(format!("{id}.log"))
}

/// Open (create + append) a log file at `log_path(id)`. Creates parent dirs
/// as needed; sets the file mode to 0600 on Unix. Returns a shared handle
/// plus the resolved path so callers can echo it to the user.
pub fn open(id: &str) -> io::Result<(Arc<Mutex<File>>, PathBuf)> {
    let path = log_path(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        set_mode(parent, 0o700)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    set_mode(&path, 0o600)?;
    Ok((Arc::new(Mutex::new(file)), path))
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(mode);
    std::fs::set_permissions(path, perm)
}

/// `Write` adapter over an `Arc<Mutex<File>>`. Writes acquire the lock, so
/// concurrent tracing events from different threads serialize cleanly.
/// Poisoned locks are treated as EOF — tracing should keep running.
pub struct SharedFileWriter {
    inner: Arc<Mutex<File>>,
}

impl SharedFileWriter {
    pub fn new(inner: Arc<Mutex<File>>) -> Self {
        Self { inner }
    }
}

impl Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.lock() {
            Ok(mut g) => g.write(buf),
            Err(_) => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.inner.lock() {
            Ok(mut g) => g.flush(),
            Err(_) => Ok(()),
        }
    }
}

/// `MakeWriter` factory that clones the shared handle per event.
#[derive(Clone)]
pub struct SharedFileMakeWriter {
    inner: Arc<Mutex<File>>,
}

impl SharedFileMakeWriter {
    pub fn new(inner: Arc<Mutex<File>>) -> Self {
        Self { inner }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedFileMakeWriter {
    type Writer = SharedFileWriter;
    fn make_writer(&'a self) -> Self::Writer {
        SharedFileWriter::new(self.inner.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_id_shape() {
        let id = log_id();
        // `<ts>-<pid>` — at least one hyphen, non-empty halves.
        let (ts, pid) = id.rsplit_once('-').expect("hyphen-separated");
        assert!(!ts.is_empty());
        assert!(!pid.is_empty());
        assert!(pid.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn open_then_write_creates_file() {
        use std::io::Read as _;
        let id = format!("test-{}", log_id());
        // We can't easily swap `state_dir` — fall back to writing to a tempdir
        // by pointing XDG_DATA_HOME (state_dir uses etcetera's data_dir on
        // non-XDG platforms; see harness_mem::state_dir). This test is best
        // effort: it verifies the writer end-to-end, not the exact path.
        let (handle, path) = open(&id).expect("open log");
        {
            let mut w = SharedFileWriter::new(handle);
            w.write_all(b"hello doctor\n").unwrap();
            w.flush().unwrap();
        }
        let mut buf = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert!(buf.contains("hello doctor"));
        // Cleanup — best effort.
        let _ = std::fs::remove_file(&path);
    }
}
