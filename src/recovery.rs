//! Holding Home as the way back when the UI cannot help itself.
//!
//! Every ordinary route home is a key some client has to act on. When that client is
//! the part that is wrong - crashed, frozen, or showing its screen with nothing
//! focused - a TV remote has nothing else to press. The compositor sees every key
//! before any client does, so a key HELD past a threshold runs a command, and the
//! command does the repair from outside the session's UI.
//!
//! Stages go up with the hold: by default 3 s runs `<command> reload` and 10 s runs
//! `<command> restart`. The key is still delivered as usual, so a short press means
//! what it always meant; only its duration adds anything. A second key going down
//! during the hold cancels it, because a chord is somebody using the remote, not
//! somebody stuck.
//!
//! Configured from the environment:
//!
//! - `TVBOX_WC_RECOVERY_CMD`: the command. Default `$HOME/.tvbox/recover.sh`; empty
//!   switches the feature off. A command that does not exist when a stage is due is
//!   skipped, so the default costs nothing on a session that does not ship one.
//! - `TVBOX_WC_RECOVERY_KEYS`: comma-separated evdev key codes. Default `172`
//!   (KEY_HOMEPAGE).
//! - `TVBOX_WC_RECOVERY_HOLD_MS`: the stage thresholds, comma-separated and
//!   ascending. Default `3000,10000`. The stage names are `reload` and `restart`, in
//!   that order; a third threshold would have no name and is ignored.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use smithay::backend::input::KeyState;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use tracing::{info, warn};

use crate::state::Tvbox;

const DEFAULT_KEYS: &[u32] = &[172];
const DEFAULT_HOLD_MS: &[u64] = &[3000, 10000];
const STAGES: &[&str] = &["reload", "restart"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    command: PathBuf,
    keys: Vec<u32>,
    stages: Vec<(Duration, &'static str)>,
}

/// Parse the three settings. `None` means the feature is off.
fn config_from(
    cmd: Option<String>,
    keys: Option<String>,
    hold: Option<String>,
    home: Option<String>,
) -> Option<Config> {
    let command = match cmd {
        Some(c) if c.trim().is_empty() => return None,
        Some(c) => PathBuf::from(c),
        None => PathBuf::from(home?).join(".tvbox/recover.sh"),
    };
    let keys = keys
        .and_then(|k| {
            let parsed: Vec<u32> = k.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            (!parsed.is_empty()).then_some(parsed)
        })
        .unwrap_or_else(|| DEFAULT_KEYS.to_vec());
    let hold: Vec<u64> = hold
        .and_then(|h| {
            let parsed: Vec<u64> = h.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            let ascending = parsed.windows(2).all(|w| w[0] < w[1]);
            (!parsed.is_empty() && ascending && parsed[0] > 0).then_some(parsed)
        })
        .unwrap_or_else(|| DEFAULT_HOLD_MS.to_vec());
    let stages = hold
        .into_iter()
        .zip(STAGES.iter().copied())
        .map(|(ms, name)| (Duration::from_millis(ms), name))
        .collect();
    Some(Config {
        command,
        keys,
        stages,
    })
}

/// Which hold is in progress, if any. Every new hold (and every cancel) takes a new
/// generation, so a timer armed for an earlier one finds it is stale and does
/// nothing - cheaper and simpler than tracking timer tokens to remove them.
#[derive(Debug, Default)]
struct Hold {
    generation: u64,
    key: Option<u32>,
}

impl Hold {
    /// A key changed state. Returns the generation to arm timers for, when this is
    /// the start of a hold.
    fn key(&mut self, code: u32, pressed: bool, watched: &[u32]) -> Option<u64> {
        if pressed {
            if self.key.is_some() {
                // A second key while one is held: somebody is using the remote.
                self.cancel();
                return None;
            }
            if watched.contains(&code) {
                self.generation += 1;
                self.key = Some(code);
                return Some(self.generation);
            }
            return None;
        }
        if self.key == Some(code) {
            self.cancel();
        }
        None
    }

    fn cancel(&mut self) {
        self.generation += 1;
        self.key = None;
    }

    /// Is the hold armed as `generation` still going?
    fn current(&self, generation: u64) -> bool {
        self.key.is_some() && self.generation == generation
    }
}

thread_local! {
    // The event loop is single-threaded; keeping this here rather than in the
    // compositor state keeps the feature in one file.
    static CONFIG: Option<Config> = config_from(
        std::env::var("TVBOX_WC_RECOVERY_CMD").ok(),
        std::env::var("TVBOX_WC_RECOVERY_KEYS").ok(),
        std::env::var("TVBOX_WC_RECOVERY_HOLD_MS").ok(),
        std::env::var("HOME").ok(),
    );
    static HOLD: RefCell<Hold> = RefCell::new(Hold::default());
}

/// Feed every keyboard event through here, with the key code in evdev numbering
/// (xkb's minus 8). Never consumes the key.
pub fn on_key(state: &mut Tvbox, evdev_code: u32, key_state: KeyState) {
    let Some(config) = CONFIG.with(|c| c.clone()) else {
        return;
    };
    let pressed = key_state == KeyState::Pressed;
    let Some(generation) = HOLD.with(|h| h.borrow_mut().key(evdev_code, pressed, &config.keys))
    else {
        return;
    };
    for (after, stage) in config.stages.iter().copied() {
        let command = config.command.clone();
        let armed = state
            .loop_handle
            .insert_source(Timer::from_duration(after), move |_, _, _| {
                if HOLD.with(|h| h.borrow().current(generation)) {
                    run(&command, stage);
                }
                TimeoutAction::Drop
            });
        if let Err(err) = armed {
            warn!(?err, "could not arm the recovery hold timer");
        }
    }
}

fn run(command: &PathBuf, stage: &str) {
    if !command.exists() {
        warn!(command = %command.display(), stage, "recovery hold: no such command");
        return;
    }
    info!(command = %command.display(), stage, "recovery hold");
    match std::process::Command::new(command)
        .arg(stage)
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        // Reaped off the event loop: the restart stage waits for the shell to go.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => warn!(?err, stage, "recovery hold: could not run the command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(cmd: Option<&str>, keys: Option<&str>, hold: Option<&str>) -> Option<Config> {
        config_from(
            cmd.map(str::to_owned),
            keys.map(str::to_owned),
            hold.map(str::to_owned),
            Some("/home/tv".to_owned()),
        )
    }

    #[test]
    fn defaults_to_home_held_for_three_then_ten_seconds() {
        let c = env(None, None, None).unwrap();
        assert_eq!(c.command, PathBuf::from("/home/tv/.tvbox/recover.sh"));
        assert_eq!(c.keys, vec![172]);
        assert_eq!(
            c.stages,
            vec![
                (Duration::from_secs(3), "reload"),
                (Duration::from_secs(10), "restart")
            ]
        );
    }

    #[test]
    fn an_empty_command_switches_it_off() {
        assert_eq!(env(Some(""), None, None), None);
        assert_eq!(env(Some("  "), None, None), None);
    }

    #[test]
    fn keys_and_thresholds_can_be_set() {
        let c = env(Some("/x"), Some("172, 158"), Some("2000")).unwrap();
        assert_eq!(c.keys, vec![172, 158]);
        assert_eq!(c.stages, vec![(Duration::from_secs(2), "reload")]);
    }

    #[test]
    fn a_threshold_list_that_makes_no_sense_falls_back_to_the_default() {
        for bad in ["", "abc", "5000,1000", "0,1000"] {
            let c = env(Some("/x"), None, Some(bad)).unwrap();
            assert_eq!(c.stages.len(), 2, "{bad}");
            assert_eq!(c.stages[0].0, Duration::from_secs(3), "{bad}");
        }
    }

    #[test]
    fn a_hold_is_armed_by_the_watched_key_only() {
        let mut h = Hold::default();
        assert_eq!(h.key(28, true, &[172]), None);
        h.key(28, false, &[172]);
        let g = h.key(172, true, &[172]).unwrap();
        assert!(h.current(g));
    }

    #[test]
    fn letting_go_ends_the_hold() {
        let mut h = Hold::default();
        let g = h.key(172, true, &[172]).unwrap();
        h.key(172, false, &[172]);
        assert!(!h.current(g));
    }

    #[test]
    fn another_key_during_the_hold_cancels_it() {
        let mut h = Hold::default();
        let g = h.key(172, true, &[172]).unwrap();
        assert_eq!(h.key(103, true, &[172]), None);
        assert!(!h.current(g));
        // ...and the release of the key that was held does not start anything.
        h.key(172, false, &[172]);
        assert!(!h.current(g));
    }

    #[test]
    fn a_new_hold_does_not_revive_the_old_one() {
        let mut h = Hold::default();
        let first = h.key(172, true, &[172]).unwrap();
        h.key(172, false, &[172]);
        let second = h.key(172, true, &[172]).unwrap();
        assert!(!h.current(first));
        assert!(h.current(second));
    }

    #[test]
    fn releasing_a_key_that_is_not_held_changes_nothing() {
        let mut h = Hold::default();
        let g = h.key(172, true, &[172]).unwrap();
        h.key(103, false, &[172]);
        assert!(h.current(g));
    }
}
