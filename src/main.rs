use anyhow::Result;
use tracing_subscriber::EnvFilter;

/// Where the log goes: `$TVBOX_WC_LOG`, else `~/.tvbox/wc.log` when that directory
/// exists (a box), else nowhere and the output stays on stdout (a dev machine).
fn log_file() -> Option<std::fs::File> {
    let path = match std::env::var("TVBOX_WC_LOG") {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => {
            let home = std::env::var("HOME").ok()?;
            let dir = std::path::Path::new(&home).join(".tvbox");
            if !dir.is_dir() {
                return None;
            }
            dir.join("wc.log")
        }
    };
    std::fs::File::create(path).ok()
}

fn main() -> Result<()> {
    let (options, early) = match tvbox_wc::cli::parse(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("tvbox-wc: {message}\n\n{}", tvbox_wc::cli::USAGE);
            std::process::exit(2);
        }
    };
    match early {
        Some(tvbox_wc::cli::Early::Help) => {
            print!("{}", tvbox_wc::cli::USAGE);
            return Ok(());
        }
        Some(tvbox_wc::cli::Early::Version) => {
            println!("tvbox-wc {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        None => {}
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // greetd hands a session stdout and stderr on the VT it started, so without this
    // the compositor's log is written to a tty nobody is looking at and kept
    // nowhere - which is what debugging a client that will not start looks like when
    // the only two things that know anything are the client and the compositor.
    //
    // Truncated at every start rather than rotated: it is a log for the session
    // running now, and a box that has been up for weeks should not be paying for
    // the ones before it.
    match log_file() {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }

    tvbox_wc::run(options)
}
