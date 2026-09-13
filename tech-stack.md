# Tech Stack

## Runtime

| Area | Technology | Purpose |
| --- | --- | --- |
| Language | Rust 2024 edition, Rust 1.90+ | Scanner, terminal UI, source adapters, news worker, paper CLI |
| Async runtime | Tokio | Concurrent HTTP collection and service execution |
| HTTP client | Reqwest with rustls | Polymarket, source adapters, and Exa search |
| Terminal UI | ratatui + crossterm | Operator views over the local or cloud store |
| Serialization | Serde and serde_json | Upstream normalization and persisted records |
| Numeric model | rust_decimal | Odds, probabilities, fees, VWAP, and risk without floats |
| Time and IDs | Chrono and UUID | UTC freshness checks and stable market-side IDs |
| Observability | tracing | Structured scanner and Lambda logs |
| CLI | Clap | Scanner, source probe, terminal UI, paper position, history and backtest commands |
| History archive (optional) | Postgres via sqlx | Durable scan inputs, outcome grading for every market seen, replayable backtests |

AWS SDK crates are feature-gated behind the `aws` Cargo feature. Browser
collection support is feature-gated behind `browser`; it is not enabled for
unapproved sportsbook automation.

### Terminal UI

`tui` renders with ratatui on a crossterm backend. The render loop draws on
change, not on a fixed frame rate: crossterm's `EventStream`, the store
snapshot channel, the engine status channel and a 1 s clock (scan age and
countdown) are one `tokio::select!`, and a frame is redrawn only when one of
them fires. The store is polled off the render loop every 2 s in local mode
and every 10 s in cloud mode into an immutable snapshot. While the TUI owns the
terminal, `tracing` output is captured into a 500-line in-memory ring buffer
that feeds the LOG panels instead of being written to stderr. Views: Overview,
Markets, Sources, Portfolio, System (`Tab` / `1`-`5`); `o` on Overview or
Markets and `c` on Portfolio reuse the `paper open`/`close` rules.

## Run Modes

`config::RunMode` (`RUN_MODE=local|cloud`) is read once and threaded through
`storage::store_for`, so every binary opens the right store:

| Binary | local | cloud |
| --- | --- | --- |
| `local` | scan loop + news loop + settlement loop + retention in one Tokio runtime; opens the TUI in-process when stdin/stdout are a TTY, `--headless` for systemd | n/a |
| `scanner` | one-shot or `--continuous` against the file store | one-shot ECS task against S3/DynamoDB/SQS |
| `tui` | attach-only viewer over `data/` | attach-only viewer over DynamoDB/S3 with the operator's AWS credentials |
| `paper`, `source-probe` | file store | AWS store (`paper settle` is how cloud-mode positions get closing lines and settlement; the ECS task does not run the settlement loop) |

`news::NewsReviewer` abstracts the review step: `KeywordReviewer` (Exa +
deterministic risk terms), `BedrockNewsEnricher` (`aws` feature), or
`DisabledReviewer`. `Store::take_news_queue` and `Store::prune` are the two
local-only operations; the AWS store's defaults are no-ops because SQS and S3
lifecycle rules own those concerns.

### Daemon liveness

`health::HealthSnapshot` is built from the TUI's `EngineStatus` channel and
written to `<data-dir>/health.json` by a publisher task in `local` on every
status change (`storage::write_json_atomic`, shared with `latest-*.json`).
`health::systemd` wraps `sd-notify` (Linux only; no-op elsewhere or without
`NOTIFY_SOCKET`): `READY=1` after the loops spawn, `WATCHDOG=1` at the end of
every scan attempt in `run_scan`, `STATUS=` with the health line, `STOPPING=1`
on shutdown. `deploy/polybot-local.service` is `Type=notify`,
`WatchdogSec=900` (three scan intervals), `Restart=always`, and runs the
immutable `target/deploy/current/local` produced by `make deploy`. `local`
handles SIGTERM like Ctrl-C so systemd stops drain the current step.
`build.rs` embeds the short git commit as `POLYBOT_GIT_COMMIT` for the health
file.

### History archive (`postgres` feature)

`storage::store_for(settings, data_dir, Archive)` wraps the mode store in
`ArchivingStore` when `DATABASE_URL` is set and the binary writes scans
(`local`, `scanner`; viewers pass `Archive::Disabled`). The wrapper forwards
every `Store` call and additionally records `save_scan` and `save_news` into
Postgres through `history::History`. An archive failure fails the scan: the
operator asked for the archive, and a silent gap would corrupt every backtest
over the window. Without `DATABASE_URL` nothing is opened; with it and no
`postgres` feature, startup fails with a clear config error.

Stack: `sqlx 0.8` (postgres, rustls, `rust_decimal` → `NUMERIC`, `uuid`,
`chrono`, `json`), one `PgPool` of four connections, migrations as plain SQL
in `migrations/` embedded with `sqlx::migrate!` and applied on connect. Tables
(`migrations/202609130001_history.sql`): `scans` (clock, gates, portfolio,
source health, origin `live|import`), `markets` (slowly changing, latest
definition as JSONB), `scan_books` (bids/offers per scan and market, with the
market fields that vary between scans), `scan_quotes` (raw quote JSONB plus
filter columns), `opportunities` (every side, flattened), `news_evidence`,
`market_outcomes` (closing line, settlement, attempts, backoff), and
`backtest_runs`. Every row keeps observation time (`fetched_at`,
`evaluated_at`) apart from event time (`start_time`, `source_timestamp`).

Determinism is what makes the archive replayable: `consensus::build_consensus`,
`OpportunityEngine::evaluate`, and `ResearchOpportunity::at` take `now`
explicitly, the scanner evaluates a whole pass at one instant recorded as
`ScanSnapshot::evaluated_at`, and `scanner::consensus_markets` /
`scanner::evaluate_books` are the shared pure functions both the live pass and
`replay::Replay` call. `backtest --audit` recomputes every live frame under
its archived gates and portfolio and diffs it against the stored rows; the
mismatch count is the regression guard for that seam.

`History::grade_outcomes` (engine settlement tick and `history grade`) walks
markets past start without a settlement, inside a 14-day window, in batches
of `GRADING_BATCH` with a 10 min → 1 h → 6 h backoff per market, fetching the
closing line once and the settlement until published. `History::reader()`
opens a `REPEATABLE READ READ ONLY` transaction so a replay sees one snapshot
of frames and outcomes. `replay::Replay` steps frames in order, settles
positions `settlement_lag_hours` after start when an outcome exists, and never
fills an order against the book it was decided on under `FillModel::NextBook`.
`metrics` holds the pure Brier / reliability / summary / drawdown arithmetic.

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
3. Collects the continuous source families concurrently (Pinnacle, Action
   Network book lines, ESPN, Kalshi, Polymarket global, Smarkets, and any
   approved direct adapters).
4. Builds preliminary consensus and opportunities.
5. When a candidate survives, refetches up to three contributing continuous
   sources plus every confirmation-tier source for the candidate sports.
6. Fetches current Polymarket books and performs depth-aware sizing.
7. Writes scans and recommendations.
8. Queues news candidates.
9. Releases the lease and exits.

### Lambda

One Rust Lambda function, `news-worker`, handles the event-driven work: it
consumes SQS messages, calls Exa search and Bedrock (`BedrockNewsEnricher`),
and stores cited news evidence. It reads `BEDROCK_MODEL_ID` and, unless
`EXA_API_KEY` is set directly, the `exa_api_key` field of the Secrets Manager
secret named by `APP_SECRET_ID`. Records with evidence newer than
`NEWS_REFRESH_SECONDS` are skipped without a search.

The news worker returns partial batch failures so one bad record does not
replay an entire successful SQS batch.

There is no hosted UI. Operators run `tui` with `RUN_MODE=cloud` against the
data services below.

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
| `OPPORTUNITY#<uuid>` | `NEWS` | Exa/Bedrock evidence |
| `PORTFOLIO` | `PAPER` | Paper bankroll and open positions |
| `LOCK#SCANNER` | `LEASE` | Scanner owner and expiry |

The lease has a DynamoDB TTL attribute and a conditional write. Paper exposure
is recalculated from persisted positions when loaded.

### Amazon SQS

The news queue decouples scanning from web search and Bedrock latency. A DLQ
receives records after three failed receives. Stable market-side opportunity
IDs and a one-hour evidence cache prevent duplicate five-minute search calls.

### Secrets Manager

The application secret is a JSON object. The news Lambda reads `exa_api_key`
at runtime; the ECS task receives `the_odds_api_key` as `THE_ODDS_API_KEY`
when `enable_the_odds_api` is set. Terraform creates the secret empty;
`README.md` shows the `put-secret-value` call that populates it.

## External APIs

### Polymarket US

Base URL:

```text
https://gateway.polymarket.us
```

`polymarket::PolymarketUsClient` uses the structured league/sport discovery
endpoints, `GET /v1/markets/{slug}/book` for depth, and two public endpoints
that grade paper positions after the fact: `GET /v1/markets/{slug}/settlement`
(404 until the market resolves) and `GET /v1/price-history?symbol={slug}&
fixedInterval=INTERVAL_LIVE&fidelity=1`, whose series starts 15 minutes before
the event; the last point at or before scheduled start is the closing line.
Every request takes a concurrency permit (`POLYMARKET_CONCURRENCY`) and then a
slot from a shared pacer (`POLYMARKET_REQUESTS_PER_SECOND`, default 18 against
the documented 20 req/s/IP public limit); a 429 pushes the pacer out one
second and retries once. No wallet, CLOB signing, Polygon, or non-US order code
is included.

Fee schedule (effective 2026-07-01): taker `0.06 * C * p * (1 - p)` per fill,
banker's-rounded to cents, order total capped at the rounding of the cumulative
exact fee; makers pay nothing and receive `0.0125 * C * p * (1 - p)`. The
market's `feeCoefficient` is read per market; the maker rebate coefficient is
`MAKER_REBATE_COEFFICIENT`.

The US book has one YES instrument:

- Long execution consumes YES offers.
- Opposing-outcome execution consumes YES bids at a side cost of
  `1 - YES bid`.

Settlement rules that matter for a moneyline bot: NFL ties pay $0.50;
pre-start withdrawal, postponement past expiry, cancellation and no-contest
settle at *last fair market price* (ITF tennis at $0.50), where sportsbooks
void. Those phrases are hard `review` terms for the keyword reviewer.

### Odds Adapters

Every adapter implements `OddsSource` in `src/sources/` and emits
`SourceQuote` values scoped to the requested sports. Three tiers exist:

| Tier | Adapters | Role |
| --- | --- | --- |
| Continuous | `PinnacleSource` (reference), `ActionNetworkSource` (multi-book; `families()` lists every family it maps), `EspnOddsSource`, `KalshiSource`, `PolymarketGlobalSource`, `SmarketsSource`, `CanonicalJsonSource` | Collected every scan for all sports; feed the preliminary consensus |
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

### Exa Search and Bedrock

`news::ExaSearchClient` posts to `https://api.exa.ai/search` with the key in
`x-api-key` and the request timeout of the calling process (the scanner's
`request_timeout`, 15 s in the Lambda). Per candidate it sends one request:

```json
{
  "query": "<participant> <market slug> <sport> injury lineup suspension withdrawal weather schedule latest",
  "numResults": 8,
  "moderation": true,
  "contents": { "highlights": true }
}
```

Each result becomes a `NewsCitation` (title, URL, published time, snippet).
The snippet is the joined highlights, which is what gives the keyword
classifier and Bedrock roster and injury text to work with. No
`startPublishedDate` or `category` filter is sent: Exa excludes undated pages
under a date filter, and game-day injury tables (ESPN, FOX, StatMuse, CBS)
carry no published date; the dated market slug keeps results on the right
game. `publishedDate` is parsed as RFC 3339 or `YYYY-MM-DD` because the docs
and responses disagree. Exa replaced Brave Search when Brave dropped its free
API plan; the client is the only place the provider is named, so swapping it
again means one struct.

`KeywordReviewer` classifies the citations deterministically (participant
must be named; hard terms force `review`, soft terms `lower`, no citations
`review`). `BedrockNewsEnricher` instead sends the numbered citations to an
Anthropic-compatible Bedrock request (`temperature` 0, 500 tokens) that must
return strict JSON:

```json
{
  "summary": "Evidence summary with citation markers",
  "confidence_effect": "unchanged",
  "manual_review": false
}
```

Allowed effects are `unchanged`, `lower`, `reject`, and `review`; any other
value is an error and the SQS record is retried. Zero citations short-circuit
to `review` with manual review before the model is called. Evidence older than
two hours cannot preserve an actionable classification.

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
  future-dated quotes, and validation-only quotes. On the confirmation pass
  only refetched sources are held to `CONFIRMATION_MAX_AGE_SECONDS`; the rest
  keep the preliminary `MAX_QUOTE_AGE_SECONDS`.
- Kalshi and Polymarket global quotes are excluded for games starting more
  than `EXCHANGE_MAX_LEAD_HOURS` out.
- Keep the most recently observed quote per source family.
- Remove vig proportionally, including neutral settlement outcomes.
- Use median fair probability and median absolute deviation.
- Remove outliers beyond the configured robust limit.
- A market with matched quotes but no consensus (invalid odds, all outliers)
  is logged at `warn`; unmatched markets at `debug`.

### Risk and Execution

- Use Decimal for every monetary and probability calculation.
- `conservative = fair - MAD - CONSENSUS_BIAS`; raw edge uses `fair`, net edge
  and sizing use `conservative`.
- Reject rows inside `MINIMUM_LEAD_MINUTES` of start.
- Walk multiple order-book levels to a maximum side price.
- Apply the venue's fee arithmetic per fill and cap the order total.
- Recompute edge from execution VWAP; size with `KELLY_FRACTION` Kelly at top
  of book, then once more at the VWAP cost and keep the smaller.
- Report `maker_net_edge` (rebate instead of fee, one tick inside the spread)
  without letting it classify.
- Enforce minimum quantity, 1-5% position risk, `MAXIMUM_EVENT_EXPOSURE` per
  event, and $5 total exposure.

### Paper Ledger

`paper::settle_positions` runs from the `local` engine loop every
`SETTLEMENT_POLL_SECONDS` and from `paper settle`. For each open position past
its start time it records the closing side price once, then, when the venue
publishes a settlement, books `quantity * (payout - entry) - fee`, archives the
position under `closed_positions`, and sets `bankroll = PAPER_BANKROLL +
realized`. `Store::load_portfolio` recomputes realized P&L and bankroll from
the closed positions on every load, so the JSON file (or DynamoDB item) is
never the source of truth for derived totals.

## Infrastructure as Code

Terraform under `infra/` provisions:

- VPC, public subnets, route table, and outbound-only scanner security group
- ECR repository and lifecycle policy
- ECS cluster, task definition, IAM, logs, and five-minute schedule
- S3 data bucket
- DynamoDB state table
- SQS news queue and DLQ
- Secrets Manager secret
- News Lambda function
- CloudWatch alarms for news, scanner, scheduler, and DLQ failures

The scanner image tag is immutable and normally uses a Git commit SHA. Lambda
archives are built with `cargo-lambda` for `arm64`.

## Development and Quality

Primary commands:

```bash
make check
make test
make tui
make probe
make terraform-check
make lambda
```

Quality gates include:

- `cargo fmt --check`
- Clippy across all targets and features with warnings denied
- Unit and fixture tests across all features
- Terraform formatting and validation

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
