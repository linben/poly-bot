# Polybot

Rust research and paper-risk tooling for pregame sports markets on the
Polymarket US API. It scans NFL, NBA, WNBA, MLB, and tennis moneylines, compares
the executable US order book with a de-vigged consensus built from free public
odds sources (ESPN/DraftKings, Kalshi, Polymarket global) and an optional
quota-limited multi-book confirmation feed, and enriches candidates with recent
Brave Search evidence summarized by AWS Bedrock.

This repository does not contain wallet credentials, exchange authentication,
or order-submission code. It cannot place a live trade.

Project direction is defined in [vision.md](vision.md). The implemented
architecture and technology choices are documented in
[tech-stack.md](tech-stack.md).

## Decision Policy

- Polymarket API: `https://gateway.polymarket.us`
- Markets: structured pregame full-game/match moneylines only
- Contract acquisition price: `$0.35` through `$0.65`
- Paper bankroll: `$100`
- Position risk: quarter Kelly, bounded to 1-5% of bankroll
- Watchlist: at least three independent source families
- Actionable consensus: at least five independent families, including a
  reference book
- Edge: at least five percentage points raw and three percentage points after
  dispersion and fees
- Confirmation: when any candidate survives the continuous pass, refetch up to
  three contributing continuous sources plus every confirmation-tier source
  (The Odds API) for the candidate sports, replace their initial quotes, and
  fail closed if the quorum no longer holds
- Freshness: a quote is fresh by when it was last observed, not by when the
  book last moved the line; an unmoved line is still a live price
- News gate: a candidate remains watchlist until Bedrock returns corroborated
  `unchanged` evidence; `lower`, `review`, and `reject` downgrade it
- News cache: stable market-side IDs reuse evidence for one hour; evidence
  older than two hours cannot preserve an actionable class

Polymarket US exposes one YES book. The engine interprets:

- Long cost as the YES offer
- Opposing-outcome cost as `1 - YES bid`

Sizing walks all eligible book levels, calculates a side-specific VWAP, and
applies fee rounding at each consumed level.

## Run Modes

`RUN_MODE` selects where state lives and which services run. Both modes share
the same scanner, consensus, sizing, and news logic.

| | `local` (default) | `cloud` |
| --- | --- | --- |
| Process | one `local` binary: scan loop, news loop, retention, terminal UI | ECS one-shot `scanner`, Lambda `news-worker` |
| Store | files under `data/` (`scans/`, `latest-opportunities.json`, `news/`, `portfolio.json`, `scanner.lock`) | S3 + DynamoDB + SQS |
| News reviewer | `keyword` (Brave + risk-term classifier), `bedrock`, `off`, or none | Bedrock via SQS |
| Dashboard | terminal UI in-process, or `tui` attached to `data/` | `tui` with `RUN_MODE=cloud` and AWS credentials |
| Requires | Rust toolchain, outbound HTTPS | AWS account, Terraform, Docker, cargo-lambda |

### Local mode

```bash
cp .env.example .env            # optional: add THE_ODDS_API_KEY / BRAVE_SEARCH_API_KEY
set -a; source .env; set +a
make local                      # or: cargo run --release --bin local
make local-once                 # one scan + one news pass, then exit
make local-headless             # loops only, no terminal UI
make tui                        # attach a viewer to data/ (or RUN_MODE=cloud)
```

`local` runs a fixed-cadence scan (`SCAN_INTERVAL_SECONDS`, default 300),
drains the news queue every 20 s, prunes scan snapshots older than
`LOCAL_RETENTION_DAYS` (default 14), and, when stdin and stdout are a TTY,
opens the terminal UI in the same process; `q` quits the UI and stops the
loops. `--headless` skips the UI, which is what `deploy/polybot-local.service`
(a user systemd unit for unattended operation) runs. A failed scan is logged
and retried next tick; only configuration errors exit. Without the UI, logs go
to stderr and JSON to stdout.

Measured on 2026-09-12 against live APIs (release build): a cold scan takes
about 3.3 s, of which market discovery is 2.2-2.5 s of Polymarket server time
(an NFL events page is 42 MB uncompressed, 1.4 MB gzip); discovery is cached
for `DISCOVERY_REFRESH_SECONDS` (default 900), so steady-state scans finish
in about 0.5 s: three sources collected concurrently (~0.3 s) and all ~90
matched books fetched in parallel (`POLYMARKET_CONCURRENCY`, default 8;
measured ~20 ms per book with no throttling at 32 concurrent). Before this
work a scan took ~24 s, dominated by a serial 110 ms per-request throttle.
Started games are dropped at evaluation time so the discovery cache cannot
leak an in-play book.

### Terminal UI

The UI is the operator surface in both modes. `local` opens it in-process when
run from a TTY, so it also shows live engine state (scan phase, countdown,
failures) and the captured log. `tui` is an attach-only viewer over the
configured store: with `RUN_MODE=local` it reads `data/` (`DATA_DIR`), with
`RUN_MODE=cloud` it reads DynamoDB and S3 using the operator's AWS
credentials. There is no hosted web UI.

| View | Shows | Keys |
| --- | --- | --- |
| Overview | status line (mode, phase, scan age and duration, counts, next scan); SCAN, SOURCES, EDGE & PAPER panels; CANDIDATES (live candidates first, then closest misses); LOG tail | `o` paper-open the top candidate, `r` rescan now (in-process) or reload (attached) |
| Markets | every evaluated market side, with a DETAIL pane for the selected row (VWAP, maker price, size, fee, sources, news, every rejection reason) | `↑`/`↓` `j`/`k` `PgUp`/`PgDn` `g`/`G`, `s` sort (net edge / raw edge / families / class / sport), `f` cycle sport filter, `⏎` toggle detail, `o` paper-open selected |
| Sources | reachability, quotes, latency per source; markets matched per source and sport | `r` rescan |
| Portfolio | bankroll, exposure gauge, headroom; open paper positions with their current class | `↑`/`↓`, `c` close selected |
| System | every threshold and cadence in force; scrollable log | `↑`/`↓`, `g`/`G` |

`Tab`/`Shift-Tab` or `1`-`5` switch views; `q` quits (and stops the loops in
`local`). Paper actions use the same rules as `paper open`/`close`: open
requires an actionable, news-reviewed row from the latest scan and respects the
$5 exposure cap.

Design, from reviewing the `vulcan` bot TUI in the sibling repository and the
ratatui ecosystem: ratatui 0.30 + crossterm 0.29 with the crossterm
`EventStream`, so keys, store updates, engine status and a 1 s clock are a
single `tokio::select!` and the screen is redrawn only when one of them fires;
the store is polled off the render loop (every 2 s for files, 10 s for
DynamoDB) into an immutable snapshot with precomputed edge statistics; while
the UI owns the terminal, `tracing` events go to a 500-line in-memory ring
buffer instead of stderr. One palette (`Tone`) maps semantic state to colour so
every panel reads the same way: green actionable / reachable / positive edge,
yellow watchlist / lower / review, red failures and `reject`, grey rejected.

### Cloud mode

The five-minute scanner runs as a scheduled one-shot ECS Fargate task. It can
perform concurrent network collection without Lambda's packaging and duration
constraints. A DynamoDB lease prevents overlapping scheduled tasks.

Lambda is used only for the short, event-driven news work: SQS-triggered
Brave Search and Bedrock enrichment.

S3 stores raw scans and quote snapshots. DynamoDB stores current
recommendations, news evidence, paper portfolio state, and the scanner lease.
Operators inspect the cloud store with `RUN_MODE=cloud cargo run --features aws
--bin tui` under their own AWS credentials.

### News reviewers

`NEWS_REVIEWER` picks how candidates are vetted. Every reviewer can only
preserve or downgrade a candidate.

- `keyword` (default when `BRAVE_SEARCH_API_KEY` is set): Brave Search, then a
  deterministic classifier. A citation counts only if it names the participant;
  hard terms (ruled out, scratched, suspended, withdrawn, postponed, ...) force
  `review` with manual review, soft terms (questionable, doubtful, injury,
  weather delay) give `lower`, otherwise `unchanged`. No citations at all is
  `review`.
- `bedrock`: Brave Search summarized by Bedrock (build with `--features aws`).
- none (default without a Brave key): no evidence is written, so nothing can
  leave the watchlist.
- `off`: records `unchanged` without searching. This removes the news veto and
  is only for operators who review every candidate by hand.

## Odds Sources

Three continuous sources run every scan. They are public, unauthenticated, and
unmetered, and each is one independent family:

| Source | Family | Sports | Notes |
| --- | --- | --- | --- |
| ESPN core odds | `draft_kings` (per provider ESPN serves) | NFL, NBA, WNBA, MLB | Current-week scoreboard only; American odds converted to decimal |
| Kalshi public market API | `kalshi` | NFL, NBA, WNBA, MLB | `1 / yes ask` per side; skipped when spread > 6c; NFL/NBA rules carry only a date, so the quote uses a 14-hour start tolerance |
| Polymarket global Gamma | `polymarket_global` | NFL, NBA, WNBA, MLB, tennis | `1 / ask` and `1 / (1 - bid)`; liquidity floor `POLYMARKET_GLOBAL_MIN_LIQUIDITY`, traded-volume floor `POLYMARKET_GLOBAL_MIN_VOLUME` (seeded, never-traded books sit at 50/50), spread <= 4c |

Three families reach the watchlist quorum, never the actionable one. The Odds
API (`ENABLE_THE_ODDS_API=true` plus `THE_ODDS_API_KEY`) is the confirmation
tier: it is only queried for sports that already have a candidate, one
request per sport returns every US and EU book (Pinnacle, BetOnline, FanDuel,
BetMGM, Caesars, ...), each mapped onto its owning family so skins are counted
once, and it stops spending below `THE_ODDS_API_MIN_REMAINING` credits. On the
free plan (500 credits/month, `us,eu` = 2 credits per sport) that is roughly
four confirmations a day, which is plenty because candidates are rare.

`config/sources.json` still defines the direct book catalog. Set
`SOURCE_<BOOK>_URL` to an approved adapter emitting the canonical JSON below
and it joins the continuous pass as its own family. Every direct book must
pass a feasibility and terms review before its endpoint is configured.
Collection must not bypass authentication, CAPTCHA, geolocation, access
controls, or other restrictions.

```json
{
  "quotes": [{
    "source_id": "ignored-and-overwritten",
    "family": "pinnacle",
    "sport": "nba",
    "event_id": "provider-event-id",
    "participant_a": "Los Angeles Lakers",
    "participant_b": "Boston Celtics",
    "participant_a_provider_ids": {"sportradar": "sr:team:1"},
    "participant_b_provider_ids": {"sportradar": "sr:team:2"},
    "start_time": "2026-08-07T02:00:00Z",
    "start_time_tolerance_minutes": 15,
    "decimal_odds_a": "1.90",
    "decimal_odds_b": "2.00",
    "decimal_odds_neutral": null,
    "source_timestamp": "2026-08-07T01:59:30Z",
    "fetched_at": "2026-08-07T01:59:31Z",
    "parser_version": "adapter-v1",
    "validation_only": false
  }]
}
```

Event matching prefers shared provider IDs (Polymarket US publishes Sportradar
and SportsDataIO team IDs). Name matching uses the canonical team name from the
market side's `team.name`, and accepts a source's city or short form
("Kansas City", "Los Angeles R") only when both participants resolve to
distinct sides; "Chicago" against Cubs and White Sox is rejected.

### What the data says

A live scan on 2026-09-12 (84 NFL and MLB moneylines) found Polymarket US
books with a half-cent spread and six-figure contract depth at the top three
levels. Against DraftKings the US midpoint differed by 0.07 pp on average
with a 0.74 pp standard deviation and a 2.2 pp maximum; against Kalshi the
standard deviation was 0.97 pp. A raw edge of five points is therefore rare
and, when it appears from one family only, has so far been a stale or
placeholder quote (a fresh global market seeded at 0.50/0.51 while the US
book sat at 0.72). The quorum exists to reject exactly that. Real candidates
should come from venues disagreeing after news (a starting pitcher change
moved Kalshi and Polymarket global four points before the US book followed),
so scans are most useful in the hours before first pitch and around NFL
inactive reports.

The reviewed `polymm` repository contributed useful concepts around de-vigging,
limit pricing, matching, and adverse selection. Its Polygon wallet/order code,
Supabase persistence, and non-US CLOB assumptions are intentionally not used.

The supplied PredictEngine article is marketing material, not performance
evidence. Its stated backtest results, user counts, bonuses, and claims of
"free money" are not used by this system.

## Local Development

Prerequisites:

- Rust 1.90 or newer
- For cloud mode only: Terraform 1.8+, Docker, `cargo-lambda`

```bash
cp .env.example .env
set -a; source .env; set +a
cargo test --all-features
cargo run --bin source-probe -- --adapters-only
make local-once
```

`make tui` attaches the terminal UI to `data/`; `make local` opens it
in-process. Scanner output is written under `data/`. The scanner uses
persisted `portfolio.json`
positions for exposure checks; it does not silently add a
position before news review. Use the explicit paper commands after reviewing a
candidate:

```bash
cargo run --bin paper -- list
cargo run --bin paper -- open <opportunity-uuid>
cargo run --bin paper -- close <opportunity-uuid>
```

`cargo run --bin scanner` performs one scan and exits (the cloud task shape;
`--continuous` loops). All binaries start with the three built-in public
sources and fail at startup with fewer than three distinct continuous
families (`MINIMUM_CONFIGURED_SOURCES`). Without a confirmation-tier source or
a reference book they log a warning: results can reach the watchlist but never
become actionable.

`source-probe` runs a real collection through every configured adapter and
reports quote counts and latency; with `--adapters-only` it skips the catalog
homepage reachability checks. Logs go to stderr, JSON results to stdout.

## AWS Deployment (cloud mode)

1. Populate the Terraform variables and create the managed ECR repository.
2. Build and push the scanner image using the immutable configured tag.
3. Build the `news-worker` Lambda archive.
4. Apply the complete Terraform stack.
5. Put the Brave free-plan key in the generated secret.

```bash
cp infra/terraform.tfvars.example infra/terraform.tfvars
terraform -chdir=infra init
terraform -chdir=infra apply -target=aws_ecr_repository.scanner

REPOSITORY="$(terraform -chdir=infra output -raw scanner_repository_url)"
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin "${REPOSITORY%%/*}"
docker build --platform linux/arm64 -t "$REPOSITORY:sha-REPLACE" .
docker push "$REPOSITORY:sha-REPLACE"

cargo lambda build --release --arm64 --features aws --bin news-worker
terraform -chdir=infra apply

aws secretsmanager put-secret-value \
  --secret-id "$(terraform -chdir=infra output -raw application_secret_id)" \
  --secret-string '{"brave_search_api_key":"replace-me","the_odds_api_key":"optional"}'
```

The Lambda zip path defaults to cargo-lambda's output under `target/lambda/`.
The image tag in `terraform.tfvars` must match the pushed tag. Terraform
outputs `application_secret_id`, `scanner_repository_url`, and `data_bucket`.

## Operations

- A scan is rejected when another task owns the unexpired DynamoDB lease.
- Raw S3 partitions are organized by scan date.
- SQS retries only failed news records; poison records move to the DLQ.
- CloudWatch alarms cover news errors, the news DLQ, scheduler target errors,
  and scanner failures.
- All consensus input preserves source timestamps and parser versions.
- Provider IDs are preferred for event matching. Comparable conflicting IDs
  are rejected rather than falling back to names.
- NFL tie and tennis walkover/withdrawal settlement profiles must be
  recognized or the market is excluded.

No model output can create an actionable numeric edge. Bedrock can only
preserve or downgrade a sportsbook-derived candidate.
