use clap::{Parser, Subcommand};
use polybot::{
    Result,
    config::Settings,
    paper::{close_position, open_position, settle_positions},
    polymarket::PolymarketUsClient,
    storage::store_for,
};
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "data", env = "DATA_DIR")]
    data_dir: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the portfolio: open positions, closed positions with closing
    /// line and realized P&L, and the running bankroll.
    List,
    Open {
        opportunity_id: Uuid,
    },
    Close {
        opportunity_id: Uuid,
    },
    /// Record closing lines and settle started positions against the venue.
    Settle,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let store = store_for(settings.run_mode, &args.data_dir).await?;
    let portfolio = match args.command {
        Command::List => store.load_portfolio(settings.bankroll).await?,
        Command::Open { opportunity_id } => {
            open_position(store.as_ref(), &settings, opportunity_id).await?
        }
        Command::Close { opportunity_id } => {
            close_position(store.as_ref(), &settings, opportunity_id).await?
        }
        Command::Settle => {
            let client = PolymarketUsClient::new(
                settings.polymarket_base_url.clone(),
                settings.request_timeout,
            )?
            .with_rate_limit(settings.polymarket_requests_per_second);
            let report = settle_positions(store.as_ref(), &settings, &client).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&portfolio)?);
    Ok(())
}
