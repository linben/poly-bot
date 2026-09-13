# Polybot

Rust research and paper-risk tooling for pregame sports markets on the
Polymarket US API. It scans NFL, NBA, WNBA, MLB, and tennis moneylines, compares
the executable US order book with a de-vigged consensus built from free public
odds sources (Pinnacle, Action Network's book lines, ESPN/DraftKings, Kalshi,
Polymarket global, Smarkets) and an optional quota-limited confirmation feed,
and enriches candidates with recent
Exa search evidence summarized by AWS Bedrock.

This repository does not contain wallet credentials, exchange authentication,
or order-submission code. It cannot place a live trade.

Project direction is defined in [vision.md](vision.md). The implemented
architecture and technology choices are documented in
[tech-stack.md](tech-stack.md).

## Quick Start

Prerequisites: Rust 1.90+, outbound HTTPS. Nothing else is required for local
mode; Postgres, Docker, Terraform, and AWS are optional layers described later.

1. **Build and configure.**

   ```bash
   git clone <repository-url> poly-bot && cd poly-bot
   cp .env.example .env
   set -a; source .env; set +a
   cargo build --release
   ```

   `.env` works as shipped: six free public odds sources, the default gates,
   a $100 paper bankroll, no news reviewer. Two optional keys change what you
   can see: `EXA_API_KEY` (with `NEWS_REVIEWER=keyword`) lets candidates pass
   news review and become actionable; `THE_ODDS_API_KEY` (with
   `ENABLE_THE_ODDS_API=true`) adds the Pinnacle-class reference book that the
   actionable class requires. Without them, results stop at the watchlist,
   which is still enough to evaluate the scanner.

2. **Check the sources reach you.**

   ```bash
   cargo run --release --bin source-probe -- --adapters-only
   ```

   Each adapter runs a real collection and reports quote counts and latency.
   The scanner refuses to start with fewer than `MINIMUM_CONFIGURED_SOURCES`
   (3) healthy source families, so fix reachability here first.

3. **Run one scan.**

   ```bash
   make local-once
   ```

   One scan, one news pass, one settlement pass, then exit; the snapshot is
   printed as JSON and written under `data/`. Expect a few hundred markets
   and quotes (measured 2026-09-13: 380 markets, 520 quotes, 13 s cold), and
   every evaluated side classified with its rejection reasons. A quiet market
   legitimately produces zero candidates.

4. **Run the operator loop with the terminal UI.**

   ```bash
   make local
   ```

   From a TTY, `local` scans every `SCAN_INTERVAL_SECONDS` (300), reviews
   news, settles paper positions, and opens the UI in the same process. `q`
   quits both. See [Using the terminal UI](#using-the-terminal-ui).

5. **Run unattended.** `make local-headless` runs the loops without the UI;
   for a persistent daemon install `deploy/polybot-local.service` (see
   [Running as a daemon](#running-as-a-daemon)). Attach a viewer from any
   other terminal with `make tui`.

6. **Optional: keep history and backtest.** `make db-up`, set `DATABASE_URL`
   in `.env`, and build with `--features postgres`; see
   [History archive and backtesting](#history-archive-and-backtesting-optional-postgres).

Where things go: scans, `latest-*.json`, `portfolio.json`, `news/`, and
`health.json` live under `data/` (`DATA_DIR`). `make local-explore` runs the
loosest gates into a separate `data-explore/` so a quiet day still shows how
rows are classified; paper positions opened there are not comparable with the
default policy.

## Decision Policy

- Polymarket API: `https://gateway.polymarket.us`
- Markets: structured pregame full-game/match moneylines only
- Contract acquisition price: `$0.35` through `$0.65`
- Paper bankroll: `$100`
- Position risk: quarter Kelly (`KELLY_FRACTION`) on the conservative
  probability, bounded to 1-5% of bankroll (floor configurable); at most
  `MAXIMUM_EVENT_EXPOSURE` ($2.50) across the markets of one event and $5 in
  total
- Watchlist: at least three independent source families (configurable, >= 1)
- Actionable consensus: at least five independent families, including a
  reference book (both configurable)
- Edge: at least five percentage points raw and three percentage points after
  dispersion, consensus bias and fees. `raw edge = fair - VWAP`;
  `net edge = (fair - MAD - CONSENSUS_BIAS) - VWAP - fee`. The bias (default
  0.02) is the intercept by which a bookmaker consensus over-states the
  outcome you back (Kaunitz et al. 2017 measured 0.034-0.037 on football
  closing odds); it is a prior until paper settlement history can fit it
- Lead time: a market starting within `MINIMUM_LEAD_MINUTES` (15) is not
  actionable. Kalshi and Polymarket global quotes leave the consensus for games
  more than `EXCHANGE_MAX_LEAD_HOURS` (4) out: exchange prices are calibrated
  in the last hours before close and drift beyond that (Moshrefi 2026, 23 M
  Kalshi moneyline trades)
- Confirmation: when any candidate survives the continuous pass, refetch up to
  three contributing continuous sources plus every confirmation-tier source
  (The Odds API) for the candidate sports, replace their initial quotes, and
  fail closed if the quorum no longer holds. Only the refetched sources are
  held to the 90 s confirmation window; families that were not refetched keep
  the preliminary window, so a slow first pass cannot silently drop them
- Freshness: a quote is fresh by when it was last observed, not by when the
  book last moved the line; an unmoved line is still a live price
- News gate: a candidate remains watchlist until the news reviewer returns
  corroborated `unchanged` evidence; `lower`, `review`, and `reject` downgrade
  it
- News cache: stable market-side IDs reuse evidence for `NEWS_REFRESH_SECONDS`
  (default 3600); evidence older than two hours cannot preserve an actionable
  class

Polymarket US exposes one YES book. The engine interprets:

- Long cost as the YES offer
- Opposing-outcome cost as `1 - YES bid`

Sizing walks all eligible book levels, calculates a side-specific VWAP, and
applies the venue's fee arithmetic: `0.06 * C * p * (1 - p)` per fill,
banker's-rounded to cents, with the order total capped at the rounding of the
cumulative exact fee. Kelly is computed at top of book, then recomputed once at
the depth-weighted cost so size never exceeds what the VWAP edge supports.
Every row also reports a `maker edge`: the same conservative edge if the order
rested one tick inside the spread and earned the `0.0125 * p * (1 - p)` maker
rebate instead of paying the taker fee (about 1.8 pp better at the midpoint).
It never classifies a row, because a resting order is not guaranteed to fill.

### Paper ledger, closing line, and settlement

A paper position records quantity, VWAP entry price, fair probability, and fee
at open. Once the game starts, the engine (every `SETTLEMENT_POLL_SECONDS`,
default 600) fetches the venue's `INTERVAL_LIVE` price history and stores the
side price at the last point before scheduled start as the closing line, then
polls `GET /v1/markets/{slug}/settlement`; when the market settles it books
`realized P&L = quantity * (payout - entry) - fee` (payout is the settlement
for long, `1 - settlement` for short), moves the position to
`closed_positions`, and sets `bankroll = PAPER_BANKROLL + realized`. Closing
line value (`closing - entry`, positive when the entry beat the venue's
pre-start price) is the leading indicator the project is built to measure;
realized P&L is the lagging one. `paper settle` runs the same pass by hand.
Both endpoints are public, so this works in local mode without credentials.

Polymarket US settles a game whose participant withdraws or that is postponed
past its expiry at *last fair market price*, not $0.50 (ITF tennis excepted),
where sportsbooks void. The keyword reviewer therefore treats
postponement/rain-out/no-contest language as a hard `review`.

## Run Modes

`RUN_MODE` selects where state lives and which services run. Both modes share
the same scanner, consensus, sizing, and news logic.

| | `local` (default) | `cloud` |
| --- | --- | --- |
| Process | one `local` binary: scan loop, news loop, retention, terminal UI | ECS one-shot `scanner`, Lambda `news-worker` |
| Store | files under `data/` (`scans/` with per-scan snapshot, `-quotes`, `-markets` and `-books` files, `latest-opportunities.json`, `latest-scan.json`, `opportunities.ndjson`, `news-queue.ndjson`, `news/`, `portfolio.json`, `scanner.lock`) | S3 + DynamoDB + SQS |
| News reviewer | `keyword` (Exa + risk-term classifier), `bedrock`, `off`, or none | Bedrock via SQS |
| Dashboard | terminal UI in-process, or `tui` attached to `data/` | `tui` with `RUN_MODE=cloud` and AWS credentials |
| Requires | Rust toolchain, outbound HTTPS | AWS account, Terraform, Docker, cargo-lambda |

### History archive and backtesting (optional Postgres)

Both modes keep working with files or AWS alone. Setting `DATABASE_URL` on a
build with `--features postgres` adds a history archive alongside the store;
nothing else changes. `make db-up` starts the compose Postgres; the archive
creates its own schema on first connection (`migrations/`).

What it unlocks:

- **Durable point-in-time inputs.** Every scan writes its markets, order
  books, source quotes, the paper portfolio, the gates in force, and every
  evaluated row (including rejections and their reasons) to Postgres. File
  retention (`LOCAL_RETENTION_DAYS`) no longer bounds the research window.
- **Outcomes for every market seen, not only paper positions.** The `local`
  engine's settlement tick also grades every archived market whose game has
  started: the venue's pre-start closing line once, then the settlement price
  until published (`history grade` does the same by hand). This is the data
  that calibration, closing-line value, and market-efficiency questions need;
  a paper book of a few positions never accumulates it.
- **Replayable backtests.** `backtest` evaluates each archived scan at its own
  `evaluated_at` with the same `consensus_markets`/`evaluate_books` functions
  the scanner runs, under the gates in the environment, and simulates the
  paper portfolio through the window:

  ```bash
  make history-import                        # archive existing data/scans (idempotent)
  make history-status
  make backtest ARGS="--from 2026-09-01 --open-class watchlist --report reports/sep.json"
  MINIMUM_RAW_EDGE=0.03 make backtest ARGS="--from 2026-09-01"   # a policy variant
  make backtest-audit ARGS="--from 2026-09-01"                   # live-vs-replay parity
  ```

  The report carries Brier scores and reliability buckets for the sportsbook
  consensus, the conservative probability and the Polymarket US price (long
  side, last observation per market, settled markets only); realized return
  per contract by raw-edge bucket; closing-line value for candidates and for
  filled positions; paper P&L, ROI on risk, drawdown and the equity curve;
  coverage counts (frames recomputed vs imported without books, graded vs
  ungraded observations); and a `limitations` list. `--fill next-book` fills
  an order only against the *next* archived book at or below the decision
  price and counts unfilled orders instead of assuming them. `--audit`
  recomputes every live frame under its archived gates and portfolio and
  diffs against the stored rows; a non-zero mismatch count means live and
  replay have diverged. Every run is stored in `backtest_runs` and marked
  `production_approved: false`.

  Two things a replay cannot see: news evidence (classes are pre-news), and
  the venue's actual settlement time (exposure is released
  `--settlement-lag-hours` after scheduled start). Scans imported from files
  written before books were captured keep the class decided at the time and
  cannot be re-gated on depth; the report counts them separately.

### Local mode

```bash
cp .env.example .env            # optional: add THE_ODDS_API_KEY / EXA_API_KEY
set -a; source .env; set +a
make local                      # or: cargo run --release --bin local
make local-explore              # loosest gates, separate data-explore/ dir
make local-once                 # one scan + one news pass, then exit
make local-headless             # loops only, no terminal UI
make tui                        # attach a viewer to data/ (or RUN_MODE=cloud)
```

Every gate is an environment variable (see `.env.example`): `MINIMUM_RAW_EDGE`,
`MINIMUM_NET_EDGE`, `CONSENSUS_BIAS`, `MINIMUM_PRICE`/`MAXIMUM_PRICE`,
`MINIMUM_LEAD_MINUTES`, `EXCHANGE_MAX_LEAD_HOURS`,
`WATCHLIST_SOURCE_FAMILIES` (>= 1; a single family is one venue, not consensus), `MINIMUM_SOURCE_FAMILIES` (>= watchlist),
`REQUIRE_REFERENCE_BOOK`, `KELLY_FRACTION`, `MINIMUM_POSITION_FRACTION` (0
disables the size floor), `MAXIMUM_EVENT_EXPOSURE`, and the freshness windows
`MAX_QUOTE_AGE_SECONDS`/`CONFIRMATION_MAX_AGE_SECONDS`/`BOOK_MAX_AGE_SECONDS`.
The System view shows the values in force. `make local-explore` runs the
loosest combination validation permits into `data-explore/`, so a quiet market
still classifies rows; paper positions opened there are not comparable with the
default policy. Under any policy the Overview table is never empty once a scan
has run: with no live candidate it ranks every side by how close it is to the
gates, and the `gap to gates` column names what each still needs (`edge
+4.3pp`, `fam +2`, `price`).

`local` runs a fixed-cadence scan (`SCAN_INTERVAL_SECONDS`, default 300),
drains the news queue every 20 s, settles started paper positions every
`SETTLEMENT_POLL_SECONDS` (600), prunes scan snapshots older than
`LOCAL_RETENTION_DAYS` (default 14), and, when stdin and stdout are a TTY,
opens the terminal UI in the same process; `q` quits the UI and stops the
loops. `--headless` skips the UI. A failed scan is logged and retried next
tick; only configuration errors exit. Without the UI, logs go to stderr and
JSON to stdout.

### Running as a daemon

`deploy/polybot-local.service` is a user systemd unit (install steps in its
header). Three things make it supervisable rather than merely restarted:

- **Health file.** The engine writes `<data-dir>/health.json` on every state
  change (atomic rename): phase, scan counters, last scan id/duration, next
  scan time, last error, reviewer, sources, git commit, PID. `cat
  data/health.json` or the `tui` answers "is it alive and what did it last
  do" without attaching to the process.
- **Native watchdog.** The unit is `Type=notify` with `WatchdogSec=900`:
  `local` sends `READY=1` once the store and sources are up, `WATCHDOG=1`
  after every scan attempt (success or failure), `STATUS=` with the health
  line, and `STOPPING=1` on SIGTERM/Ctrl-C. A scan that hangs for three
  intervals is killed and restarted; a loop that is alive but failing keeps
  pinging, because a restart cannot fix an unreachable source, and shows up
  in `health.json`/`systemctl status` instead. Outside systemd every
  notification is a no-op.
- **Immutable deploy path.** `make deploy` builds and installs the binaries
  under `target/deploy/<git-sha>/` and points `target/deploy/current` at it;
  the unit runs from `current`, so `cargo build` never swaps the file under a
  running daemon. Restart the unit to adopt a new deploy. `FEATURES=--features
  postgres DEPLOY_BINS="local tui paper history backtest" make deploy` for an
  archive-enabled host.

The unit also sets `Restart=always` with `StartLimitIntervalSec=0` (a flapping
source must not leave the unit stopped), `NoNewPrivileges`, a socket-family
allowlist, and `UMask=0077`. Mount-namespace sandboxing is deliberately
omitted for a user unit; add `ProtectSystem=strict` + `ReadWritePaths` when
running it as a system unit.

Measured on 2026-09-12 against live APIs (release build): a cold scan takes
about 3.3 s, of which market discovery is 2.2-2.5 s of Polymarket server time
(an NFL events page is 42 MB uncompressed, 1.4 MB gzip); discovery is cached
for `DISCOVERY_REFRESH_SECONDS` (default 900). Books are fetched in parallel
(`POLYMARKET_CONCURRENCY`, default 8) but request starts are paced to
`POLYMARKET_REQUESTS_PER_SECOND` (18) because the gateway documents a
20 requests/second/IP public limit; 32 unpaced concurrent requests were not
throttled when measured, but a 429 now backs every request off for one second
and retries once. At 18 rps the ~75 matched books take about 4 s per pass
(measured 2026-09-13: 13.6 s for a cold scan including discovery). Before this
work a scan took ~24 s, dominated by a serial 110 ms per-request throttle.
Started games are dropped at evaluation time so the discovery cache cannot
leak an in-play book.

### Using the terminal UI

The UI is the operator surface in both modes. There are two ways in:

```bash
make local        # engine + UI in one process: live phase, countdown, log, `r` rescans now
make tui          # attach a read-mostly viewer to data/ (or DynamoDB/S3 with RUN_MODE=cloud)
```

`local` needs a TTY on both stdin and stdout; without one it falls back to
headless. The attached `tui` polls the store every 2 s (10 s in cloud mode)
and can open and close paper positions, but cannot trigger a scan; `r` reloads
the store instead. Several viewers may attach at once. There is no hosted web
UI.

**Reading the screen.** The top line is the status bar: run mode, engine phase
(`STARTING` / `IDLE` / `SCANNING` / `REVIEWING`, or `ATTACHED` for `tui`),
age and duration of the last scan, market/quote/evaluated/candidate counts,
the countdown to the next scan, and, after an action, a transient notice
(`scan requested`, the opened position, or the reason it was refused); the
last error shows there when nothing else does. The bottom line lists the keys
for the current view and the view tabs. Colour is consistent everywhere:
green = actionable, reachable, positive edge; yellow = watchlist,
`lower`/`review` news, headroom warnings; red = failures and `reject`; grey =
rejected rows.

**A typical session.**

1. **Overview** (`1`). Watch the CANDIDATES table. It is never empty once a
   scan has run: live candidates come first, then every other side ranked by
   how close it is to the gates, and the `gap to gates` column says what each
   still lacks (`edge +4.3pp`, `fam +2`, `price`). SCAN, SOURCES and EDGE &
   PAPER summarize freshness, source health, edge distribution and exposure
   headroom. Press `r` to scan now instead of waiting for the tick.
2. **Markets** (`2`). Every evaluated side. Move with `↑`/`↓` (or `j`/`k`,
   `PgUp`/`PgDn`, `g`/`G`), press `⏎` to toggle the DETAIL pane for the
   selected row: consensus and conservative probability, VWAP for the sized
   quantity, maker price and maker edge, size, fee, contributing sources,
   news summary and citations, and every rejection reason in full. `s` cycles
   the sort (net edge → raw edge → families → class → sport); `f` cycles the
   sport filter (NFL, NBA, WNBA, MLB, tennis, all).
3. **Sources** (`3`). Per adapter: reachable or not, quotes returned, latency,
   and how many markets it matched per sport. A family missing here is why a
   row reads `fam +N` on the Overview.
4. **Open a paper position.** With a row selected on Markets (or from the
   Overview, where `o` takes the top candidate) press `o`. This applies the
   same rules as `paper open`: the row must be actionable in the latest scan
   with `unchanged` news evidence less than two hours old, and the position
   must fit the per-event and $5 total exposure caps. A refusal names the
   rule in the notice line. Nothing is ever opened automatically.
5. **Portfolio** (`4`). Bankroll (base plus realized), an exposure gauge with
   headroom, and realized P&L. Open positions show quantity, entry VWAP, the
   venue's closing line once the game has started, and closing-line value
   (`closing - entry`, the project's leading indicator). Closed positions show
   payout and P&L. Settlement happens on the engine's `SETTLEMENT_POLL_SECONDS`
   tick (or `paper settle`); `c` archives the selected open position by hand
   without a result.
6. **System** (`5`). SETTINGS IN FORCE: edge gates, price band, quorum, risk
   limits, freshness windows, cadences, concurrency, gateway URL and the
   configured sources, as validated from `.env`; below it the scrollable log
   (`↑`/`↓`, `g`/`G`; in-process only, an attached viewer has no log). When a
   row is rejected for a reason you did not expect, this is where to confirm
   which threshold is active.

**Key reference.**

| Key | Where | Action |
| --- | --- | --- |
| `Tab` / `Shift-Tab`, `1`–`5` | all | switch view |
| `q`, `Esc`, `Ctrl-C` | all | quit; in `local` also stops the engine loops |
| `r` | all | rescan now (`local`) or reload the store (`tui`) |
| `↑`/`↓`, `j`/`k`, `PgUp`/`PgDn`, `Home`/`g`, `End`/`G` | Markets, Portfolio, System | move the cursor / scroll |
| `⏎` | Markets | toggle the DETAIL pane |
| `s` | Markets | cycle sort key |
| `f` | Markets | cycle sport filter |
| `o` | Overview, Markets | paper-open the top candidate / the selected row |
| `c` | Portfolio | close the selected open position without a result |

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
Exa search and Bedrock enrichment.

S3 stores raw scans and quote snapshots. DynamoDB stores current
recommendations, news evidence, paper portfolio state, and the scanner lease.
Operators inspect the cloud store with `RUN_MODE=cloud cargo run --features aws
--bin tui` under their own AWS credentials.

### News reviewers

`NEWS_REVIEWER` picks how candidates are vetted. Every reviewer can only
preserve or downgrade a candidate.

- `keyword` (default when `EXA_API_KEY` is set): Exa search, then a
  deterministic classifier. A citation counts only if it names the participant;
  hard terms (ruled out, scratched, suspended, withdrawn, postponed, ...) force
  `review` with manual review, soft terms (questionable, doubtful, injury,
  weather delay) give `lower`, otherwise `unchanged`. No citations at all is
  `review`.
- `bedrock`: the same Exa citations summarized by Bedrock (build with
  `--features aws`; needs `BEDROCK_MODEL_ID` and AWS credentials).
- none (default without an Exa key): no evidence is written, so nothing can
  leave the watchlist.
- `off`: records `unchanged` without searching. This removes the news veto and
  is only for operators who review every candidate by hand.

Exa (`POST https://api.exa.ai/search`, `x-api-key`) replaced Brave Search,
which dropped its free API plan. One request per candidate asks for eight
results with highlights for `<participant> <market slug> <sport> injury lineup
suspension withdrawal weather schedule latest`; the highlights become the
citation snippet, so the classifier and Bedrock read the actual roster or
injury text rather than a search-result blurb. No published-date or category
filter is sent: Exa excludes undated pages under a date filter, and the
game-day injury tables (ESPN, FOX, StatMuse, CBS) carry no published date. The
dated market slug in the query keeps results on the right game. Exa's
`publishedDate` is accepted as RFC 3339 or `YYYY-MM-DD`; a citation without
one is kept but cannot prove freshness.

## Odds Sources

Six continuous sources run every scan. They are public, unauthenticated, and
unmetered; each is disabled with `ENABLE_<NAME>=false`:

| Source | Families | Sports | Notes |
| --- | --- | --- | --- |
| Pinnacle guest API | `pinnacle` (reference) | NFL, NBA, WNBA, MLB, tennis | The logged-out feed behind pinnacle.com; period-0 moneylines joined to pregame matchups; props, alternates and live matchups dropped. Makes the actionable quorum reachable |
| Action Network scoreboard | `draft_kings`, `fan_duel`, `bet_mgm`, `caesars`, `bet365`, `kambi`, `fanatics`, ... | NFL, NBA, WNBA, MLB | One call per league returns each book's line with its own `inserted` timestamp; lines older than 48 h are dropped as stale openers. The API's book set varies call to call, so coverage per book fluctuates |
| ESPN core odds | `draft_kings` (per provider ESPN serves) | NFL, NBA, WNBA, MLB | Current-week scoreboard only; American odds converted to decimal |
| Kalshi public market API | `kalshi` | NFL, NBA, WNBA, MLB | `1 / yes ask` per side; skipped when spread > 6c; NFL/NBA rules carry only a date, so the quote uses a 14-hour start tolerance |
| Polymarket global Gamma | `polymarket_global` | NFL, NBA, WNBA, MLB, tennis | `1 / ask` and `1 / (1 - bid)`; liquidity floor `POLYMARKET_GLOBAL_MIN_LIQUIDITY`, traded-volume floor `POLYMARKET_GLOBAL_MIN_VOLUME` (seeded, never-traded books sit at 50/50), spread <= 4c |
| Smarkets exchange | `smarkets` | NFL, MLB, tennis (NBA/WNBA when listed) | Best offer per contract (`10000 / price`); markets with a > 10 pp spread, thin size or one empty side skipped. Quotes endpoint is limited to 20 requests/min, which one scan uses ~5 of |

Skins of one operator collapse onto one family (DraftKings via ESPN and via
Action Network is one vote), and the newest quote per family wins. With
Pinnacle in the consensus a full-quorum side (five families plus the
reference) is `actionable` when it clears the edge gates; measured on
2026-09-12, 40 of 148 evaluated sides had 5-8 families.

Probed and rejected from this host (US datacenter IP, no keys): DraftKings,
Caesars, BetMGM and Circa direct APIs return 403; Bovada returns an empty
body; BetOnline times out; Kambi's public CDN needs a customer key; FanDuel's
content API works but adds nothing Action Network does not already carry;
Novig has no public API. The Odds API (`ENABLE_THE_ODDS_API=true` plus
`THE_ODDS_API_KEY`) remains available as a confirmation tier: it is only
queried for sports that already have a candidate, one request per sport
returns every US and EU book mapped onto its owning family, and it stops
spending below `THE_ODDS_API_MIN_REMAINING` credits.

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

Running the system is covered in [Quick Start](#quick-start). For development:

- Rust 1.90 or newer; for cloud mode only, Terraform 1.8+, Docker, `cargo-lambda`
- `make check` (fmt + clippy, all features, warnings denied) and `make test`
  are the gates; Postgres-backed code compiles with `--features postgres`

```bash
set -a; source .env; set +a
make check && make test
cargo run --bin source-probe -- --adapters-only
make local-once
```

The scanner uses persisted `portfolio.json` positions for exposure checks; it
never opens a position on its own. The UI's `o`/`c` keys and the `paper` CLI
are the two ways to act on a candidate, and they apply the same rules:

```bash
cargo run --bin paper -- list       # open and closed positions, CLV, P&L, bankroll
cargo run --bin paper -- open <opportunity-uuid>
cargo run --bin paper -- close <opportunity-uuid>   # archive without a result
cargo run --bin paper -- settle     # record closing lines, settle started games
```

`cargo run --bin scanner` performs one scan and exits (the cloud task shape;
`--continuous` loops). All binaries start with the six built-in public
sources and fail at startup with fewer than `MINIMUM_CONFIGURED_SOURCES`
distinct continuous families. Without a reference book (Pinnacle disabled and
no confirmation tier) they log a warning: results can reach the watchlist but
never become actionable.

`source-probe` runs a real collection through every configured adapter and
reports quote counts and latency; with `--adapters-only` it skips the catalog
homepage reachability checks. Logs go to stderr, JSON results to stdout.

## AWS Deployment (cloud mode)

1. Populate the Terraform variables and create the managed ECR repository.
2. Build and push the scanner image using the immutable configured tag.
3. Build the `news-worker` Lambda archive.
4. Apply the complete Terraform stack.
5. Put the Exa API key in the generated secret.

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
  --secret-string '{"exa_api_key":"replace-me","the_odds_api_key":"optional"}'
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

No reviewer output can create an actionable numeric edge. The keyword
classifier and Bedrock can only preserve or downgrade a sportsbook-derived
candidate.
