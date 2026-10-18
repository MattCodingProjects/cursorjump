#[cfg(windows)]
mod win;

use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    #[cfg(windows)]
    win::init_process();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--diagnose") {
        #[cfg(windows)]
        {
            win::run_diagnose(std::time::Duration::from_secs(diagnose_seconds(&args)));
            return;
        }
        #[cfg(not(windows))]
        {
            tracing::error!("--diagnose is only available on Windows");
            std::process::exit(2);
        }
    }
    println!("cursorjump {}", env!("CARGO_PKG_VERSION"));
}

/// `--diagnose-seconds N` (default 120).
#[cfg(windows)]
fn diagnose_seconds(args: &[String]) -> u64 {
    args.iter()
        .position(|a| a == "--diagnose-seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(120)
}
