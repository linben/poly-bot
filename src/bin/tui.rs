//! Attach-only terminal UI over the configured store. Works against the local
//! `data/` directory or, with `RUN_MODE=cloud`, against DynamoDB/S3 using the
//! operator's AWS credentials. Nothing here scans or writes except paper
//! open/close.

use std::sync::Arc;

use clap::Parser;
use polybot::{
    Result,
    config::Settings,
    storage::{Archive, store_for},
    tui::{self, TuiOptions},
};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "data", env = "DATA_DIR")]
    data_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let store = store_for(&settings, &args.data_dir, Archive::Disabled).await?;
    // Logs would corrupt the screen; the attached viewer has nothing to log.
    let logs = tui::log::LogBuffer::install(tracing_subscriber::EnvFilter::new("warn"), 200);
    tui::run(TuiOptions {
        settings,
        store: Arc::clone(&store),
        engine: None,
        logs,
    })
    .await
}
