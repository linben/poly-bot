use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Error, Result,
    domain::{NewsEvidence, Opportunity, PaperPortfolio, ScanSnapshot, SourceQuote},
};

#[async_trait]
pub trait Store: Send + Sync {
    async fn acquire_scan_lease(&self, lease_id: Uuid, ttl: Duration) -> Result<bool>;
    async fn release_scan_lease(&self, lease_id: Uuid) -> Result<()>;
    async fn load_portfolio(&self, bankroll: Decimal) -> Result<PaperPortfolio>;
    async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()>;
    async fn save_scan(&self, snapshot: &ScanSnapshot, quotes: &[SourceQuote]) -> Result<()>;
    async fn latest_opportunities(&self) -> Result<Vec<Opportunity>>;
    async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>>;
    async fn enqueue_news(&self, opportunities: &[Opportunity]) -> Result<()>;
    async fn save_news(&self, evidence: &NewsEvidence) -> Result<()>;
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalLease {
    lease_id: Uuid,
    expires_at: DateTime<Utc>,
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::Storage(format!("create {}: {error}", parent.display())))?;
        }
        let content = serde_json::to_vec_pretty(value)?;
        fs::write(path, content)
            .map_err(|error| Error::Storage(format!("write {}: {error}", path.display())))
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
        let path = self.root.join("portfolio.json");
        if !path.exists() {
            return Ok(empty_portfolio(bankroll));
        }
        let content = fs::read(&path)
            .map_err(|error| Error::Storage(format!("read {}: {error}", path.display())))?;
        let portfolio = serde_json::from_slice(&content)?;
        normalize_portfolio(portfolio, bankroll)
    }

    async fn save_portfolio(&self, portfolio: &PaperPortfolio) -> Result<()> {
        self.write_json(&self.root.join("portfolio.json"), portfolio)
    }

    async fn save_scan(&self, snapshot: &ScanSnapshot, quotes: &[SourceQuote]) -> Result<()> {
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
            &quotes,
        )?;
        self.write_json(
            &self.root.join("latest-opportunities.json"),
            &snapshot.opportunities,
        )?;
        for opportunity in &snapshot.opportunities {
            self.append_json_line(&self.root.join("opportunities.ndjson"), opportunity)?;
        }
        Ok(())
    }

    async fn latest_opportunities(&self) -> Result<Vec<Opportunity>> {
        let path = self.root.join("latest-opportunities.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read(path)
            .map_err(|error| Error::Storage(format!("read opportunities: {error}")))?;
        Ok(serde_json::from_slice(&content)?)
    }

    async fn news_for(&self, opportunity_id: Uuid) -> Result<Option<NewsEvidence>> {
        let path = self
            .root
            .join("news")
            .join(format!("{opportunity_id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read(&path)
            .map_err(|error| Error::Storage(format!("read {}: {error}", path.display())))?;
        Ok(Some(serde_json::from_slice(&content)?))
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

        async fn save_scan(&self, snapshot: &ScanSnapshot, quotes: &[SourceQuote]) -> Result<()> {
            let date = snapshot.started_at;
            let prefix = format!(
                "scans/year={}/month={:02}/day={:02}/{}",
                date.year(),
                date.month(),
                date.day(),
                snapshot.scan_id
            );
            self.put_json(format!("{prefix}.json"), snapshot).await?;
            self.put_json(format!("{prefix}-quotes.json"), &quotes)
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
            self.dynamodb
                .put_item()
                .table_name(&self.table)
                .item("pk", AttributeValue::S("LATEST".into()))
                .item("sk", AttributeValue::S("OPPORTUNITIES".into()))
                .item(
                    "record",
                    AttributeValue::S(serde_json::to_string(&snapshot.opportunities)?),
                )
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            Ok(())
        }

        async fn latest_opportunities(&self) -> Result<Vec<Opportunity>> {
            let output = self
                .dynamodb
                .get_item()
                .table_name(&self.table)
                .key("pk", AttributeValue::S("LATEST".into()))
                .key("sk", AttributeValue::S("OPPORTUNITIES".into()))
                .send()
                .await
                .map_err(|error| Error::Storage(error.to_string()))?;
            let Some(item) = output.item else {
                return Ok(Vec::new());
            };
            let record = item
                .get("record")
                .and_then(|value| value.as_s().ok())
                .ok_or_else(|| Error::Storage("latest record is malformed".into()))?;
            Ok(serde_json::from_str(record)?)
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
    PaperPortfolio {
        bankroll,
        open_exposure: Decimal::ZERO,
        open_positions: Vec::new(),
    }
}

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
    portfolio.bankroll = bankroll;
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
    use crate::domain::{OutcomeSide, PaperPosition};

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

        let portfolio = PaperPortfolio {
            bankroll: Decimal::ONE_HUNDRED,
            open_exposure: Decimal::new(99, 0),
            open_positions: vec![PaperPosition {
                opportunity_id: Uuid::new_v4(),
                event_id: "event".into(),
                market_id: "market".into(),
                side: OutcomeSide::Long,
                maximum_loss: Decimal::new(2, 0),
                opened_at: Utc::now(),
            }],
        };
        store.save_portfolio(&portfolio).await.unwrap();
        let loaded = store.load_portfolio(Decimal::ONE_HUNDRED).await.unwrap();
        assert_eq!(loaded.open_exposure, Decimal::new(2, 0));
        assert_eq!(loaded.open_positions.len(), 1);

        fs::remove_dir_all(root).unwrap();
    }
}
