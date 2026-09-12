# Tech Stack

## Runtime

| Area | Technology | Purpose |
| --- | --- | --- |
| Language | Rust 2024 edition, Rust 1.90+ | Scanner, API, source adapters, news worker, paper CLI |
| Async runtime | Tokio | Concurrent HTTP collection and service execution |
| HTTP client | Reqwest with rustls | Polymarket, source adapters, and Brave Search |
| Local API | Axum | Dashboard and JSON endpoints |
| Serialization | Serde and serde_json | Upstream normalization and persisted records |
| Numeric model | rust_decimal | Odds, probabilities, fees, VWAP, and risk without floats |
| Time and IDs | Chrono and UUID | UTC freshness checks and stable market-side IDs |
| Observability | tracing | Structured scanner and Lambda logs |
| CLI | Clap | Scanner, source probe, API, and paper position commands |

AWS SDK crates are feature-gated behind the `aws` Cargo feature. Browser
collection support is feature-gated behind `browser`; it is not enabled for
unapproved sportsbook automation.

## Run Modes

`config::RunMode` (`RUN_MODE=local|cloud`) is read once and threaded through
`storage::store_for`, so every binary opens the right store:

| Binary | local | cloud |
| --- | --- | --- |
| `local` | scan loop + news loop + retention + dashboard in one Tokio runtime | n/a |
| `scanner` | one-shot or `--continuous` against the file store | one-shot ECS task against S3/DynamoDB/SQS |
| `api` | axum on `LISTEN_ADDRESS` | Lambda behind API Gateway |
| `news-worker` | n/a (the `local` news loop drains `news-queue.ndjson`) | SQS-triggered Lambda |
| `paper`, `source-probe` | file store | AWS store |

`news::NewsReviewer` abstracts the review step: `KeywordReviewer` (Brave +
deterministic risk terms), `BedrockNewsEnricher` (`aws` feature), or
`DisabledReviewer`. `Store::take_news_queue` and `Store::prune` are the two
local-only operations; the AWS store's defaults are no-ops because SQS and S3
lifecycle rules own those concerns.

Local performance choices, all measured against live endpoints: Polymarket
discovery runs the five sports concurrently with gzip and is cached for
`DISCOVERY_REFRESH_SECONDS`; books are fetched through a semaphore
(`POLYMARKET_CONCURRENCY`) instead of a serial throttle; each odds adapter
collects its sports concurrently. Steady-state scans complete in roughly half
a second.

## AWS Architecture

### ECS Fargate

A one-shot scanner task is launched by EventBridge Scheduler every five
minutes. Fargate is used because the scan performs concurrent, variable-length
network collection and may later need heavier source adapters.

The task:

1. Acquires a DynamoDB lease.
2. Discovers supported Polymarket US events.
3. Collects the continuous source families concurrently (ESPN, Kalshi,
   Polymarket global, and any approved direct adapters).
4. Builds preliminary consensus and opportunities.
5. When a candidate survives, refetches up to three contributing continuous
   sources plus every confirmation-tier source for the candidate sports.
6. Fetches current Polymarket books and performs depth-aware sizing.
7. Writes scans and recommendations.
8. Queues news candidates.
9. Releases the lease and exits.

### Lambda

Two Rust Lambda functions handle event-driven work:

- `api`: serves health and authenticated opportunity data.
- `news-worker`: consumes SQS messages, calls Brave Search and Bedrock, and
  stores cited news evidence.

The news worker returns partial batch failures so one bad record does not
replay an entire successful SQS batch.

### API and Dashboard

- API Gateway HTTP API routes `/api/opportunities` to the API Lambda.
- A Cognito JWT authorizer protects opportunity data.
- Cognito uses authorization code flow with PKCE.
- A private S3 bucket stores `index.html` and generated `config.js`.
- CloudFront uses Origin Access Control for S3 and routes `/api/*` to API
  Gateway.

The local development API serves the same dashboard without authentication on
`127.0.0.1:8080`.

## Data Services

### Amazon S3

S3 stores immutable scan and quote snapshots under date partitions:

```text
scans/year=YYYY/month=MM/day=DD/<scan-id>.json
scans/year=YYYY/month=MM/day=DD/<scan-id>-quotes.json
```

The bucket uses versioning, server-side encryption, blocked public access, and
a lifecycle transition.

### Amazon DynamoDB

The state table uses `pk` and `sk` string keys. Main records include:

| Partition key | Sort key | Content |
| --- | --- | --- |
| `LATEST` | `OPPORTUNITIES` | Latest recommendation collection |
| `OPPORTUNITY#<uuid>` | timestamp | Historical opportunity |
| `OPPORTUNITY#<uuid>` | `NEWS` | Brave/Bedrock evidence |
| `PORTFOLIO` | `PAPER` | Paper bankroll and open positions |
| `LOCK#SCANNER` | `LEASE` | Scanner owner and expiry |

The lease has a DynamoDB TTL attribute and a conditional write. Paper exposure
is recalculated from persisted positions when loaded.

### Amazon SQS

The news queue decouples scanning from web search and Bedrock latency. A DLQ
receives records after three failed receives. Stable market-side opportunity
IDs and a one-hour evidence cache prevent duplicate five-minute search calls.

### Secrets Manager

The application secret stores the Brave Search key. The news Lambda reads it
at runtime. The Odds API key, when used, is supplied to the scanner task as
`THE_ODDS_API_KEY`.

## External APIs

### Polymarket US

Base URL:

```text
https://gateway.polymarket.us
```

The client uses structured league/sport discovery endpoints and the US market
book endpoint. No wallet, CLOB signing, Polygon, or non-US order code is
included.

The US book has one YES instrument:

- Long execution consumes YES offers.
- Opposing-outcome execution consumes YES bids at a side cost of
  `1 - YES bid`.

### Odds Adapters

Every adapter implements `OddsSource` in `src/sources/` and emits
`SourceQuote` values scoped to the requested sports. Three tiers exist:

| Tier | Adapters | Role |
| --- | --- | --- |
| Continuous | `EspnOddsSource`, `KalshiSource`, `PolymarketGlobalSource`, `CanonicalJsonSource` | Collected every scan for all sports; feed the preliminary consensus |
| Confirmation | `TheOddsApiSource` | Collected only for sports with a live candidate; bookmaker keys map onto owning families; stops at a credit floor |
| Validation | none built in | `validation_only` quotes never enter consensus |

Public sources use one shared `reqwest` client with an identifying user agent
that carries a contact URL (`sources::USER_AGENT`); ESPN rejects bare
`name/version` agents.

Direct adapters (`SOURCE_<BOOK>_URL`, canonical JSON in `README.md`) must
preserve:

- Source event and participant identifiers
- Provider IDs when available
- Decimal moneyline odds
- Neutral outcome odds when applicable
- Event start time and, if only a date is known, a start-time tolerance
- Source quote timestamp
- Fetch timestamp
- Parser version

The scanner requires `MINIMUM_CONFIGURED_SOURCES` (default 3) distinct
continuous families at startup and fails otherwise. A reference book and the
five-family quorum are enforced per opportunity at runtime.

### Brave Search and Bedrock

Brave Search retrieves recent injury, lineup, suspension, withdrawal, weather,
and schedule evidence. Bedrock runs an Anthropic-compatible request that must
return strict JSON:

```json
{
  "summary": "Evidence summary with citation markers",
  "confidence_effect": "unchanged",
  "manual_review": false
}
```

Allowed effects are `unchanged`, `lower`, `reject`, and `review`. Evidence
older than two hours cannot preserve an actionable classification.

## Domain Logic

### Matching

- Match sport and start time within 15 minutes, or within the quote's own
  tolerance when the source publishes only a date (Kalshi NFL/NBA).
- Prefer shared provider IDs.
- Reject comparable conflicting provider IDs.
- Fall back to normalized two-participant names using the market's canonical
  `team.name`; accept a city or short-form prefix only when both participants
  resolve to distinct sides.
- Preserve tennis doubles separators.

### Consensus

- Reject quotes not observed within the freshness window (`fetched_at`),
  future-dated quotes, and validation-only quotes.
- Keep the most recently observed quote per source family.
- Remove vig proportionally, including neutral settlement outcomes.
- Use median fair probability and median absolute deviation.
- Remove outliers beyond the configured robust limit.

### Risk and Execution

- Use Decimal for every monetary and probability calculation.
- Walk multiple order-book levels to a maximum side price.
- Apply fee coefficients and banker's rounding.
- Recompute edge from execution VWAP.
- Size with quarter Kelly.
- Enforce minimum quantity, 1-5% position risk, and $5 total exposure.

## Infrastructure as Code

Terraform under `infra/` provisions:

- VPC, public subnets, route table, and outbound-only scanner security group
- ECR repository and lifecycle policy
- ECS cluster, task definition, IAM, logs, and five-minute schedule
- S3 data and dashboard buckets
- DynamoDB state table
- SQS news queue and DLQ
- Secrets Manager secret
- API and news Lambda functions
- API Gateway, Cognito, CloudFront, and S3 Origin Access Control
- CloudWatch alarms for API, news, scanner, scheduler, and DLQ failures

The scanner image tag is immutable and normally uses a Git commit SHA. Lambda
archives are built with `cargo-lambda` for `arm64`.

## Development and Quality

Primary commands:

```bash
make check
make test
make api
make probe
make terraform-check
make lambda
```

Quality gates include:

- `cargo fmt --check`
- Clippy across all targets and features with warnings denied
- Unit and fixture tests across all features
- Terraform formatting and validation
- HTTP verification of local dashboard endpoints

Fixtures cover Polymarket event and book payloads, canonical source quotes,
YES/opposing-outcome conversion, fee rounding, multi-level sizing, matching,
quorum filtering, stale data, news gating, leases, and portfolio invariants.

## Deliberate Exclusions

- Live Polymarket authentication and order submission
- Paid odds dependencies
- Private sportsbook endpoint discovery
- CAPTCHA, geolocation, or authentication bypass
- Supabase, Polygon wallet, and non-US CLOB assumptions from the reviewed
  reference bot
