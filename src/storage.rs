use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Error, Result,
    domain::{NewsEvidence, Opportunity, PaperPortfolio, ScanCapture, ScanSummary},
};

#[async_trait]
pub trait Store: Send + Sync {
    async fn acquire_scan_lease(&self, lease_id: Uuid, ttl: Duration) -> Result<bool>;
    async fn release_scan_lease(&self, lease_id: Uuid) -> Result<()>;
    async fn load_portfolio(&self, bankroll: Decimal) -> Result<PaperPortfolio>;
    async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()>;
    /// Persist a scan and the inputs it decided on (markets, books, quotes).
    async fn save_scan(&self, capture: &ScanCapture) -> Result<()>;
    async fn latest_opportunities(&self) -> Result<Vec<Opportunity>>;
    /// Timing and source health of the most recent scan, without its rows.
    async fn latest_scan(&self) -> Result<Option<ScanSummary>>;
    async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>>;
    async fn enqueue_news(&self, opportunities: &[Opportunity]) -> Result<()>;
    async fn save_news(&self, evidence: &NewsEvidence) -> Result<()>;
    /// Remove and return every queued news candidate. Stores whose queue is
    /// consumed elsewhere (SQS) return nothing.
    async fn take_news_queue(&self) -> Result<Vec<Opportunity>> {
        Ok(Vec::new())
    }
    /// Delete scan history older than `retention`. No-op for stores with
    /// lifecycle rules of their own.
    async fn prune(&self, _retention: Duration) -> Result<usize> {
        Ok(0)
    }
}

/// Whether a binary writes scans/news that belong in the history archive.
/// Viewers and the paper CLI only read, so they never open a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    Enabled,
    Disabled,
}

/// Build the store for the configured run mode. Cloud requires the `aws`
/// feature and `DATA_BUCKET`/`STATE_TABLE`/`NEWS_QUEUE_URL`. With
/// `Archive::Enabled` and `DATABASE_URL` set, the store is wrapped so every
/// scan and news record is also archived in Postgres (`postgres` feature).
pub async fn store_for(
    settings: &crate::config::Settings,
    data_dir: &str,
    archive: Archive,
) -> Result<Arc<dyn Store>> {
    let store: Arc<dyn Store> = match settings.run_mode {
        crate::config::RunMode::Local => Arc::new(LocalStore::new(data_dir)?),
        #[cfg(feature = "aws")]
        crate::config::RunMode::Cloud => Arc::new(aws::AwsStore::from_env().await?),
        #[cfg(not(feature = "aws"))]
        crate::config::RunMode::Cloud => {
            return Err(Error::Config(
                "RUN_MODE=cloud requires a build with the aws feature".into(),
            ));
        }
    };
    let Some(url) = settings
        .database_url
        .as_ref()
        .filter(|_| archive == Archive::Enabled)
    else {
        return Ok(store);
    };
    #[cfg(feature = "postgres")]
    {
        let history = crate::history::History::connect(url).await?;
        Ok(Arc::new(ArchivingStore::new(store, history, settings)?))
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = url;
        Err(Error::Config(
            "DATABASE_URL requires a build with the postgres feature".into(),
        ))
    }
}

/// Forwards everything to the operational store and additionally archives
/// scans and news evidence in Postgres. An archive failure fails the write:
/// the operator configured the archive, and a silent gap would corrupt every
/// backtest over the window.
#[cfg(feature = "postgres")]
pub struct ArchivingStore {
    inner: Arc<dyn Store>,
    history: crate::history::History,
    settings: serde_json::Value,
}

#[cfg(feature = "postgres")]
impl ArchivingStore {
    pub fn new(
        inner: Arc<dyn Store>,
        history: crate::history::History,
        settings: &crate::config::Settings,
    ) -> Result<Self> {
        Ok(Self {
            inner,
            history,
            settings: serde_json::to_value(settings)?,
        })
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl Store for ArchivingStore {
    async fn acquire_scan_lease(&self, lease_id: Uuid, ttl: Duration) -> Result<bool> {
        self.inner.acquire_scan_lease(lease_id, ttl).await
    }
    async fn release_scan_lease(&self, lease_id: Uuid) -> Result<()> {
        self.inner.release_scan_lease(lease_id).await
    }
    async fn load_portfolio(&self, bankroll: Decimal) -> Result<PaperPortfolio> {
        self.inner.load_portfolio(bankroll).await
    }
    async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()> {
        self.inner.save_portfolio(portfolio).await
    }
    async fn save_scan(&self, capture: &ScanCapture) -> Result<()> {
        self.inner.save_scan(capture).await?;
        self.history
            .record_scan(capture, &self.settings, crate::history::Origin::Live)
            .await?;
        Ok(())
    }
    async fn latest_opportunities(&self) -> Result<Vec<Opportunity>> {
        self.inner.latest_opportunities().await
    }
    async fn latest_scan(&self) -> Result<Option<ScanSummary>> {
        self.inner.latest_scan().await
    }
    async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>> {
        self.inner.news_for(opportunity_id).await
    }
    async fn enqueue_news(&self, opportunities: &[Opportunity]) -> Result<()> {
        self.inner.enqueue_news(opportunities).await
    }
    async fn save_news(&self, evidence: &NewsEvidence) -> Result<()> {
        self.inner.save_news(evidence).await?;
        self.history.record_news(evidence).await
    }
    async fn take_news_queue(&self) -> Result<Vec<Opportunity>> {
        self.inner.take_news_queue().await
    }
    async fn prune(&self, retention: Duration) -> Result<usize> {
        self.inner.prune(retention).await
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalLease {
    lease_id: Uuid,
    expires_at: DateTime<Utc>,
}

/// Write to a sibling temp file and rename over the target. `latest-*.json`,
/// `portfolio.json` and `health.json` are re-read every couple of seconds by
/// the UI or a watchdog, and rename is the only atomic replace a plain
/// filesystem offers.
pub fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::Storage(format!("create {}: {error}", parent.display())))?;
    }
    let content = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, content)
        .map_err(|error| Error::Storage(format!("write {}: {error}", temporary.display())))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::Storage(format!("replace {}: {error}", path.display()))
    })
}

pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .map_err(|error| Error::Storage(format!("create {}: {error}", root.display())))?;
        Ok(Self { root })
    }

    fn write_json(&self, path: &Path, value: &impl serde::Serialize) -> Result<()> {
        write_json_atomic(path, value)
    }

    fn append_json_line(&self, path: &Path, value: &impl serde::Serialize) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::Storage(format!("create {}: {error}", parent.display())))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| Error::Storage(format!("open {}: {error}", path.display())))?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")
            .map_err(|error| Error::Storage(format!("append {}: {error}", path.display())))
    }
}

/// Read one file; `None` when it does not exist.
async fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Storage(format!("read {}: {error}", path.display()))),
    }
}

/// Decode one JSON record. The error names the file so an operator can tell
/// a schema change (delete the file, or let the next scan rewrite
/// `latest-*.json`) from a corrupt store.
fn decode<T: serde::de::DeserializeOwned>(path: &Path, content: &[u8]) -> Result<T> {
    serde_json::from_slice(content).map_err(|error| {
        Error::Storage(format!(
            "decode {}: {error} (schema change? delete the file or let the next scan rewrite it)",
            path.display()
        ))
    })
}

/// State files (`portfolio.json`, news evidence): a decode failure is an error.
async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    read_bytes(path)
        .await?
        .map(|content| decode(path, &content))
        .transpose()
}

/// `latest-*.json` are caches of the most recent scan that the next scan
/// rewrites in full. One written by an older binary (schema change) must not
/// wedge a viewer or the scan loop, and the UI polls every 2 s, so it is moved
/// aside to `<name>.stale` once (nothing is destroyed) and then reads as
/// absent. IO errors still propagate.
async fn read_cache<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(content) = read_bytes(path).await? else {
        return Ok(None);
    };
    match decode(path, &content) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            let aside = path.with_extension("json.stale");
            match tokio::fs::rename(path, &aside).await {
                Ok(()) => {
                    tracing::warn!(%error, aside = %aside.display(), "moved stale cache aside")
                }
                Err(rename_error) => tracing::warn!(%error, %rename_error, "ignoring stale cache"),
            }
            Ok(None)
        }
    }
}

#[async_trait]
impl Store for LocalStore {
    async fn acquire_scan_lease(&self, lease_id: Uuid, ttl: Duration) -> Result<bool> {
        let path = self.root.join("scanner.lock");
        for _ in 0..2 {
            let lease = LocalLease {
                lease_id,
                expires_at: Utc::now()
                    + chrono::Duration::from_std(ttl)
                        .map_err(|error| Error::Storage(format!("lease TTL: {error}")))?,
            };
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    serde_json::to_writer(&mut file, &lease)?;
                    file.write_all(b"\n").map_err(|error| {
                        Error::Storage(format!("write {}: {error}", path.display()))
                    })?;
                    return Ok(true);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::read(&path)
                        .ok()
                        .and_then(|content| serde_json::from_slice::<LocalLease>(&content).ok())
                        .is_none_or(|current| current.expires_at <= Utc::now());
                    if !stale {
                        return Ok(false);
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(remove_error)
                            if remove_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(remove_error) => {
                            return Err(Error::Storage(format!(
                                "remove stale {}: {remove_error}",
                                path.display()
                            )));
                        }
                    }
                }
                Err(error) => {
                    return Err(Error::Storage(format!(
                        "create {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(false)
    }

    async fn release_scan_lease(&self, lease_id: Uuid) -> Result<()> {
        let path = self.root.join("scanner.lock");
        let Ok(content) = fs::read(&path) else {
            return Ok(());
        };
        let current: LocalLease = serde_json::from_slice(&content)?;
        if current.lease_id == lease_id {
            fs::remove_file(&path)
                .map_err(|error| Error::Storage(format!("remove {}: {error}", path.display())))?;
        }
        Ok(())
    }

    async fn load_portfolio(&self, bankroll: Decimal) -> Result<PaperPortfolio> {
        match read_json(&self.root.join("portfolio.json")).await? {
            Some(portfolio) => normalize_portfolio(portfolio, bankroll),
            None => Ok(empty_portfolio(bankroll)),
        }
    }

    async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()> {
        self.write_json(&self.root.join("portfolio.json"), portfolio)
    }

    async fn save_scan(&self, capture: &ScanCapture) -> Result<()> {
        let snapshot = &capture.snapshot;
        let date = snapshot.started_at;
        let partition = self.root.join(format!(
            "scans/year={}/month={:02}/day={:02}",
            date.year(),
            date.month(),
            date.day()
        ));
        self.write_json(
            &partition.join(format!("{}.json", snapshot.scan_id)),
            snapshot,
        )?;
        self.write_json(
            &partition.join(format!("{}-quotes.json", snapshot.scan_id)),
            &capture.quotes,
        )?;
        self.write_json(
            &partition.join(format!("{}-markets.json", snapshot.scan_id)),
            &capture.markets,
        )?;
        self.write_json(
            &partition.join(format!("{}-books.json", snapshot.scan_id)),
            &capture.books,
        )?;
        self.write_json(
            &self.root.join("latest-opportunities.json"),
            &snapshot.opportunities,
        )?;
        self.write_json(&self.root.join("latest-scan.json"), &snapshot.summary())?;
        // Rejected rows live in the per-scan snapshot; the running history
        // only keeps candidates so it stays small enough to grep.
        for opportunity in snapshot
            .opportunities
            .iter()
            .filter(|item| item.class != crate::domain::RecommendationClass::Rejected)
        {
            self.append_json_line(&self.root.join("opportunities.ndjson"), opportunity)?;
        }
        Ok(())
    }

    async fn latest_scan(&self) -> Result<Option<ScanSummary>> {
        read_cache(&self.root.join("latest-scan.json")).await
    }
    async fn latest_opportunities(&self) -> Result<Vec<Opportunity>> {
        Ok(read_cache(&self.root.join("latest-opportunities.json"))
            .await?
            .unwrap_or_default())
    }

    async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>> {
        read_json(
            &self
                .root
                .join("news")
                .join(format!("{opportunity_id}.json")),
        )
        .await
    }

    async fn enqueue_news(&self, opportunities: &[Opportunity]) -> Result<()> {
        for opportunity in opportunities {
            self.append_json_line(&self.root.join("news-queue.ndjson"), opportunity)?;
        }
        Ok(())
    }

    async fn save_news(&self, evidence: &NewsEvidence) -> Result<()> {
        self.write_json(
            &self
                .root
                .join("news")
                .join(format!("{}.json", evidence.opportunity_id)),
            evidence,
        )
    }

    async fn take_news_queue(&self) -> Result<Vec<Opportunity>> {
        let path = self.root.join("news-queue.ndjson");
        if !path.exists() {
            return Ok(Vec::new());
        }
        // Rename first so a concurrent scan appends to a fresh file instead of
        // racing the read.
        let taken = self.root.join("news-queue.processing.ndjson");
        fs::rename(&path, &taken)
            .map_err(|error| Error::Storage(format!("rotate {}: {error}", path.display())))?;
        let content = fs::read_to_string(&taken)
            .map_err(|error| Error::Storage(format!("read {}: {error}", taken.display())))?;
        let mut queued = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Opportunity>(line) {
                Ok(opportunity) => queued.push(opportunity),
                Err(error) => tracing::warn!(%error, "skipping malformed news queue line"),
            }
        }
        fs::remove_file(&taken)
            .map_err(|error| Error::Storage(format!("remove {}: {error}", taken.display())))?;
        Ok(queued)
    }

    async fn prune(&self, retention: Duration) -> Result<usize> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(retention)
            .unwrap_or(std::time::UNIX_EPOCH);
        let scans = self.root.join("scans");
        if !scans.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        let mut stack = vec![scans];
        while let Some(directory) = stack.pop() {
            let entries = fs::read_dir(&directory).map_err(|error| {
                Error::Storage(format!("read {}: {error}", directory.display()))
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| Error::Storage(error.to_string()))?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .map_err(|error| Error::Storage(error.to_string()))?;
                if modified < cutoff {
                    fs::remove_file(&path).map_err(|error| {
                        Error::Storage(format!("remove {}: {error}", path.display()))
                    })?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(feature = "aws")]
pub mod aws {
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_s3::primitives::ByteStream;

    use super::*;

    pub struct AwsStore {
        bucket: String,
        table: String,
        queue_url: String,
        s3: aws_sdk_s3::Client,
        dynamodb: aws_sdk_dynamodb::Client,
        sqs: aws_sdk_sqs::Client,
    }

    impl AwsStore {
        pub async fn from_env() -> Result<Self> {
            let config = aws_config::load_from_env().await;
            Ok(Self {
                bucket: std::env::var("DATA_BUCKET")
                    .map_err(|_| Error::Config("DATA_BUCKET is required".into()))?,
                table: std::env::var("STATE_TABLE")
                    .map_err(|_| Error::Config("STATE_TABLE is required".into()))?,
                queue_url: std::env::var("NEWS_QUEUE_URL")
                    .map_err(|_| Error::Config("NEWS_QUEUE_URL is required".into()))?,
                s3: aws_sdk_s3::Client::new(&config),
                dynamodb: aws_sdk_dynamodb::Client::new(&config),
                sqs: aws_sdk_sqs::Client::new(&config),
            })
        }

        async fn put_json(&self, key: String, value: &impl serde::Serialize) -> Result<()> {
            let bytes = serde_json::to_vec(value)?;
            self.s3
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type("application/json")
                .body(ByteStream::from(bytes))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            Ok(())
        }

        async fn put_latest(&self, sort_key: &str, value: &impl serde::Serialize) -> Result<()> {
            self.dynamodb
                .put_item()
                .table_name(&self.table)
                .item("pk", AttributeValue::S("LATEST".into()))
                .item("sk", AttributeValue::S(sort_key.into()))
                .item("record", AttributeValue::S(serde_json::to_string(value)?))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            Ok(())
        }

        async fn get_latest<T: serde::de::DeserializeOwned>(
            &self,
            sort_key: &str,
        ) -> Result<Option<T>> {
            let output = self
                .dynamodb
                .get_item()
                .table_name(&self.table)
                .key("pk", AttributeValue::S("LATEST".into()))
                .key("sk", AttributeValue::S(sort_key.into()))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            let Some(item) = output.item else {
                return Ok(None);
            };
            let record = item
                .get("record")
                .and_then(|value| value.as_s().ok())
                .ok_or_else(|| Error::Storage(format!("latest {sort_key} record is malformed")))?;
            Ok(Some(serde_json::from_str(record)?))
        }
    }

    #[async_trait]
    impl Store for AwsStore {
        async fn acquire_scan_lease(&self, lease_id: Uuid, ttl: Duration) -> Result<bool> {
            let now = Utc::now().timestamp();
            let expires_at = now
                + i64::try_from(ttl.as_secs())
                    .map_err(|error| Error::Storage(format!("lease TTL: {error}")))?;
            let result = self
                .dynamodb
                .put_item()
                .table_name(&self.table)
                .item("pk", AttributeValue::S("LOCK#SCANNER".into()))
                .item("sk", AttributeValue::S("LEASE".into()))
                .item("lease_id", AttributeValue::S(lease_id.to_string()))
                .item("expires_at", AttributeValue::N(expires_at.to_string()))
                .condition_expression("attribute_not_exists(pk) OR expires_at < :now")
                .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                .send()
                .await;
            match result {
                Ok(_) => Ok(true),
                Err(error)
                    if error
                        .as_service_error()
                        .is_some_and(|service| service.is_conditional_check_failed_exception()) =>
                {
                    Ok(false)
                }
                Err(error) => Err(Error::Storage(error.to_string())),
            }
        }

        async fn release_scan_lease(&self, lease_id: Uuid) -> Result<()> {
            let result = self
                .dynamodb
                .delete_item()
                .table_name(&self.table)
                .key("pk", AttributeValue::S("LOCK#SCANNER".into()))
                .key("sk", AttributeValue::S("LEASE".into()))
                .condition_expression("lease_id = :lease_id")
                .expression_attribute_values(":lease_id", AttributeValue::S(lease_id.to_string()))
                .send()
                .await;
            match result {
                Ok(_) => Ok(()),
                Err(error)
                    if error
                        .as_service_error()
                        .is_some_and(|service| service.is_conditional_check_failed_exception()) =>
                {
                    Ok(())
                }
                Err(error) => Err(Error::Storage(error.to_string())),
            }
        }

        async fn load_portfolio(&self, bankroll: Decimal) -> Result<PaperPortfolio> {
            let output = self
                .dynamodb
                .get_item()
                .table_name(&self.table)
                .key("pk", AttributeValue::S("PORTFOLIO".into()))
                .key("sk", AttributeValue::S("PAPER".into()))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            let Some(item) = output.item else {
                return Ok(empty_portfolio(bankroll));
            };
            let record = item
                .get("record")
                .and_then(|value| value.as_s().ok())
                .ok_or_else(|| Error::Storage("paper portfolio record is malformed".into()))?;
            normalize_portfolio(serde_json::from_str(record)?, bankroll)
        }

        async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()> {
            self.dynamodb
                .put_item()
                .table_name(&self.table)
                .item("pk", AttributeValue::S("PORTFOLIO".into()))
                .item("sk", AttributeValue::S("PAPER".into()))
                .item(
                    "record",
                    AttributeValue::S(serde_json::to_string(portfolio)?),
                )
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            Ok(())
        }

        async fn save_scan(&self, capture: &ScanCapture) -> Result<()> {
            let snapshot = &capture.snapshot;
            let date = snapshot.started_at;
            let prefix = format!(
                "scans/year={}/month={:02}/day={:02}/{}",
                date.year(),
                date.month(),
                date.day(),
                snapshot.scan_id
            );
            self.put_json(format!("{prefix}.json"), snapshot).await?;
            self.put_json(format!("{prefix}-quotes.json"), &capture.quotes)
                .await?;
            self.put_json(format!("{prefix}-markets.json"), &capture.markets)
                .await?;
            self.put_json(format!("{prefix}-books.json"), &capture.books)
                .await?;
            for opportunity in &snapshot.opportunities {
                let item = serde_json::to_string(opportunity)?;
                self.dynamodb
                    .put_item()
                    .table_name(&self.table)
                    .item(
                        "pk",
                        AttributeValue::S(format!("OPPORTUNITY#{}", opportunity.id)),
                    )
                    .item(
                        "sk",
                        AttributeValue::S(opportunity.generated_at.to_rfc3339()),
                    )
                    .item("record", AttributeValue::S(item))
                    .item("entity", AttributeValue::S("opportunity".into()))
                    .send()
                    .await
                    .map_err(|error| Error::Storage(error.to_string()))?;
            }
            self.put_latest("OPPORTUNITIES", &snapshot.opportunities)
                .await?;
            self.put_latest("SCAN", &snapshot.summary()).await
        }

        async fn latest_opportunities(&self) -> Result<Vec<Opportunity>> {
            Ok(self
                .get_latest::<Vec<Opportunity>>("OPPORTUNITIES")
                .await?
                .unwrap_or_default())
        }

        async fn latest_scan(&self) -> Result<Option<ScanSummary>> {
            self.get_latest("SCAN").await
        }

        async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>> {
            let output = self
                .dynamodb
                .get_item()
                .table_name(&self.table)
                .key(
                    "pk",
                    AttributeValue::S(format!("OPPORTUNITY#{opportunity_id}")),
                )
                .key("sk", AttributeValue::S("NEWS".into()))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            let Some(item) = output.item else {
                return Ok(None);
            };
            let record = item
                .get("record")
                .and_then(|value| value.as_s().ok())
                .ok_or_else(|| Error::Storage("news record is malformed".into()))?;
            Ok(Some(serde_json::from_str(record)?))
        }

        async fn enqueue_news(&self, opportunities: &[Opportunity]) -> Result<()> {
            for opportunity in opportunities {
                self.sqs
                    .send_message()
                    .queue_url(&self.queue_url)
                    .message_body(serde_json::to_string(opportunity)?)
                    .send()
                    .await
                    .map_err(|error| Error::Storage(error.to_string()))?;
            }
            Ok(())
        }

        async fn save_news(&self, evidence: &NewsEvidence) -> Result<()> {
            self.dynamodb
                .put_item()
                .table_name(&self.table)
                .item(
                    "pk",
                    AttributeValue::S(format!("OPPORTUNITY#{}", evidence.opportunity_id)),
                )
                .item("sk", AttributeValue::S("NEWS".into()))
                .item(
                    "record",
                    AttributeValue::S(serde_json::to_string(evidence)?),
                )
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            Ok(())
        }
    }
}

fn empty_portfolio(bankroll: Decimal) -> PaperPortfolio {
    PaperPortfolio::new(bankroll)
}

/// `bankroll` is the configured sizing base; the stored bankroll and
/// realized P&L are derived from the positions on every load so a hand-edited
/// or older file cannot drift from them.
fn normalize_portfolio(mut portfolio: PaperPortfolio, bankroll: Decimal) -> Result<PaperPortfolio> {
    if bankroll <= Decimal::ZERO {
        return Err(Error::Config("PAPER_BANKROLL must be positive".into()));
    }
    if portfolio
        .open_positions
        .iter()
        .any(|position| position.maximum_loss <= Decimal::ZERO)
    {
        return Err(Error::Storage(
            "paper portfolio contains a non-positive position".into(),
        ));
    }
    portfolio.realized_pnl = portfolio
        .closed_positions
        .iter()
        .map(|position| position.realized_pnl.unwrap_or(Decimal::ZERO))
        .sum();
    portfolio.bankroll = bankroll + portfolio.realized_pnl;
    portfolio.open_exposure = portfolio
        .open_positions
        .iter()
        .map(|position| position.maximum_loss)
        .sum();
    if portfolio.open_exposure > portfolio.bankroll {
        return Err(Error::Storage(
            "paper portfolio exposure exceeds bankroll".into(),
        ));
    }
    Ok(portfolio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OutcomeSide, PaperPosition, RecommendationClass, Sport};

    fn opportunity(maximum_loss: Decimal) -> Opportunity {
        Opportunity {
            id: Uuid::new_v4(),
            generated_at: Utc::now(),
            class: RecommendationClass::Actionable,
            sport: Sport::Nba,
            event_id: "event".into(),
            market_id: "market".into(),
            market_slug: "market-slug".into(),
            participant: "Team".into(),
            side: OutcomeSide::Long,
            fair_probability: Decimal::new(55, 2),
            conservative_probability: Decimal::new(52, 2),
            executable_price: Decimal::new(50, 2),
            maker_price: None,
            maker_net_edge: None,
            raw_edge: Decimal::new(5, 2),
            net_edge: Decimal::new(2, 2),
            quantity: Decimal::new(4, 0),
            maximum_loss,
            estimated_fee: Decimal::new(1, 2),
            family_count: 3,
            source_ids: vec!["a".into(), "b".into(), "c".into()],
            start_time: Utc::now() + chrono::Duration::hours(2),
            book_time: Utc::now(),
            reasons: Vec::new(),
        }
    }

    #[tokio::test]
    async fn local_store_persists_portfolio_and_enforces_lease() {
        let root = std::env::temp_dir().join(format!("polybot-storage-test-{}", Uuid::new_v4()));
        let store = LocalStore::new(&root).unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(
            store
                .acquire_scan_lease(first, Duration::from_secs(30))
                .await
                .unwrap()
        );
        assert!(
            !store
                .acquire_scan_lease(second, Duration::from_secs(30))
                .await
                .unwrap()
        );
        store.release_scan_lease(first).await.unwrap();
        assert!(
            store
                .acquire_scan_lease(second, Duration::from_secs(30))
                .await
                .unwrap()
        );
        store.release_scan_lease(second).await.unwrap();

        let mut portfolio = PaperPortfolio::new(Decimal::ONE_HUNDRED);
        portfolio.open_exposure = Decimal::new(99, 0);
        portfolio
            .open_positions
            .push(PaperPosition::from_opportunity(
                &opportunity(Decimal::new(2, 0)),
                Utc::now(),
            ));
        let mut settled =
            PaperPosition::from_opportunity(&opportunity(Decimal::new(3, 0)), Utc::now());
        settled.realized_pnl = Some(Decimal::new(-3, 0));
        settled.closed_at = Some(Utc::now());
        portfolio.closed_positions.push(settled);
        portfolio
            .closed_positions
            .push(PaperPosition::from_opportunity(
                &opportunity(Decimal::new(1, 0)),
                Utc::now(),
            ));
        store.save_portfolio(&portfolio).await.unwrap();
        let loaded = store.load_portfolio(Decimal::ONE_HUNDRED).await.unwrap();
        assert_eq!(loaded.open_exposure, Decimal::new(2, 0));
        assert_eq!(loaded.open_positions.len(), 1);
        assert_eq!(loaded.closed_positions.len(), 2);
        // Realized P&L is recomputed from the settled records (a manual close
        // without a result counts as zero) and the bankroll carries it.
        assert_eq!(loaded.realized_pnl, Decimal::new(-3, 0));
        assert_eq!(loaded.bankroll, Decimal::new(97, 0));

        fs::remove_dir_all(root).unwrap();
    }

    /// A `latest-*.json` written by an older binary must not wedge a viewer:
    /// it is moved aside once and then reads as absent. `portfolio.json` is
    /// state and must fail loudly, naming the file.
    #[tokio::test]
    async fn stale_caches_are_moved_aside_but_state_files_are_strict() {
        let root = std::env::temp_dir().join(format!("polybot-storage-test-{}", Uuid::new_v4()));
        let store = LocalStore::new(&root).unwrap();
        fs::write(root.join("latest-opportunities.json"), br#"[{"id":"x"}]"#).unwrap();
        fs::write(root.join("latest-scan.json"), br#"{"scan_id":"nope"}"#).unwrap();
        fs::write(root.join("portfolio.json"), br#"{"bankroll":"100"}"#).unwrap();

        assert!(store.latest_opportunities().await.unwrap().is_empty());
        assert!(store.latest_scan().await.unwrap().is_none());
        assert!(!root.join("latest-opportunities.json").exists());
        assert_eq!(
            fs::read(root.join("latest-opportunities.json.stale")).unwrap(),
            br#"[{"id":"x"}]"#
        );
        assert!(root.join("latest-scan.json.stale").exists());
        let error = store
            .load_portfolio(Decimal::ONE_HUNDRED)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("portfolio.json"), "{error}");

        fs::remove_dir_all(root).unwrap();
    }
}
