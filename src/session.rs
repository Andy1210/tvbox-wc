//! The session the compositor exists for.
//!
//! greetd starts one command per session, so the compositor starts the session
//! itself: audio, the shell's respawn loop, whatever else the box needs. It runs as
//! a child, which is what makes the lifetimes right in both directions - it cannot
//! start before the Wayland socket is listening, and it cannot outlive the display
//! it is drawing on.

use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use smithay::reexports::calloop::{channel, LoopHandle};
use tracing::{info, warn};

use crate::state::Tvbox;

/// A running session.
pub struct Session {
    /// Also the process group id: the child leads its own group.
    pid: i32,
}

/// How long the session's processes get to exit on SIGTERM before they are killed.
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);

impl Session {
    /// End the session and everything it started.
    ///
    /// The whole group, because the session is a shell script whose real work is
    /// its children - signalling only the script would leave the shell running on a
    /// display that is about to disappear. The group is signalled even after the
    /// script itself has exited and been reaped: the group outlives its leader, and
    /// the kernel does not hand its id to anyone else while a member is alive, so
    /// the signal can only reach what the session started.
    ///
    /// SIGTERM first, then SIGKILL for whatever is still there after [`GRACE`]. A
    /// player blocked in a network read, or a program that ignores SIGTERM, would
    /// otherwise outlive the display and meet the next session's copy of itself.
    pub fn stop(&self) {
        stop_group(self.pid, GRACE);
    }
}

/// Signal a process group to end, and kill it when it does not.
fn stop_group(pgid: i32, grace: std::time::Duration) {
    // Safety: kill(2) with a negative pid signals the process group; ESRCH (nothing
    // left in it) is the answer when the session has already gone.
    if unsafe { libc::kill(-pgid, libc::SIGTERM) } != 0 {
        return;
    }
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        // Signal 0 only asks whether any member is left.
        if unsafe { libc::kill(-pgid, 0) } != 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    warn!(pgid, "the session did not exit on SIGTERM - killing it");
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// The session must not outlive the compositor, whatever ends it.
///
/// It is deliberately in its own process group - so a signal aimed at the
/// compositor does not travel to it - which also means the group kill greetd sends
/// at the end of a session does not reach it either. Without this, an error path or
/// a panic leaves the shell, mpv and the audio running against a Wayland socket
/// that no longer exists, and the next compositor comes up with a second session on
/// top of the first.
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the session, and stop the compositor when it exits.
///
/// The child gets its own process group so a signal meant for it does not travel
/// back to the compositor, and so the compositor can end the whole tree at once.
pub fn spawn(
    loop_handle: &LoopHandle<'static, Tvbox>,
    command: Vec<String>,
    environment: &[(&str, &std::ffi::OsStr)],
    running: Arc<AtomicBool>,
) -> Result<Session> {
    let (program, arguments) = command
        .split_first()
        .expect("parse rejects an empty command");
    let mut child = Command::new(program)
        .args(arguments)
        .envs(environment.iter().copied())
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to start the session: {program}"))?;
    let pid = child.id() as i32;
    info!(pid, command = ?command, "session started");

    // The wait happens on a thread because the event loop must keep dispatching -
    // the session's first act is to connect to it. The channel is how the answer
    // gets back into the loop; a bare atomic would not wake it up.
    let (sender, receiver) = channel::channel::<i32>();
    std::thread::Builder::new()
        .name("session-wait".to_owned())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            let _ = sender.send(code);
        })
        .context("failed to start the session watcher")?;

    loop_handle
        .insert_source(receiver, move |event, _, _state| {
            if let channel::Event::Msg(code) = event {
                warn!(code, "the session exited - stopping");
                running.store(false, Ordering::SeqCst);
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to watch the session: {err}"))?;

    Ok(Session { pid })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_child_is_stopped_after_its_leader_has_exited() {
        // The leader exits at once and leaves a child behind in its group, which is
        // the shape of a session script that crashed.
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null & echo $!"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pgid = leader.id() as i32;
        let mut out = String::new();
        std::io::Read::read_to_string(leader.stdout.as_mut().unwrap(), &mut out).unwrap();
        leader.wait().unwrap();
        let orphan: i32 = out.trim().parse().unwrap();
        assert!(alive(orphan));

        stop_group(pgid, std::time::Duration::from_secs(2));
        // The orphan is not our child, so nothing reaps it here; its init does.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while alive(orphan) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!alive(orphan));
    }

    #[test]
    fn a_process_that_ignores_sigterm_is_killed() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let started = std::time::Instant::now();
        stop_group(child.id() as i32, std::time::Duration::from_millis(300));
        let status = child.wait().unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(300));
        assert!(!status.success());
    }
}
