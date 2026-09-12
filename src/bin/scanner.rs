//! One-shot scan (the cloud ECS task) or a resilient continuous loop.

use clap::Parser;
use polybot::{Result, config::Settings, scanner::Scanner, storage::store_for};

#[derive(Debug, Parser)]
struct Args {
    /// Keep scanning on a fixed cadence; transient failures are logged, not fatal.
    #[arg(long)]
    continuous: bool,
    #[arg(long, default_value = "data")]
    data_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let store = store_for(settings.run_mode, &args.data_dir).await?;
    let scanner = Scanner::from_settings(settings.clone(), store)?;

    if !args.continuous {
        let snapshot = scanner.run_once().await?;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    let mut ticker = tokio::time::interval(settings.scan_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match scanner.run_once().await {
            Ok(snapshot) => println!("{}", serde_json::to_string(&snapshot)?),
            Err(error) => tracing::error!(%error, "scan failed; retrying next interval"),
        }
    }
}
