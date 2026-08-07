use std::collections::HashSet;
use std::sync::Arc;

use clap::Parser;
use polybot::{
    Result,
    config::Settings,
    polymarket::PolymarketUsClient,
    scanner::Scanner,
    sources::SourceCatalog,
    storage::{LocalStore, Store},
};

#[derive(Debug, Parser)]
struct Args {
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
    let catalog = SourceCatalog::load(&settings.source_config_path)?;
    let sources = catalog.configured_sources(settings.request_timeout)?;
    let direct_sources = sources
        .iter()
        .filter(|source| !source.validation_only())
        .collect::<Vec<_>>();
    let families = direct_sources
        .iter()
        .map(|source| source.family())
        .collect::<HashSet<_>>();
    if direct_sources.len() < settings.minimum_configured_sources
        || families.len() < settings.minimum_configured_sources
        || !families.iter().any(|family| family.is_reference())
    {
        return Err(polybot::Error::Config(format!(
            "scanner requires {} independent direct sources including a reference book; found {} sources across {} families",
            settings.minimum_configured_sources,
            direct_sources.len(),
            families.len()
        )));
    }
    let polymarket = PolymarketUsClient::new(
        settings.polymarket_base_url.clone(),
        settings.request_timeout,
    )?;

    #[cfg(feature = "aws")]
    let store: Arc<dyn Store> = if std::env::var("STORAGE_MODE").as_deref() == Ok("aws") {
        Arc::new(polybot::storage::aws::AwsStore::from_env().await?)
    } else {
        Arc::new(LocalStore::new(&args.data_dir)?)
    };
    #[cfg(not(feature = "aws"))]
    let store: Arc<dyn Store> = Arc::new(LocalStore::new(&args.data_dir)?);

    let scanner = Scanner::new(settings.clone(), polymarket, sources, store);
    loop {
        let result = match scanner.run_once().await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(%error, "scan failed");
                return Err(error);
            }
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        if !args.continuous {
            break;
        }
        tokio::time::sleep(settings.scan_interval).await;
    }
    Ok(())
}
