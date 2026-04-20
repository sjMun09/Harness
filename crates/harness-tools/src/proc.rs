//! Process helpers — setsid + killpg wiring for Bash cancel. PLAN §2.2 / §8.2.
//!
//! On cancel, the turn loop sends `killpg(pgid, SIGTERM)`; if the process
//! group is still alive `GRACEFUL_SHUTDOWN` later, escalate to SIGKILL.
//!
//! Background spawns also opt into Linux `PR_SET_PDEATHSIG = SIGTERM`
//! (PLAN §3.2 / §8.2) so children die when Harness exits.

use std::time::Duration;

pub const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);

/// Configure a `tokio::process::Command` so the spawned child:
///   * starts a new session (`setsid`) → fresh pgid we can `killpg` later
///   * (Linux only) requests `PR_SET_PDEATHSIG = SIGTERM` so the child
///     dies if Harness exits before reaping it
///
/// Safety: `pre_exec` runs after `fork(2)` but before `execve(2)`, in the
/// child. Only async-signal-safe code may run there. `setsid(2)` and
/// `prctl(PR_SET_PDEATHSIG)` are both async-signal-safe. No allocation,
/// no Rust drops.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn configure_session_and_pdeathsig(cmd: &mut tokio::process::Command) {
    // SAFETY: Only async-signal-safe libc calls below. `setsid(2)` is on
    // the POSIX async-signal-safe list. `prctl(PR_SET_PDEATHSIG)` is
    // documented Linux-safe. We allocate / drop nothing inside the closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub fn configure_session_and_pdeathsig(_cmd: &mut tokio::process::Command) {}

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
