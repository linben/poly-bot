# Polybot

Rust research and paper-risk tooling for pregame sports markets on the
Polymarket US API. It scans NFL, NBA, WNBA, MLB, and tennis moneylines, compares
the executable US order book with de-vigged sportsbook consensus, and enriches
candidates with recent Brave Search evidence summarized by AWS Bedrock.

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
- Total open exposure: at most `$5`
- Watchlist: at least three independent source families
- Actionable consensus: at least five independent families, including a
  reference book
- Edge: at least five percentage points raw and three percentage points after
  dispersion and fees
- Confirmation: refetch three relevant books, replace their initial quotes,
  and fail closed if the quorum no longer holds
- News gate: a candidate remains watchlist until Bedrock returns corroborated
  `unchanged` evidence; `lower`, `review`, and `reject` downgrade it
- News cache: stable market-side IDs reuse evidence for one hour; evidence
  older than two hours cannot preserve an actionable class

Polymarket US exposes one YES book. The engine interprets:

- Long cost as the YES offer
- Opposing-outcome cost as `1 - YES bid`

Sizing walks all eligible book levels, calculates a side-specific VWAP, and
applies fee rounding at each consumed level.

## Why ECS and Lambda

The five-minute scanner runs as a scheduled one-shot ECS Fargate task. It can
perform concurrent network collection without Lambda's packaging and duration
constraints. A DynamoDB lease prevents overlapping scheduled tasks.

Lambda is used for the short, event-driven work:

- SQS-triggered Brave Search and Bedrock enrichment
- Authenticated dashboard API

S3 stores raw scans and quote snapshots. DynamoDB stores current
recommendations, news evidence, paper portfolio state, and the scanner lease.
CloudFront serves a private S3 dashboard and routes `/api/*` to API Gateway.
Cognito uses authorization code flow with PKCE.

## Sportsbook Sources

`config/sources.json` defines ten preferred books:

1. Pinnacle
2. Circa
3. Bookmaker.eu
4. BetOnline
5. bet365
6. DraftKings
7. FanDuel
8. Caesars
9. BetMGM
10. Fanatics

BetRivers, Hard Rock Bet, and theScore Bet are fallbacks. The source-family
model prevents multiple skins from being counted as independent evidence.

There is no free hosted odds API with enough allowance to collect ten books
across five sports every five minutes. The free services reviewed are useful
only as sampled validators:

| Service | Free allowance reviewed | Use |
| --- | ---: | --- |
| Odds-API.io | 500 requests/day | Occasional comparison |
| The Odds API | 500 requests/month | Opt-in validator |
| SportsGameOdds | 2,500 objects/month | Fixture/sampled validation |

The Odds API adapter is disabled unless both
`ENABLE_THE_ODDS_API_VALIDATOR=true` and `THE_ODDS_API_KEY` are set. It is
excluded from consensus under all conditions.

Every direct book must pass a feasibility and terms review before its endpoint
is configured. Collection must not bypass authentication, CAPTCHA,
geolocation, access controls, or other restrictions. The application accepts
only approved public adapters that emit this canonical JSON:

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

Set each approved adapter URL through its `SOURCE_<BOOK>_URL` environment
variable. A configured URL is not proof that collection is permitted; record
the approval and parser owner outside this service.

The reviewed `polymm` repository contributed useful concepts around de-vigging,
limit pricing, matching, and adverse selection. Its Polygon wallet/order code,
Supabase persistence, and non-US CLOB assumptions are intentionally not used.
Its private scraper pipeline is not present in that repository.

The supplied PredictEngine article is marketing material, not performance
evidence. Its stated backtest results, user counts, bonuses, and claims of
"free money" are not used by this system.

## Local Development

Prerequisites:

- Rust 1.90 or newer
- Terraform 1.8 or newer
- Docker for the scanner image
- `cargo-lambda` for Lambda archives

```bash
cp .env.example .env
set -a; source .env; set +a
cargo test --all-features
cargo run --bin source-probe
cargo run --bin api
```

The local dashboard listens on `http://127.0.0.1:8080` by default. Scanner
output is written under `data/`. The scanner uses persisted `portfolio.json`
positions for exposure checks; it does not silently add a
position before news review. Use the explicit paper commands after reviewing a
candidate:

```bash
cargo run --bin paper -- list
cargo run --bin paper -- open <opportunity-uuid>
cargo run --bin paper -- close <opportunity-uuid>
```

`cargo run --bin scanner` intentionally fails until ten independent direct
source endpoints, including a reference book, are configured. An empty
successful scan would hide a broken production feed.

`source-probe` checks only public homepages for basic reachability. It does not
approve a source or discover private endpoints.

## AWS Deployment

1. Populate the Terraform variables and create the managed ECR repository.
2. Build and push the scanner image using the immutable configured tag.
3. Build the two Rust Lambda archives.
4. Apply the complete Terraform stack.
5. Put the Brave free-plan key in the generated secret.
6. Create the first Cognito user.

```bash
cp infra/terraform.tfvars.example infra/terraform.tfvars
terraform -chdir=infra init
terraform -chdir=infra apply -target=aws_ecr_repository.scanner

REPOSITORY="$(terraform -chdir=infra output -raw scanner_repository_url)"
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin "${REPOSITORY%%/*}"
docker build --platform linux/arm64 -t "$REPOSITORY:sha-REPLACE" .
docker push "$REPOSITORY:sha-REPLACE"

cargo lambda build --release --arm64 --features aws --bin api --bin news-worker
terraform -chdir=infra apply

aws secretsmanager put-secret-value \
  --secret-id "$(terraform -chdir=infra output -raw application_secret_id)" \
  --secret-string '{"brave_search_api_key":"replace-me"}'

aws cognito-idp admin-create-user \
  --user-pool-id "$(terraform -chdir=infra output -raw cognito_user_pool_id)" \
  --username you@example.com
```

The Lambda zip paths default to cargo-lambda's output under `target/lambda/`.
The image tag in `terraform.tfvars` must match the pushed tag. Use Terraform
outputs for the dashboard URL, data bucket, user pool, and ECR repository.

## Operations

- A scan is rejected when another task owns the unexpired DynamoDB lease.
- Raw S3 partitions are organized by scan date.
- SQS retries only failed news records; poison records move to the DLQ.
- CloudWatch alarms cover API errors, news errors, the news DLQ, and scheduler
  target errors.
- All consensus input preserves source timestamps and parser versions.
- Provider IDs are preferred for event matching. Comparable conflicting IDs
  are rejected rather than falling back to names.
- NFL tie and tennis walkover/withdrawal settlement profiles must be
  recognized or the market is excluded.

No model output can create an actionable numeric edge. Bedrock can only
preserve or downgrade a sportsbook-derived candidate.
