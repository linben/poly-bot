pub mod config;
pub mod consensus;
pub mod dashboard;
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

/// Logs go to stderr; binaries print JSON results on stdout. The default
/// filter covers the library and every binary while muting HTTP internals;
/// override with `RUST_LOG`.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,hyper_util=warn,hyper=warn,reqwest=warn,rustls=warn,h2=warn,tower_http=warn".into()
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
