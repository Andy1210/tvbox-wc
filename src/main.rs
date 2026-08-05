use anyhow::Result;
use tracing_subscriber::EnvFilter;

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
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tvbox_wc::run(options)
}
