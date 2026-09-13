-- History archive: every scan's point-in-time inputs and decisions, the
-- outcome of every market seen, and backtest reports. Prices and
-- probabilities are NUMERIC because the application computes in Decimal.
-- Timestamps are TIMESTAMPTZ. Every table carries the observation time
-- (`fetched_at`, `evaluated_at`) separately from the event time
-- (`start_time`, `source_timestamp`); replays filter on observation time.

CREATE TABLE IF NOT EXISTS scans (
    scan_id       UUID PRIMARY KEY,
    started_at    TIMESTAMPTZ NOT NULL,
    -- The `now` every freshness, lead and news check in the scan used.
    evaluated_at  TIMESTAMPTZ NOT NULL,
    completed_at  TIMESTAMPTZ NOT NULL,
    market_count  INTEGER NOT NULL,
    quote_count   INTEGER NOT NULL,
    -- Gates in force; the class of each stored row was decided under these.
    settings      JSONB NOT NULL,
    -- Paper portfolio at evaluation time; sizing is bounded by its exposure.
    portfolio     JSONB NOT NULL,
    source_health JSONB NOT NULL,
    -- 'live' from the scanner, 'import' from files written before the
    -- archive existed (no books, so only the decided rows survive).
    origin        TEXT NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS scans_evaluated_at ON scans (evaluated_at);

-- Slowly changing: one row per Polymarket US market, latest definition.
CREATE TABLE IF NOT EXISTS markets (
    market_id         TEXT PRIMARY KEY,
    market_slug       TEXT NOT NULL,
    event_id          TEXT NOT NULL,
    sport             TEXT NOT NULL,
    start_time        TIMESTAMPTZ NOT NULL,
    long_participant  TEXT NOT NULL,
    short_participant TEXT NOT NULL,
    -- Full UsMoneylineMarket; NULL for markets known only from imported
    -- opportunity rows.
    market            JSONB,
    first_seen_at     TIMESTAMPTZ NOT NULL,
    last_seen_at      TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS markets_start_time ON markets (start_time);

-- Order books as fetched, for the markets a scan had a consensus for. The
-- market fields that vary between scans are copied so a replay sees them as
-- the scan did.
CREATE TABLE IF NOT EXISTS scan_books (
    scan_id       UUID NOT NULL REFERENCES scans (scan_id) ON DELETE CASCADE,
    market_id     TEXT NOT NULL REFERENCES markets (market_id),
    start_time    TIMESTAMPTZ NOT NULL,
    ep3_status    TEXT NOT NULL,
    state         TEXT NOT NULL,
    transact_time TIMESTAMPTZ NOT NULL,
    fetched_at    TIMESTAMPTZ NOT NULL,
    bids          JSONB NOT NULL,
    offers        JSONB NOT NULL,
    PRIMARY KEY (scan_id, market_id)
);
CREATE INDEX IF NOT EXISTS scan_books_market_fetched ON scan_books (market_id, fetched_at DESC);

-- Sportsbook and exchange quotes as collected; matching and consensus are
-- recomputed at replay so both can change.
CREATE TABLE IF NOT EXISTS scan_quotes (
    id               BIGSERIAL PRIMARY KEY,
    scan_id          UUID NOT NULL REFERENCES scans (scan_id) ON DELETE CASCADE,
    source_id        TEXT NOT NULL,
    sport            TEXT NOT NULL,
    event_id         TEXT NOT NULL,
    start_time       TIMESTAMPTZ NOT NULL,
    source_timestamp TIMESTAMPTZ NOT NULL,
    fetched_at       TIMESTAMPTZ NOT NULL,
    quote            JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS scan_quotes_scan ON scan_quotes (scan_id);

-- Every evaluated side, including rejected rows, with the reasons.
CREATE TABLE IF NOT EXISTS opportunities (
    scan_id                  UUID NOT NULL REFERENCES scans (scan_id) ON DELETE CASCADE,
    opportunity_id           UUID NOT NULL,
    market_id                TEXT NOT NULL REFERENCES markets (market_id),
    event_id                 TEXT NOT NULL,
    side                     TEXT NOT NULL,
    class                    TEXT NOT NULL,
    sport                    TEXT NOT NULL,
    participant              TEXT NOT NULL,
    fair_probability         NUMERIC NOT NULL,
    conservative_probability NUMERIC NOT NULL,
    executable_price         NUMERIC NOT NULL,
    maker_price              NUMERIC,
    maker_net_edge           NUMERIC,
    raw_edge                 NUMERIC NOT NULL,
    net_edge                 NUMERIC NOT NULL,
    quantity                 NUMERIC NOT NULL,
    maximum_loss             NUMERIC NOT NULL,
    estimated_fee            NUMERIC NOT NULL,
    family_count             INTEGER NOT NULL,
    source_ids               JSONB NOT NULL,
    reasons                  JSONB NOT NULL,
    start_time               TIMESTAMPTZ NOT NULL,
    book_time                TIMESTAMPTZ NOT NULL,
    generated_at             TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (scan_id, opportunity_id)
);
CREATE INDEX IF NOT EXISTS opportunities_market ON opportunities (market_id, generated_at);
CREATE INDEX IF NOT EXISTS opportunities_class ON opportunities (class, generated_at);

CREATE TABLE IF NOT EXISTS news_evidence (
    opportunity_id    UUID NOT NULL,
    generated_at      TIMESTAMPTZ NOT NULL,
    confidence_effect TEXT NOT NULL,
    manual_review     BOOLEAN NOT NULL,
    summary           TEXT NOT NULL,
    citations         JSONB NOT NULL,
    PRIMARY KEY (opportunity_id, generated_at)
);

-- Closing line and settlement for every market seen, not only paper
-- positions. Rows are created when grading first attempts a market.
CREATE TABLE IF NOT EXISTS market_outcomes (
    market_id            TEXT PRIMARY KEY REFERENCES markets (market_id),
    -- Venue side prices at the last observation before scheduled start.
    closing_long         NUMERIC,
    closing_short        NUMERIC,
    closing_observed_at  TIMESTAMPTZ,
    closing_recorded_at  TIMESTAMPTZ,
    -- YES payout per contract once the venue settles.
    settlement           NUMERIC,
    settled_recorded_at  TIMESTAMPTZ,
    attempts             INTEGER NOT NULL DEFAULT 0,
    next_attempt_at      TIMESTAMPTZ NOT NULL,
    last_error           TEXT
);
CREATE INDEX IF NOT EXISTS market_outcomes_pending
    ON market_outcomes (next_attempt_at) WHERE settlement IS NULL;

CREATE TABLE IF NOT EXISTS backtest_runs (
    run_id       UUID PRIMARY KEY,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    from_at      TIMESTAMPTZ NOT NULL,
    through_at   TIMESTAMPTZ NOT NULL,
    settings     JSONB NOT NULL,
    report       JSONB NOT NULL
);
