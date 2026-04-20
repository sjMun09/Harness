//! Process helpers — setsid + killpg wiring for Bash cancel. PLAN §2.2 / §8.2.
//!
//! On cancel, the turn loop sends `killpg(pgid, SIGTERM)`; if the process
//! group is still alive `GRACEFUL_SHUTDOWN` later, escalate to SIGKILL.

use std::time::Duration;

pub const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);

/// Send SIGTERM to a process group. `pgid` is a positive pgid (not negated).
#[cfg(unix)]
pub fn killpg_term(pgid: i32) -> std::io::Result<()> {
    send_signal(pgid, nix::sys::signal::Signal::SIGTERM)
}

#[cfg(unix)]
pub fn killpg_kill(pgid: i32) -> std::io::Result<()> {
    send_signal(pgid, nix::sys::signal::Signal::SIGKILL)
}

#[cfg(unix)]
fn send_signal(pgid: i32, sig: nix::sys::signal::Signal) -> std::io::Result<()> {
    use nix::errno::Errno;
    use nix::unistd::Pid;
    match nix::sys::signal::killpg(Pid::from_raw(pgid), sig) {
        Ok(()) => Ok(()),
        // ESRCH = group already gone; treat as success (idempotent).
        Err(Errno::ESRCH) => Ok(()),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

#[cfg(not(unix))]
pub fn killpg_term(_pgid: i32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub fn killpg_kill(_pgid: i32) -> std::io::Result<()> {
    Ok(())
}

/// Wait up to `GRACEFUL_SHUTDOWN` for the pgid to exit, then escalate to
/// SIGKILL. Pure helper — caller owns the `Child` and is responsible for
/// eventual `.wait().await`.
pub async fn graceful_kill_pgid(pgid: i32) {
    let _ = killpg_term(pgid);
    tokio::time::sleep(GRACEFUL_SHUTDOWN).await;
    let _ = killpg_kill(pgid);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn killpg_on_nonexistent_group_is_idempotent() {
        // Pick a highly unlikely PGID — kernel returns ESRCH, our wrapper
        // maps that to Ok(()).
        assert!(killpg_term(1_999_999).is_ok());
    }
}
