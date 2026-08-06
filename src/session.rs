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
    /// Set once the child has been reaped. After that the pid belongs to the
    /// kernel again and may already name someone else's process group, so the
    /// signal below must not go out.
    reaped: Arc<AtomicBool>,
}

impl Session {
    /// Ask the session and everything it started to end.
    ///
    /// The whole group, because the session is a shell script whose real work is
    /// its children - signalling only the script would leave the shell running on a
    /// display that is about to disappear.
    pub fn stop(&self) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        // Safety: kill(2) with a negative pid signals the process group.
        unsafe { libc::kill(-self.pid, libc::SIGTERM) };
    }
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
    running: Arc<AtomicBool>,
) -> Result<Session> {
    let (program, arguments) = command
        .split_first()
        .expect("parse rejects an empty command");
    let mut child = Command::new(program)
        .args(arguments)
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to start the session: {program}"))?;
    let pid = child.id() as i32;
    info!(pid, command = ?command, "session started");

    // The wait happens on a thread because the event loop must keep dispatching -
    // the session's first act is to connect to it. The channel is how the answer
    // gets back into the loop; a bare atomic would not wake it up.
    let (sender, receiver) = channel::channel::<i32>();
    let reaped = Arc::new(AtomicBool::new(false));
    let watcher_reaped = reaped.clone();
    std::thread::Builder::new()
        .name("session-wait".to_owned())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            watcher_reaped.store(true, Ordering::SeqCst);
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

    Ok(Session { pid, reaped })
}
