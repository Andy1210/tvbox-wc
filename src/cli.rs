//! The command line.
//!
//! Deliberately tiny: this is started by greetd, with one argument shape.

/// How the compositor was asked to run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The session to start once the compositor is up, if any.
    ///
    /// Everything after `--`. It inherits `WAYLAND_DISPLAY` and
    /// `TVBOX_WC_SOCKET`, and when it exits the compositor stops - a session that
    /// has ended is the end of the session.
    pub session: Option<Vec<String>>,
}

/// What to do instead of running.
#[derive(Debug, PartialEq, Eq)]
pub enum Early {
    /// Print usage.
    Help,
    /// Print the version.
    Version,
}

/// Parse the arguments, without the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<(Options, Option<Early>), String> {
    let mut options = Options::default();
    let mut args = args.into_iter();

    let Some(arg) = args.next() else {
        return Ok((options, None));
    };
    match arg.as_str() {
        "--" => {
            let session: Vec<String> = args.collect();
            if session.is_empty() {
                return Err("-- needs a command after it".to_owned());
            }
            options.session = Some(session);
            Ok((options, None))
        }
        "-h" | "--help" => Ok((options, Some(Early::Help))),
        "-V" | "--version" => Ok((options, Some(Early::Version))),
        other => Err(format!("unknown argument: {other}")),
    }
}

/// The usage text.
pub const USAGE: &str = "\
tvbox-wc - the Wayland compositor for the tvbox

Usage: tvbox-wc [OPTIONS] [-- COMMAND [ARGS...]]

  -- COMMAND    start COMMAND once the compositor is up, and stop when it exits
  -h, --help    print this
  -V, --version print the version
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<(Options, Option<Early>), String> {
        parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn no_arguments_is_a_compositor_with_no_session() {
        assert_eq!(parse_args(&[]).unwrap(), (Options::default(), None));
    }

    #[test]
    fn everything_after_the_separator_is_the_session() {
        // The session script takes its own flags, and none of them are ours.
        let (options, early) =
            parse_args(&["--", "/usr/local/bin/tvbox-session", "--verbose"]).unwrap();
        assert_eq!(early, None);
        assert_eq!(
            options.session,
            Some(vec![
                "/usr/local/bin/tvbox-session".to_owned(),
                "--verbose".to_owned()
            ])
        );
    }

    #[test]
    fn a_separator_with_nothing_after_it_is_a_mistake() {
        assert!(parse_args(&["--"]).is_err());
    }

    #[test]
    fn an_unknown_argument_is_refused_rather_than_ignored() {
        // greetd shows no output, so a silently ignored argument would look like a
        // setting that does not work.
        assert!(parse_args(&["--sesion", "x"]).is_err());
    }
}
