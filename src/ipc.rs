//! The shell's control channel.
//!
//! A unix socket in `$XDG_RUNTIME_DIR`, one JSON object per line, request and
//! response matched by `id`. That shape is not chosen for elegance: the shell
//! already speaks exactly this to mpv, so it needs no new client code and a person
//! can drive it from a terminal.
//!
//! ```text
//! -> {"id":1,"request":"get_outputs"}
//! <- {"id":1,"ok":{"outputs":[{"name":"HDMIA-1","current":{"w":1360,"h":768,"refresh":60000},...}]}}
//! -> {"id":2,"request":"set_mode","output":"HDMIA-1","w":1920,"h":1080}
//! <- {"id":2,"ok":null}
//! -> {"id":3,"request":"nonsense"}
//! <- {"id":3,"error":"unknown request"}
//! ```
//!
//! The socket is owned by the session user and lives in a directory only that user
//! can reach, which is the same protection mpv's control socket has, and a
//! connection from any other uid is refused. A sandboxed app (Flatpak) gets a
//! private runtime directory with only the Wayland socket in it, so it cannot reach
//! this one at all; an unsandboxed program running as the session user can, just as
//! it can reach every other socket that user owns.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::wayland::seat::WaylandFocus as _;
use tracing::{debug, warn};

use crate::state::{Focus, Tvbox};

/// A request from the shell.
#[derive(Debug, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Everything the shell needs to decide what to ask for.
    GetOutputs,
    /// Drive the output at a different mode.
    ///
    /// The refresh rate is optional: a TV usually offers one rate per size, and the
    /// shell should not have to know which of 59.94 and 60 the kernel reports.
    SetMode {
        /// Connector name, as reported by `get_outputs`.
        output: String,
        /// Width in pixels.
        w: i32,
        /// Height in pixels.
        h: i32,
        /// Refresh rate in mHz, if it matters.
        #[serde(default)]
        refresh: Option<i32>,
    },
    /// What the compositor is currently told and doing.
    GetState,
    /// Where windows go: a rectangle, or the whole output.
    ///
    /// Named by `app_id` (every window of that client) or by `title` (the one
    /// window carrying it). A title is what places a single window of a client with
    /// several - the shell's, whose windows all share one app id.
    PlaceWindow {
        /// The client's Wayland app id (`mpv` for the player).
        #[serde(default)]
        app_id: Option<String>,
        /// One window's title, when an app id would be too broad.
        #[serde(default)]
        title: Option<String>,
        /// Left edge, in output pixels. All four are needed for a rectangle;
        /// leaving them out puts the client back on the whole output.
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
        #[serde(default)]
        w: Option<i32>,
        #[serde(default)]
        h: Option<i32>,
    },
    /// Type a string into the focused field.
    TypeText {
        /// What to type.
        text: String,
        /// Replace what the field already holds instead of appending to it.
        #[serde(default)]
        select_all: bool,
    },
    /// Render the scene to a PNG, for looking at the screen from a terminal.
    Screenshot {
        /// Where to write it.
        path: String,
    },
    /// Tell the compositor who owns the screen.
    ///
    /// It cannot work this out for itself: the launcher and an app can be windows
    /// of the same process, and "an app is on screen" is the shell's own state.
    SetFocus {
        /// The launcher, or an app.
        owner: FocusOwner,
        /// Which app, when it is one.
        #[serde(default)]
        app: Option<String>,
    },
    /// Claim the output's colour space for HDR content, or give it back.
    ///
    /// A claim, not a setting: the colour space covers the whole output, so an SDR
    /// UI is read as PQ for as long as it is held.
    SetHdr {
        /// Connector name.
        output: String,
        /// Whether to claim it.
        on: bool,
    },
}

/// Who the shell says owns the screen.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusOwner {
    /// The launcher's own UI.
    Launcher,
    /// An app.
    App,
}

/// A mode, in the units the Wayland output protocol uses.
#[derive(Debug, Serialize)]
pub struct ModeInfo {
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
    /// Refresh rate in mHz.
    pub refresh: i32,
    /// Whether the display asked for this one.
    pub preferred: bool,
}

/// What an output can do and what it is doing.
#[derive(Debug, Serialize)]
pub struct OutputInfo {
    /// Connector name.
    pub name: String,
    /// The mode in use.
    pub current: Option<ModeInfo>,
    /// Every mode the connector advertises.
    pub modes: Vec<ModeInfo>,
    /// Whether a display is attached right now.
    ///
    /// A TV that has been switched off reports false, and nothing is drawn until it
    /// comes back.
    pub connected: bool,
    /// Colour space state.
    pub hdr: HdrInfo,
}

/// Whether HDR can be claimed on this output, and whether it is.
#[derive(Debug, Serialize)]
pub struct HdrInfo {
    /// The driver exposes the connector properties a claim needs.
    ///
    /// This says nothing about the panel. Whether the TV can show HDR is in its
    /// EDID, which the shell already reads.
    pub supported: bool,
    /// A claim is in effect.
    pub on: bool,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<u64>,
    #[serde(flatten)]
    request: Request,
}

/// The longest request line accepted. Requests are small - the largest carries a
/// string to type - and a peer that never sends a newline would otherwise grow this
/// process at socket speed until the box's OOM killer picks the biggest thing on it,
/// which is the compositor.
const MAX_LINE: usize = 64 * 1024;

/// How much one connection is read, and how many of its requests are answered, per
/// turn of the event loop. The source is level-triggered, so whatever is left is
/// picked up on the next turn - after the frames, the input and every other client
/// have had theirs. Without a bound a peer that writes as fast as it can would keep
/// the loop in this callback and freeze the picture and the remote.
const MAX_READ_PER_TURN: usize = 256 * 1024;
const MAX_REQUESTS_PER_TURN: usize = 64;

/// Start listening, and return the path so it can be handed to children.
pub fn listen(loop_handle: &LoopHandle<'static, Tvbox>, path: PathBuf) -> Result<PathBuf> {
    // A socket left behind by a compositor that did not shut down cleanly would
    // otherwise make every start after a crash fail.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind the control socket at {}", path.display()))?;
    // Owner only. This socket sets the output mode, types keys and takes
    // screenshots, and its usual home ($XDG_RUNTIME_DIR) is already 0700 - but the
    // fallback is not, and a socket inherits the umask rather than choosing. State
    // the permission instead of depending on where it landed.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to restrict the control socket at {}",
            path.display()
        )
    })?;
    listener
        .set_nonblocking(true)
        .context("failed to make the control socket non-blocking")?;

    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, _state| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => accept(stream),
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            warn!(?err, "failed to accept a control connection");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to insert the control socket source: {err}"))?;

    Ok(path)
}

fn accept(stream: UnixStream) {
    // The socket's mode already says this, but not when it landed in the /tmp
    // fallback before the permission was set, and a peer's uid costs one call.
    match peer_uid(&stream) {
        Some(uid) if uid == unsafe { libc::geteuid() } => {}
        uid => {
            warn!(?uid, "refused a control connection from another user");
            return;
        }
    }
    if let Err(err) = stream.set_nonblocking(true) {
        warn!(?err, "failed to configure a control connection");
        return;
    }
    debug!("control connection accepted");

    // Registering from inside the accept callback needs the loop handle, which the
    // state carries; do it from an idle callback so the borrow is clean.
    CONNECTIONS.with(|pending| pending.borrow_mut().push(stream));
}

/// The uid of the process at the other end of a unix socket.
fn peer_uid(stream: &UnixStream) -> Option<libc::uid_t> {
    use std::os::unix::io::AsRawFd as _;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Safety: SO_PEERCRED writes a ucred into a buffer of exactly that size.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (result == 0).then_some(cred.uid)
}

thread_local! {
    static CONNECTIONS: std::cell::RefCell<Vec<UnixStream>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Register any connection accepted since the last call.
///
/// Called from the event loop's per-iteration callback: accepting and registering
/// in one step would need the loop handle inside a source's own callback.
pub fn register_pending(state: &mut Tvbox) {
    let pending: Vec<UnixStream> =
        CONNECTIONS.with(|pending| pending.borrow_mut().drain(..).collect());
    for stream in pending {
        let mut buffer = Vec::new();
        let inserted = state.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state: &mut Tvbox| {
                let mut chunk = [0u8; 4096];
                let mut read = 0;
                let mut closed = false;
                // Only while there is room: the requests already in the buffer are
                // answered first, so a peer cannot fill it faster than it is drained.
                while read < MAX_READ_PER_TURN && buffer.len() <= MAX_LINE {
                    match (&**stream).read(&mut chunk) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        Ok(n) => {
                            read += n;
                            buffer.extend_from_slice(&chunk[..n]);
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(err) => {
                            warn!(?err, "control connection failed");
                            return Ok(PostAction::Remove);
                        }
                    }
                }

                let mut answered = 0;
                while answered < MAX_REQUESTS_PER_TURN {
                    let Some(end) = buffer.iter().position(|byte| *byte == b'\n') else {
                        break;
                    };
                    let line: Vec<u8> = buffer.drain(..=end).collect();
                    answered += 1;
                    let reply = handle_line(state, &line[..line.len() - 1]);
                    if let Err(err) = send(stream, &reply) {
                        warn!(?err, "failed to answer a control request");
                        return Ok(PostAction::Remove);
                    }
                }

                if unterminated_too_long(&buffer) {
                    warn!(
                        bytes = buffer.len(),
                        "a control connection sent an over-long line - dropping it"
                    );
                    return Ok(PostAction::Remove);
                }
                if closed && !buffer.contains(&b'\n') {
                    return Ok(PostAction::Remove);
                }
                Ok(PostAction::Continue)
            },
        );
        if let Err(err) = inserted {
            warn!(?err, "failed to register a control connection");
        }
    }
}

/// Whether the part of the buffer after its last complete line is already longer
/// than any request may be. Counted after the LAST newline: a peer that sends one
/// short line and then an endless one must not get past the cap on the strength of
/// the newline that is already in the buffer.
fn unterminated_too_long(buffer: &[u8]) -> bool {
    let start = buffer
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    buffer.len() - start > MAX_LINE
}

fn handle_line(state: &mut Tvbox, line: &[u8]) -> String {
    if line.iter().all(u8::is_ascii_whitespace) {
        return String::new();
    }

    let envelope: Envelope = match serde_json::from_slice(line) {
        Ok(envelope) => envelope,
        Err(err) => {
            return encode(&Response {
                id: None,
                ok: None,
                error: Some(&err.to_string()),
            });
        }
    };

    let id = envelope.id;
    match dispatch(state, envelope.request) {
        Ok(value) => encode(&Response {
            id,
            ok: Some(value),
            error: None,
        }),
        Err(err) => encode(&Response {
            id,
            ok: None,
            error: Some(&err.to_string()),
        }),
    }
}

fn dispatch(state: &mut Tvbox, request: Request) -> Result<serde_json::Value> {
    match request {
        Request::GetOutputs => {
            let outputs = state.tty.outputs();
            Ok(serde_json::json!({ "outputs": outputs }))
        }
        Request::SetMode {
            output,
            w,
            h,
            refresh,
        } => {
            state.set_mode(&output, w, h, refresh)?;
            Ok(serde_json::Value::Null)
        }
        Request::SetHdr { output, on } => {
            state.tty.set_hdr(&output, on)?;
            Ok(serde_json::Value::Null)
        }
        Request::GetState => {
            // Back to front, with the one that holds the keyboard marked. What is on
            // screen and what answers the remote are decided here and nowhere else,
            // so when a key goes somewhere unexpected this is the question to ask.
            let focused = state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus());
            let windows: Vec<serde_json::Value> = crate::stacking::stacked(&state.space)
                .iter()
                .map(|window| {
                    let surface = window.wl_surface().map(|s| s.into_owned());
                    serde_json::json!({
                        "app_id": crate::stacking::app_id(window),
                        // The title as well, because it is what marks the overlay -
                        // and a window whose title never arrived looks exactly like
                        // one that was never meant to be in front. Without it here,
                        // that is a guess.
                        "title": crate::stacking::window_title(window),
                        // Whether the window has drawn anything yet. A toplevel
                        // exists before its first buffer, and this list holds it
                        // from the moment it appears while the keyboard passes it
                        // over - so a window listed with no keyboard and no overlay
                        // in sight is an answer, and this is what says so.
                        "mapped": crate::stacking::mapped(window),
                        "keyboard": surface.is_some() && surface == focused,
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "focus": state.focus,
                // Something on screen is asking the box to stay awake - a game, a
                // player. The shell's ambient screen is the thing that should honour it.
                "idle_inhibited": state.idle_inhibited(),
                "windows": windows,
                // What is RUNNING, which is not what is installed: the compositor is
                // the session, so a newly installed binary is still only a file until
                // greetd restarts. A caller deciding whether a behaviour of ours can
                // be relied on has to ask the process, and the shell does exactly that
                // before it offers a field's contents to its keyboard - that offer is
                // only safe once `type_text` replaces rather than appends. Absent on
                // every build before this one, which is the right answer for them.
                "version": env!("CARGO_PKG_VERSION"),
            }))
        }
        Request::PlaceWindow {
            app_id,
            title,
            x,
            y,
            w,
            h,
        } => {
            let key = match (app_id, title) {
                // Every window of the shell shares its app id, the launcher included,
                // so a placement by it would move the whole UI - off screen, or into
                // a corner - and nothing the shell does needs that. Its windows are
                // placed one at a time, by title.
                (Some(app_id), None) if app_id == crate::stacking::shell_app_id() => {
                    anyhow::bail!("the shell's windows are placed by title, not by app id")
                }
                (Some(app_id), None) => crate::state::PlaceKey::AppId(app_id),
                (None, Some(title)) => crate::state::PlaceKey::Title(title),
                _ => anyhow::bail!(
                    "name the windows by app_id or by title, not both and not neither"
                ),
            };
            let rect = match (x, y, w, h) {
                (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => {
                    Some(smithay::utils::Rectangle::new((x, y).into(), (w, h).into()))
                }
                (None, None, None, None) => None,
                _ => anyhow::bail!("a rectangle needs x, y, w and h, and a positive size"),
            };
            state.set_placement(key, rect);
            Ok(serde_json::Value::Null)
        }
        Request::TypeText { text, select_all } => {
            let keys = state.type_text(&text, select_all)?;
            Ok(serde_json::json!({ "keys": keys }))
        }
        Request::Screenshot { path } => {
            let (w, h) = state.screenshot(std::path::Path::new(&path))?;
            Ok(serde_json::json!({ "path": path, "w": w, "h": h }))
        }
        Request::SetFocus { owner, app } => {
            state.focus = match owner {
                FocusOwner::Launcher => Focus::Launcher,
                FocusOwner::App => Focus::App(app.unwrap_or_default()),
            };
            debug!(focus = ?state.focus, "focus reported by the shell");
            Ok(serde_json::Value::Null)
        }
    }
}

fn encode(response: &Response<'_>) -> String {
    serde_json::to_string(response).unwrap_or_else(|err| {
        format!("{{\"id\":null,\"error\":\"failed to encode a response: {err}\"}}")
    })
}

fn send(mut stream: &UnixStream, reply: &str) -> std::io::Result<()> {
    if reply.is_empty() {
        return Ok(());
    }
    let mut payload = reply.as_bytes();
    let mut line = Vec::with_capacity(payload.len() + 1);
    line.extend_from_slice(payload);
    line.push(b'\n');
    payload = &line;

    // Replies are a few hundred bytes and the socket buffer is orders of magnitude
    // larger, so a partial write means the peer has stopped reading. Retrying a
    // bounded number of times keeps a stuck client from blocking the compositor.
    let mut written = 0;
    for _ in 0..16 {
        match stream.write(&payload[written..]) {
            Ok(0) => break,
            Ok(n) => {
                written += n;
                if written == payload.len() {
                    return Ok(());
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::yield_now();
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::WouldBlock,
        "the client stopped reading",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_line_does_not_let_an_endless_one_through() {
        let mut buffer = b"{}\n".to_vec();
        buffer.extend(std::iter::repeat_n(b'x', MAX_LINE));
        assert!(!unterminated_too_long(&buffer));
        buffer.push(b'x');
        assert!(unterminated_too_long(&buffer));
    }

    #[test]
    fn complete_lines_do_not_count_against_the_cap() {
        let mut buffer = Vec::new();
        for _ in 0..=(MAX_LINE / 4) {
            buffer.extend_from_slice(b"{}\n\n");
        }
        assert!(buffer.len() > MAX_LINE);
        assert!(!unterminated_too_long(&buffer));
    }

    #[test]
    fn the_peer_uid_is_ours_on_a_socketpair() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a), Some(unsafe { libc::geteuid() }));
    }
}
