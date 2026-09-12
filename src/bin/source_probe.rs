use clap::Parser;
use futures::{StreamExt, stream};
use polybot::{
    Result,
    config::Settings,
    sources::{SourceCatalog, http_client, probe_url},
};

/// Probes every configured adapter by running a real collection, and checks
/// basic reachability of the catalog homepages for books that still need a
/// reviewed endpoint.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Skip catalog homepage reachability checks.
    #[arg(long)]
    adapters_only: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let catalog = SourceCatalog::load(&settings.source_config_path)?;
    let sources = catalog.configured_sources(settings.request_timeout)?;
    let mut health = stream::iter(sources)
        .map(|source| async move { source.probe().await })
        .buffer_unordered(args.concurrency)
        .collect::<Vec<_>>()
        .await;

    if !args.adapters_only {
        let client = http_client(settings.request_timeout)?;
        let homepages = stream::iter(catalog.specs)
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
        health.extend(homepages);
    }
    println!("{}", serde_json::to_string_pretty(&health)?);
    Ok(())
}
