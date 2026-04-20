//! Process helpers — setsid + killpg wiring for Bash cancel. PLAN §2.2 / §8.2.
//!
//! Stub for MVP skeleton; Linux/BSD impl lands in iter 1 via `nix`.

use std::time::Duration;

/// Graceful shutdown timeout before SIGKILL. PLAN §2.2.
pub const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);

/// Kill a whole process group. Unix-only in practice.
pub fn killpg_term(_pgid: i32) -> std::io::Result<()> {
    // Iter 1 body: nix::sys::signal::killpg(Pid::from_raw(-pgid), SIGTERM).
    Ok(())
}

pub fn killpg_kill(_pgid: i32) -> std::io::Result<()> {
    // Iter 1 body: nix::sys::signal::killpg(Pid::from_raw(-pgid), SIGKILL).
    Ok(())
}
