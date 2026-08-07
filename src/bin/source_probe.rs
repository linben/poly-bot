use std::time::Duration;

use clap::Parser;
use futures::{StreamExt, stream};
use polybot::{
    Result,
    config::Settings,
    sources::{SourceCatalog, probe_url},
};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let catalog = SourceCatalog::load(&settings.source_config_path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("polybot-source-feasibility/0.1")
        .build()?;
    let health = stream::iter(catalog.specs)
        .map(|spec| {
            let client = client.clone();
            async move {
                let family = spec.source_family().expect("validated source family");
                probe_url(&client, &spec.id, family, &spec.homepage).await
            }
        })
        .buffer_unordered(args.concurrency)
        .collect::<Vec<_>>()
        .await;
    println!("{}", serde_json::to_string_pretty(&health)?);
    Ok(())
}
