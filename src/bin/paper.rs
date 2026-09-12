use clap::{Parser, Subcommand};
use polybot::{
    Result,
    config::Settings,
    paper::{close_position, open_position},
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
    List,
    Open { opportunity_id: Uuid },
    Close { opportunity_id: Uuid },
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
    };
    println!("{}", serde_json::to_string_pretty(&portfolio)?);
    Ok(())
}
