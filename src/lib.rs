pub mod config;
pub mod consensus;
pub mod domain;
pub mod error;
pub mod matching;
pub mod news;
pub mod opportunity;
pub mod polymarket;
pub mod risk;
pub mod scanner;
pub mod sources;
pub mod storage;

pub use error::{Error, Result};

pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "polybot=info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
