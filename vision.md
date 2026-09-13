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
timing, and news context to audit the decision.

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

Raw scans, source timestamps, parser versions, normalized quotes, decisions,
and news citations are retained so a recommendation can be reconstructed.

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

- Scheduled scans complete reliably every five minutes
- Every configured source adapter is monitored for freshness and failures
- No actionable result violates matching, quorum, price, or exposure rules
- Every actionable result has current news evidence and traceable citations
- Paper positions and maximum loss remain internally consistent
- Scanner overlap, Lambda failures, and news DLQ messages are alarmed
- Historical paper results can be evaluated without look-ahead data

Profit is not an initial acceptance criterion. A statistically meaningful
paper history, calibration, drawdown, and execution-slippage review must come
before any proposal for authenticated live trading.

## Future Direction

Settlement ingestion, paper P&L, and closing-line value are recorded per
position. After sufficient paper history:

1. Fit `CONSENSUS_BIAS` and the exchange lead window from realized
   calibration instead of literature priors.
2. Measure drawdown, source contribution, and maker-vs-taker fill assumptions
   against the recorded closing lines.
3. Add replayable historical backtests using only point-in-time data.
4. Tune sport-specific matching and settlement policies.
5. Evaluate whether a separate, explicitly approved live-execution service is
   justified.

Any live-execution phase must remain isolated from research, use separate
credentials and IAM boundaries, and require explicit human approval.
