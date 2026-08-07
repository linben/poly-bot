use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use polybot::{
    Error, Result,
    config::Settings,
    domain::{PaperPosition, RecommendationClass, ResearchOpportunity},
    storage::{LocalStore, Store},
};
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "data")]
    data_dir: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    List,
    Open { opportunity_id: Uuid },
    Close { opportunity_id: Uuid },
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;

    #[cfg(feature = "aws")]
    let store: Arc<dyn Store> = if std::env::var("STORAGE_MODE").as_deref() == Ok("aws") {
        Arc::new(polybot::storage::aws::AwsStore::from_env().await?)
    } else {
        Arc::new(LocalStore::new(&args.data_dir)?)
    };
    #[cfg(not(feature = "aws"))]
    let store: Arc<dyn Store> = Arc::new(LocalStore::new(&args.data_dir)?);

    let mut portfolio = store.load_portfolio(settings.bankroll).await?;
    match args.command {
        Command::List => {}
        Command::Open { opportunity_id } => {
            let opportunity = store
                .latest_opportunities()
                .await?
                .into_iter()
                .find(|item| item.id == opportunity_id)
                .ok_or_else(|| {
                    Error::InvalidData("opportunity is not in the latest scan".into())
                })?;
            let news = store.news_for(opportunity.id).await?;
            let research = ResearchOpportunity::new(opportunity, news);
            if research.effective_class != RecommendationClass::Actionable {
                return Err(Error::InvalidData(
                    "paper position requires an actionable, news-reviewed opportunity".into(),
                ));
            }
            if portfolio.has_market_side(&research.opportunity.market_id, research.opportunity.side)
            {
                return Err(Error::InvalidData(
                    "paper portfolio already contains this market side".into(),
                ));
            }
            let position = PaperPosition {
                opportunity_id: research.opportunity.id,
                event_id: research.opportunity.event_id,
                market_id: research.opportunity.market_id,
                side: research.opportunity.side,
                maximum_loss: research.opportunity.maximum_loss,
                opened_at: Utc::now(),
            };
            if !portfolio.open(position, settings.maximum_total_exposure) {
                return Err(Error::InvalidData(
                    "paper position violates bankroll or exposure limits".into(),
                ));
            }
            store.save_portfolio(&portfolio).await?;
        }
        Command::Close { opportunity_id } => {
            let original_len = portfolio.open_positions.len();
            portfolio
                .open_positions
                .retain(|position| position.opportunity_id != opportunity_id);
            if portfolio.open_positions.len() == original_len {
                return Err(Error::InvalidData("paper position was not found".into()));
            }
            portfolio.open_exposure = portfolio
                .open_positions
                .iter()
                .map(|position| position.maximum_loss)
                .sum();
            store.save_portfolio(&portfolio).await?;
        }
    }
    println!("{}", serde_json::to_string_pretty(&portfolio)?);
    Ok(())
}
