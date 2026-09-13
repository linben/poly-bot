# Vision

## Purpose

Polybot is a research and paper-risk system for finding mispriced pregame
sports moneylines on Polymarket US. It combines executable prediction-market
prices, independent sportsbook consensus, and recent news evidence to produce
conservative, reviewable opportunities.

The system is designed for a roughly $100 bankroll. Capital preservation,
source quality, and reproducibility take priority over trade frequency.

## User

The initial user is a single technically capable researcher who:

- Uses the Polymarket US platform
- Reviews NFL, NBA, WNBA, MLB, and tennis
- Wants a five-minute market scan
- Has no budget for paid odds feeds
- Can risk 1-5% per position, with no more than $5 total exposure
- Wants evidence and paper tracking before considering any live execution

## Product Outcome

For each supported pregame moneyline, Polybot should answer:

1. Is the event and settlement contract matched correctly?
2. What probability is implied by independent, de-vigged sportsbook prices?
3. What contract quantity is executable between $0.35 and $0.65?
4. Does the edge survive dispersion, depth, fees, and conservative sizing?
5. Is recent news consistent with the opportunity?
6. Can the position fit inside the paper portfolio limits?

The output is an operational terminal dashboard with actionable, watchlist, and
rejected candidates. Every classification includes enough source, price,
timing, and news context to audit the decision. Over time the system should
also answer, from its own history rather than from claims: how well the
sportsbook consensus and the Polymarket US price each predicted settled
outcomes, and whether the edges it flagged were real.

## Principles

### Fail Closed

Missing sources, stale books, unresolved settlement rules, conflicting event
IDs, insufficient depth, and stale news must prevent an actionable result.
An empty successful scan must not hide a broken source configuration.

### Executable Prices, Not Headlines

Edges are calculated from Polymarket US order-book depth and fees. Displayed
midpoints, sportsbook marketing odds, and model narratives are not executable
prices.

### Independent Evidence

Multiple skins or feeds from the same underlying sportsbook family count once.
An actionable consensus requires at least five independent families and one
reference book.

### News Can Veto, Not Invent

Exa search citations, whether classified by keyword or summarized by Bedrock,
may preserve or downgrade a sportsbook-derived candidate. Neither the
classifier nor an LLM can create a probability, increase position size, or
turn a negative numeric edge into an actionable result.

### Small-Bankroll Discipline

Sizing uses quarter Kelly and is bounded to 1-5% of bankroll. Total open paper
exposure cannot exceed $5. Rounding, minimum quantity, depth, and fees are part
of maximum loss.

### Reproducible Research

Raw scans, source timestamps, parser versions, normalized quotes, order books,
decisions with their reasons, and news citations are retained so a
recommendation can be reconstructed. Every retained record carries the time it
was observed separately from the time of the event it describes, and the
classifier is a pure function of its inputs and a supplied clock, so a replay
over the archive reproduces the live decision rather than approximating it.

## Initial Scope

- Polymarket US public API
- Pregame full-game or match moneylines
- NFL, NBA, WNBA, MLB, and tennis
- Free public continuous sources every five minutes (Pinnacle as the
  reference book, Action Network's per-book lines, ESPN odds, Kalshi,
  Polymarket global, Smarkets), plus a quota-limited multi-book confirmation
  pass and any approved direct sportsbook adapters
- Proportional de-vigging and robust median/MAD consensus
- Provider-ID-first event matching
- Multi-level YES and opposing-outcome execution pricing
- Exa search and Bedrock news review
- Explicit paper position open and close workflow
- Terminal UI over the local or cloud store
- Optional Postgres history archive: every scan's markets, books, quotes and
  rows; closing line and settlement for every market seen; replayable
  backtests with calibration, closing-line value and paper P&L reporting
- Unattended operation: a systemd service with a health file and watchdog
  locally, a scheduled ECS task with alarms in the cloud

## Non-Goals

- Live order submission, wallet custody, or exchange credentials
- Guaranteed profit or loss recovery
- In-play betting
- Props, spreads, totals, futures, parlays, or market making
- Circumventing sportsbook authentication, CAPTCHA, geolocation, or access
  controls
- Treating backtests or marketing claims as proof of future performance
- Using quota-limited free hosted odds APIs as the primary five-minute feed

## Classification Contract

An opportunity is actionable only when all of the following hold:

- The contract is an exact supported pregame moneyline
- The market and order book are open and fresh
- Acquisition VWAP is between $0.35 and $0.65
- Raw edge is at least five percentage points
- Conservative after-fee edge is at least three percentage points
- At least five independent source families remain after confirmation
- A reference sportsbook is present
- Position loss is between 1% and 5% of bankroll
- Total portfolio exposure remains at or below $5
- Fresh news evidence has an `unchanged` effect and needs no manual review

Three or four valid source families can produce a watchlist candidate. Any
hard market, matching, freshness, depth, risk, or news failure produces a
rejected candidate.

## Success Measures

The first production phase is successful when:

- Scheduled scans complete reliably every five minutes, unattended
- Every configured source adapter is monitored for freshness and failures
- No actionable result violates matching, quorum, price, or exposure rules
- Every actionable result has current news evidence and traceable citations
- Paper positions and maximum loss remain internally consistent
- The running system is supervised from outside the process: locally by a
  health file and a systemd watchdog that restarts a hung scan loop; in the
  cloud by alarms on scanner overlap, Lambda failures, and the news DLQ
- Every market the scanner evaluated is graded against the venue's closing
  line and settlement, not only the markets a paper position was opened on
- Historical decisions can be replayed without look-ahead data, and the
  replay reproduces the live classification exactly

Profit is not an initial acceptance criterion. A statistically meaningful
paper history, calibration, drawdown, and execution-slippage review must come
before any proposal for authenticated live trading.

## Deployment Model

Local mode is the primary deployment: one `local` process on a single host
runs the scan, news, settlement, and grading loops, opens the terminal UI when
run interactively, and runs as a systemd service otherwise. Everything it
needs is a Rust toolchain and outbound HTTPS.

Two layers are optional and independent:

- **History archive (Postgres).** Adds durable point-in-time inputs, outcome
  grading for every market seen, and the `backtest` replay. Without it the
  file store keeps a rolling window and the paper ledger alone is graded.
- **Cloud mode (AWS).** The same scanner as a scheduled one-shot ECS task
  with S3, DynamoDB, SQS and a news Lambda, for operators who would rather
  not keep a host running. It shares every decision path with local mode and
  is kept working, but it is not where the research history accumulates
  unless its `DATABASE_URL` points at a reachable Postgres.

Adding a layer never changes a classification; it changes only what is
retained and who restarts the process.

## Future Direction

The archive turns the open questions from "wait for paper positions to
settle" into queries over every market the scanner has seen. After enough
history has accumulated:

1. Fit `CONSENSUS_BIAS` and the exchange lead window from realized
   calibration (the archive's reliability buckets) instead of literature
   priors, and confirm the change with `backtest` before it goes live.
2. Measure drawdown, source contribution, and maker-vs-taker fill assumptions
   against the recorded closing lines and next-book fills.
3. Tune sport-specific matching and settlement policies from graded
   outcomes, including the venue's last-fair-price settlements.
4. Bring cloud mode to parity with local mode for research: grade outcomes
   from the scheduled task and point its archive at a managed Postgres, so a
   host is not required to accumulate history.
5. Evaluate whether a separate, explicitly approved live-execution service is
   justified.

Any live-execution phase must remain isolated from research, use separate
credentials and IAM boundaries, and require explicit human approval.
