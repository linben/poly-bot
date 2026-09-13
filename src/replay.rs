//! Point-in-time replay of the classifier over archived scans. Each frame is
//! evaluated at its own `evaluated_at` with the same `consensus_markets` /
//! `evaluate_books` functions the live scanner runs, a paper portfolio is
//! simulated through the frames, and every observed market side is graded
//! against the venue's closing line and settlement. Pure: frames and outcomes
//! come in, a report comes out; nothing here reads a store or a clock.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    config::Settings,
    domain::{
        MarketBook, Opportunity, OutcomeSide, PaperPortfolio, PaperPosition, RecommendationClass,
        SourceQuote, Sport, UsMoneylineMarket,
    },
    metrics::{self, Drawdown, ReliabilityBucket, Summary},
    risk::{ExecutionLevel, size_position_from_levels},
    scanner::{consensus_markets, evaluate_books},
};

/// Closing line and settlement for one market; either half may be missing
/// until the venue publishes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketOutcome {
    pub market_id: String,
    pub start_time: DateTime<Utc>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub closing_long: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub closing_short: Option<Decimal>,
    pub closing_observed_at: Option<DateTime<Utc>>,
    /// YES payout per contract.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub settlement: Option<Decimal>,
    pub settled_recorded_at: Option<DateTime<Utc>>,
}

impl MarketOutcome {
    pub fn closing_side_price(&self, side: OutcomeSide) -> Option<Decimal> {
        match side {
            OutcomeSide::Long => self.closing_long,
            OutcomeSide::Short => self.closing_short,
        }
    }

    /// Side payout per contract: `settlement` for long, `1 - settlement`
    /// for short.
    pub fn payout(&self, side: OutcomeSide) -> Option<Decimal> {
        self.settlement.map(|settlement| match side {
            OutcomeSide::Long => settlement,
            OutcomeSide::Short => Decimal::ONE - settlement,
        })
    }
}

/// One archived scan with everything a replay needs. `markets` and `books`
/// cover only the markets the scan fetched a book for; an imported scan has
/// neither, only its decided rows.
#[derive(Debug, Clone)]
pub struct Frame {
    pub scan_id: Uuid,
    pub evaluated_at: DateTime<Utc>,
    pub origin: String,
    pub settings: serde_json::Value,
    pub portfolio: PaperPortfolio,
    pub markets: Vec<UsMoneylineMarket>,
    pub books: Vec<MarketBook>,
    pub quotes: Vec<SourceQuote>,
    pub opportunities: Vec<Opportunity>,
}

impl Frame {
    /// Whether the classifier can be re-run on this frame.
    pub fn has_books(&self) -> bool {
        !self.books.is_empty()
    }
}

/// How a paper order becomes a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FillModel {
    /// Fill at the depth-weighted price of the book the decision saw.
    /// Optimistic: assumes the fill happened within the same second.
    SameBook,
    /// Submit at the decision, fill against the next frame's book only if it
    /// still offers the quantity at or below the decision price. Unfilled
    /// orders are counted, never assumed.
    NextBook,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayOptions {
    pub settings: Settings,
    /// Least class that opens a paper position.
    pub open_class: RecommendationClass,
    pub fill: FillModel,
    /// Time after scheduled start at which a settled market releases its
    /// exposure in the simulation; the venue's actual settlement time is not
    /// archived per position.
    pub settlement_lag_hours: i64,
}

fn class_rank(class: RecommendationClass) -> u8 {
    match class {
        RecommendationClass::Rejected => 0,
        RecommendationClass::Watchlist => 1,
        RecommendationClass::Actionable => 2,
    }
}

/// `(probability, payout)` pairs for a Brier score or reliability diagram.
type Pairs = Vec<(Decimal, Decimal)>;

/// The last evaluation of one market side inside the window.
#[derive(Debug, Clone)]
struct Observation {
    class: RecommendationClass,
    sport: Sport,
    market_id: String,
    side: OutcomeSide,
    fair_probability: Decimal,
    conservative_probability: Decimal,
    /// Taker side price for the sized quantity.
    executable_price: Decimal,
    /// Venue's implied probability for the side: top-of-book mid when the
    /// book is archived, else the executable price.
    market_probability: Decimal,
    raw_edge: Decimal,
    fee_per_contract: Decimal,
}

#[derive(Debug, Clone)]
struct PendingOrder {
    opportunity: Opportunity,
    submitted_at: DateTime<Utc>,
}

pub struct Replay {
    options: ReplayOptions,
    portfolio: PaperPortfolio,
    pending: Vec<PendingOrder>,
    observations: HashMap<(String, OutcomeSide), Observation>,
    equity: Vec<(DateTime<Utc>, Decimal)>,
    frames: usize,
    frames_recomputed: usize,
    frames_stored: usize,
    rows_by_class: BTreeMap<String, usize>,
    submitted: usize,
    filled: usize,
    partial_fills: usize,
    unfilled: BTreeMap<String, usize>,
    blocked_by_exposure: usize,
    first_input_at: Option<DateTime<Utc>>,
    last_input_at: Option<DateTime<Utc>>,
}

impl Replay {
    pub fn new(options: ReplayOptions) -> Self {
        let portfolio = PaperPortfolio::new(options.settings.bankroll);
        Self {
            options,
            portfolio,
            pending: Vec::new(),
            observations: HashMap::new(),
            equity: Vec::new(),
            frames: 0,
            frames_recomputed: 0,
            frames_stored: 0,
            rows_by_class: BTreeMap::new(),
            submitted: 0,
            filled: 0,
            partial_fills: 0,
            unfilled: BTreeMap::new(),
            blocked_by_exposure: 0,
            first_input_at: None,
            last_input_at: None,
        }
    }

    /// Advance to `frame`. Frames must arrive in `evaluated_at` order.
    pub fn step(&mut self, frame: &Frame, outcomes: &HashMap<String, MarketOutcome>) {
        let now = frame.evaluated_at;
        self.frames += 1;
        self.first_input_at.get_or_insert(now);
        self.last_input_at = Some(now);
        self.settle_due(now, outcomes);

        let recomputed = frame.has_books();
        let rows = if recomputed {
            self.frames_recomputed += 1;
            let quotes = frame.quotes.iter().collect::<Vec<_>>();
            let evaluable = consensus_markets(&frame.markets, &quotes, &self.options.settings, now);
            evaluate_books(
                &self.options.settings,
                &evaluable,
                &frame.books,
                &self.portfolio,
                now,
            )
        } else {
            self.frames_stored += 1;
            frame.opportunities.clone()
        };
        let books_by_slug = frame
            .books
            .iter()
            .map(|book| (book.market_slug.as_str(), book))
            .collect::<HashMap<_, _>>();
        let markets_by_id = frame
            .markets
            .iter()
            .map(|market| (market.market_id.as_str(), market))
            .collect::<HashMap<_, _>>();

        self.resolve_pending(&markets_by_id, &books_by_slug, now);

        for row in &rows {
            *self.rows_by_class.entry(class_text(row.class)).or_default() += 1;
            let market_probability = books_by_slug
                .get(row.market_slug.as_str())
                .and_then(|book| side_mid(book, row.side))
                .unwrap_or(row.executable_price);
            self.observations.insert(
                (row.market_id.clone(), row.side),
                Observation {
                    class: row.class,
                    sport: row.sport,
                    market_id: row.market_id.clone(),
                    side: row.side,
                    fair_probability: row.fair_probability,
                    conservative_probability: row.conservative_probability,
                    executable_price: row.executable_price,
                    market_probability,
                    raw_edge: row.raw_edge,
                    fee_per_contract: fee_per_contract(row),
                },
            );
            if class_rank(row.class) < class_rank(self.options.open_class)
                || row.quantity <= Decimal::ZERO
                || self.portfolio.has_market_side(&row.market_id, row.side)
                || self.pending.iter().any(|order| {
                    order.opportunity.market_id == row.market_id
                        && order.opportunity.side == row.side
                })
            {
                continue;
            }
            self.submitted += 1;
            match self.options.fill {
                FillModel::SameBook => {
                    let position = PaperPosition::from_opportunity(row, now);
                    if !self
                        .portfolio
                        .open(position, self.options.settings.maximum_total_exposure)
                    {
                        self.blocked_by_exposure += 1;
                    } else {
                        self.filled += 1;
                    }
                }
                FillModel::NextBook => self.pending.push(PendingOrder {
                    opportunity: row.clone(),
                    submitted_at: now,
                }),
            }
        }
    }

    /// Orders submitted on an earlier frame fill against this frame's book,
    /// never their own. A book that no longer offers the quantity at the
    /// decision price leaves the order unfilled; a market without a book in
    /// this frame (started, suspended, lost its consensus) is dropped.
    fn resolve_pending(
        &mut self,
        markets_by_id: &HashMap<&str, &UsMoneylineMarket>,
        books_by_slug: &HashMap<&str, &MarketBook>,
        now: DateTime<Utc>,
    ) {
        let pending = std::mem::take(&mut self.pending);
        for order in pending {
            if order.submitted_at >= now {
                self.pending.push(order);
                continue;
            }
            let row = &order.opportunity;
            let Some(market) = markets_by_id.get(row.market_id.as_str()) else {
                *self
                    .unfilled
                    .entry("no book in next frame".into())
                    .or_default() += 1;
                continue;
            };
            let Some(book) = books_by_slug.get(row.market_slug.as_str()) else {
                *self
                    .unfilled
                    .entry("no book in next frame".into())
                    .or_default() += 1;
                continue;
            };
            if market.start_time <= now {
                *self.unfilled.entry("market started".into()).or_default() += 1;
                continue;
            }
            let sized = size_position_from_levels(
                row.maximum_loss,
                market.fee_coefficient,
                market.minimum_quantity,
                row.executable_price,
                &execution_levels(book, row.side),
            );
            if sized.quantity <= Decimal::ZERO {
                *self
                    .unfilled
                    .entry("price moved through limit".into())
                    .or_default() += 1;
                continue;
            }
            if sized.quantity < row.quantity {
                self.partial_fills += 1;
            }
            let mut position = PaperPosition::from_opportunity(row, now);
            position.quantity = sized.quantity;
            position.entry_price = sized.average_side_price;
            position.estimated_fee = sized.estimated_fee;
            position.maximum_loss = sized.maximum_loss;
            if self
                .portfolio
                .open(position, self.options.settings.maximum_total_exposure)
            {
                self.filled += 1;
            } else {
                self.blocked_by_exposure += 1;
            }
        }
    }

    /// Settle open positions whose game started at least `settlement_lag`
    /// ago and whose market has a recorded settlement. Positions without one
    /// stay open and are reported as unsettled.
    fn settle_due(&mut self, now: DateTime<Utc>, outcomes: &HashMap<String, MarketOutcome>) {
        let lag = chrono::Duration::hours(self.options.settlement_lag_hours);
        let mut index = 0;
        while index < self.portfolio.open_positions.len() {
            let position = &self.portfolio.open_positions[index];
            let due = position.start_time.is_some_and(|start| start + lag <= now);
            let payout = outcomes
                .get(&position.market_id)
                .and_then(|outcome| outcome.payout(position.side));
            let (true, Some(payout)) = (due, payout) else {
                index += 1;
                continue;
            };
            let mut position = self.portfolio.open_positions.remove(index);
            let realized =
                position.quantity * (payout - position.entry_price) - position.estimated_fee;
            position.settlement_payout = Some(payout);
            position.realized_pnl = Some(realized);
            position.closed_at = Some(now);
            position.closing_price = outcomes
                .get(&position.market_id)
                .and_then(|outcome| outcome.closing_side_price(position.side));
            self.portfolio.realized_pnl += realized;
            self.portfolio.open_exposure -= position.maximum_loss;
            self.portfolio.bankroll = self.options.settings.bankroll + self.portfolio.realized_pnl;
            self.equity.push((now, self.portfolio.bankroll));
            self.portfolio.closed_positions.push(position);
        }
    }

    /// Settle what is due at `through` and build the report.
    pub fn finish(
        mut self,
        through: DateTime<Utc>,
        outcomes: &HashMap<String, MarketOutcome>,
    ) -> ReplayReport {
        self.settle_due(through, outcomes);
        for _order in std::mem::take(&mut self.pending) {
            *self.unfilled.entry("window ended".into()).or_default() += 1;
        }

        let graded = self
            .observations
            .values()
            .filter_map(|observation| {
                let outcome = outcomes.get(&observation.market_id)?;
                let payout = outcome.payout(observation.side)?;
                Some((observation, outcome, payout))
            })
            .collect::<Vec<_>>();
        let ungraded = self.observations.len() - graded.len();

        // Calibration uses the long side only: the short side is the mirror
        // image and would double-count every market.
        let long = graded
            .iter()
            .filter(|(observation, _, _)| observation.side == OutcomeSide::Long)
            .collect::<Vec<_>>();
        let consensus_pairs = long
            .iter()
            .map(|(observation, _, payout)| (observation.fair_probability, *payout))
            .collect::<Vec<_>>();
        let conservative_pairs = long
            .iter()
            .map(|(observation, _, payout)| (observation.conservative_probability, *payout))
            .collect::<Vec<_>>();
        let market_pairs = long
            .iter()
            .map(|(observation, _, payout)| (observation.market_probability, *payout))
            .collect::<Vec<_>>();
        let mut by_sport: BTreeMap<String, (Pairs, Pairs)> = BTreeMap::new();
        for (observation, _, payout) in &long {
            let entry = by_sport.entry(observation.sport.to_string()).or_default();
            entry.0.push((observation.fair_probability, *payout));
            entry.1.push((observation.market_probability, *payout));
        }
        let efficiency = Efficiency {
            graded_markets: long.len(),
            brier_consensus: metrics::brier(&consensus_pairs),
            brier_conservative: metrics::brier(&conservative_pairs),
            brier_polymarket: metrics::brier(&market_pairs),
            reliability_consensus: metrics::reliability(&consensus_pairs, 10),
            reliability_polymarket: metrics::reliability(&market_pairs, 10),
            by_sport: by_sport
                .into_iter()
                .map(|(sport, (consensus, market))| {
                    (
                        sport,
                        SportEfficiency {
                            graded_markets: consensus.len(),
                            brier_consensus: metrics::brier(&consensus),
                            brier_polymarket: metrics::brier(&market),
                        },
                    )
                })
                .collect(),
        };

        let edge_buckets = edge_buckets(&graded);

        let candidate_clv = graded
            .iter()
            .filter(|(observation, _, _)| {
                class_rank(observation.class) >= class_rank(self.options.open_class)
            })
            .filter_map(|(observation, outcome, _)| {
                outcome
                    .closing_side_price(observation.side)
                    .map(|closing| closing - observation.executable_price)
            })
            .collect::<Vec<_>>();
        let position_clv = self
            .portfolio
            .closed_positions
            .iter()
            .filter_map(PaperPosition::closing_line_value)
            .collect::<Vec<_>>();
        let position_returns = self
            .portfolio
            .closed_positions
            .iter()
            .filter_map(|position| position.realized_pnl)
            .collect::<Vec<_>>();
        let risked: Decimal = self
            .portfolio
            .closed_positions
            .iter()
            .map(|position| position.maximum_loss)
            .sum();
        let paper = Paper {
            submitted: self.submitted,
            filled: self.filled,
            partial_fills: self.partial_fills,
            unfilled: self.unfilled.clone(),
            blocked_by_exposure: self.blocked_by_exposure,
            settled: self.portfolio.closed_positions.len(),
            unsettled_open: self.portfolio.open_positions.len(),
            realized_pnl: self.portfolio.realized_pnl.round_dp(4),
            risked: risked.round_dp(4),
            roi_on_risk: (risked > Decimal::ZERO)
                .then(|| (self.portfolio.realized_pnl / risked).round_dp(4)),
            final_bankroll: self.portfolio.bankroll.round_dp(4),
            drawdown: metrics::max_drawdown(&self.equity),
            position_pnl: metrics::summary(&position_returns),
            closing_line_value: metrics::summary(&position_clv),
            equity: self.equity.clone(),
        };

        let mut limitations = vec![
            "News evidence is not replayable; classes are pre-news (Actionable here means \
             the numeric gates passed, which the live system would still send to review)."
                .to_string(),
            "Fills are assumed from displayed depth; no queue position, no partial-fill \
             timing, no maker fills."
                .to_string(),
            format!(
                "Settlement releases exposure {} h after scheduled start regardless of when \
                 the venue actually settled.",
                self.options.settlement_lag_hours
            ),
        ];
        if self.frames_stored > 0 {
            limitations.push(format!(
                "{} of {} frames were imported without books: their rows keep the class \
                 decided under the gates of that time and their market probability is the \
                 taker price, not the mid.",
                self.frames_stored, self.frames
            ));
        }
        if paper.unsettled_open > 0 {
            limitations.push(format!(
                "{} positions never settled inside the window; realized P&L excludes them.",
                paper.unsettled_open
            ));
        }

        ReplayReport {
            generated_at: Utc::now(),
            production_approved: false,
            frames: self.frames,
            frames_recomputed: self.frames_recomputed,
            frames_stored: self.frames_stored,
            first_input_at: self.first_input_at,
            last_input_at: self.last_input_at,
            options: self.options,
            rows_by_class: self.rows_by_class,
            observations: self.observations.len(),
            graded_observations: graded.len(),
            ungraded_observations: ungraded,
            efficiency,
            edge_buckets,
            candidate_closing_line_value: metrics::summary(&candidate_clv),
            paper,
            limitations,
        }
    }
}

fn class_text(class: RecommendationClass) -> String {
    match class {
        RecommendationClass::Actionable => "actionable",
        RecommendationClass::Watchlist => "watchlist",
        RecommendationClass::Rejected => "rejected",
    }
    .to_string()
}

fn fee_per_contract(row: &Opportunity) -> Decimal {
    if row.quantity > Decimal::ZERO {
        row.estimated_fee / row.quantity
    } else {
        Decimal::ZERO
    }
}

/// Top-of-book mid as the venue's implied probability for `side`; `None`
/// when either side of the book is empty.
fn side_mid(book: &MarketBook, side: OutcomeSide) -> Option<Decimal> {
    let bid = book.bids.first()?.yes_price;
    let ask = book.offers.first()?.yes_price;
    let mid = (bid + ask) / Decimal::TWO;
    Some(match side {
        OutcomeSide::Long => mid,
        OutcomeSide::Short => Decimal::ONE - mid,
    })
}

fn execution_levels(book: &MarketBook, side: OutcomeSide) -> Vec<ExecutionLevel> {
    match side {
        OutcomeSide::Long => book
            .offers
            .iter()
            .map(|level| ExecutionLevel {
                side_price: level.yes_price,
                yes_price: level.yes_price,
                quantity: level.quantity,
            })
            .collect(),
        OutcomeSide::Short => book
            .bids
            .iter()
            .map(|level| ExecutionLevel {
                side_price: Decimal::ONE - level.yes_price,
                yes_price: level.yes_price,
                quantity: level.quantity,
            })
            .collect(),
    }
}

/// Realized return per contract of the last observation of every graded
/// side, grouped by raw edge. If the edge is real, higher buckets earn more.
fn edge_buckets(graded: &[(&Observation, &MarketOutcome, Decimal)]) -> Vec<EdgeBucket> {
    let bounds: [(Option<Decimal>, Option<Decimal>); 5] = [
        (None, Some(Decimal::ZERO)),
        (Some(Decimal::ZERO), Some(Decimal::new(2, 2))),
        (Some(Decimal::new(2, 2)), Some(Decimal::new(5, 2))),
        (Some(Decimal::new(5, 2)), Some(Decimal::new(10, 2))),
        (Some(Decimal::new(10, 2)), None),
    ];
    bounds
        .iter()
        .filter_map(|(lower, upper)| {
            let returns = graded
                .iter()
                .filter(|(observation, _, _)| {
                    lower.is_none_or(|lower| observation.raw_edge >= lower)
                        && upper.is_none_or(|upper| observation.raw_edge < upper)
                })
                .map(|(observation, _, payout)| {
                    payout - observation.executable_price - observation.fee_per_contract
                })
                .collect::<Vec<_>>();
            metrics::summary(&returns).map(|returns| EdgeBucket {
                lower: *lower,
                upper: *upper,
                return_per_contract: returns,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Efficiency {
    /// Settled markets with a long-side observation.
    pub graded_markets: usize,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub brier_consensus: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub brier_conservative: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub brier_polymarket: Option<Decimal>,
    pub reliability_consensus: Vec<ReliabilityBucket>,
    pub reliability_polymarket: Vec<ReliabilityBucket>,
    pub by_sport: BTreeMap<String, SportEfficiency>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SportEfficiency {
    pub graded_markets: usize,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub brier_consensus: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub brier_polymarket: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeBucket {
    #[serde(with = "rust_decimal::serde::str_option")]
    pub lower: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub upper: Option<Decimal>,
    pub return_per_contract: Summary,
}

#[derive(Debug, Clone, Serialize)]
pub struct Paper {
    pub submitted: usize,
    pub filled: usize,
    pub partial_fills: usize,
    pub unfilled: BTreeMap<String, usize>,
    pub blocked_by_exposure: usize,
    pub settled: usize,
    pub unsettled_open: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub risked: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub roi_on_risk: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub final_bankroll: Decimal,
    pub drawdown: Drawdown,
    pub position_pnl: Option<Summary>,
    pub closing_line_value: Option<Summary>,
    pub equity: Vec<(DateTime<Utc>, Decimal)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub generated_at: DateTime<Utc>,
    /// Always false: a backtest is evidence for research, not approval.
    pub production_approved: bool,
    pub frames: usize,
    pub frames_recomputed: usize,
    pub frames_stored: usize,
    pub first_input_at: Option<DateTime<Utc>>,
    pub last_input_at: Option<DateTime<Utc>>,
    pub options: ReplayOptions,
    pub rows_by_class: BTreeMap<String, usize>,
    /// Distinct market sides observed.
    pub observations: usize,
    pub graded_observations: usize,
    pub ungraded_observations: usize,
    pub efficiency: Efficiency,
    pub edge_buckets: Vec<EdgeBucket>,
    /// Closing side price minus executable price for the last observation of
    /// every side at or above `open_class`, whether or not it was opened.
    pub candidate_closing_line_value: Option<Summary>,
    pub paper: Paper,
    pub limitations: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::{BookLevel, MarketParticipant};

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    fn t(minutes: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + minutes * 60, 0).unwrap()
    }

    fn market(start: DateTime<Utc>) -> UsMoneylineMarket {
        UsMoneylineMarket {
            event_id: "event".into(),
            game_id: None,
            sportradar_game_id: None,
            sport: Sport::Nba,
            start_time: start,
            market_id: "market".into(),
            market_slug: "slug".into(),
            description: "full game winner".into(),
            sports_market_type: "moneyline".into(),
            long_participant: MarketParticipant {
                side_id: "a".into(),
                name: "Home".into(),
                long: true,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            short_participant: MarketParticipant {
                side_id: "b".into(),
                name: "Away".into(),
                long: false,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            tick_size: d("0.01"),
            minimum_quantity: d("0.01"),
            fee_coefficient: d("0.06"),
            ep3_status: "OPEN".into(),
            ep3_synced_at: None,
        }
    }

    fn book(ask: &str, at: DateTime<Utc>) -> MarketBook {
        MarketBook {
            market_slug: "slug".into(),
            bids: vec![BookLevel {
                yes_price: d(ask) - d("0.01"),
                quantity: d("1000"),
            }],
            offers: vec![BookLevel {
                yes_price: d(ask),
                quantity: d("1000"),
            }],
            state: "MARKET_STATE_OPEN".into(),
            transact_time: at,
            fetched_at: at,
        }
    }

    fn quote(
        id: &str,
        family: crate::domain::SourceFamily,
        at: DateTime<Utc>,
        start: DateTime<Utc>,
    ) -> SourceQuote {
        SourceQuote {
            source_id: id.into(),
            family,
            sport: Sport::Nba,
            event_id: "e".into(),
            participant_a: "Home".into(),
            participant_b: "Away".into(),
            participant_a_provider_ids: BTreeMap::new(),
            participant_b_provider_ids: BTreeMap::new(),
            start_time: start,
            start_time_tolerance_minutes: 15,
            // Fair ~0.667 for Home.
            decimal_odds_a: d("1.45"),
            decimal_odds_b: d("2.90"),
            decimal_odds_neutral: None,
            source_timestamp: at,
            fetched_at: at,
            parser_version: "test".into(),
            validation_only: false,
        }
    }

    fn frame(at: DateTime<Utc>, start: DateTime<Utc>, ask: &str) -> Frame {
        use crate::domain::SourceFamily::*;
        let quotes = [
            ("pinnacle", Pinnacle),
            ("circa", Circa),
            ("fanduel", FanDuel),
            ("draftkings", DraftKings),
            ("betmgm", BetMgm),
        ]
        .into_iter()
        .map(|(id, family)| quote(id, family, at, start))
        .collect();
        Frame {
            scan_id: Uuid::new_v4(),
            evaluated_at: at,
            origin: "live".into(),
            settings: serde_json::Value::Null,
            portfolio: PaperPortfolio::new(Decimal::ONE_HUNDRED),
            markets: vec![market(start)],
            books: vec![book(ask, at)],
            quotes,
            opportunities: Vec::new(),
        }
    }

    fn options(fill: FillModel) -> ReplayOptions {
        ReplayOptions {
            settings: Settings {
                minimum_position_fraction: Decimal::ZERO,
                ..Settings::default()
            },
            open_class: RecommendationClass::Actionable,
            fill,
            settlement_lag_hours: 4,
        }
    }

    fn settled(payout: Decimal) -> HashMap<String, MarketOutcome> {
        HashMap::from([(
            "market".to_string(),
            MarketOutcome {
                market_id: "market".into(),
                start_time: t(120),
                closing_long: Some(d("0.60")),
                closing_short: Some(d("0.40")),
                closing_observed_at: None,
                settlement: Some(payout),
                settled_recorded_at: None,
            },
        )])
    }

    #[test]
    fn same_book_opens_settles_and_grades() {
        let outcomes = settled(Decimal::ONE);
        let mut replay = Replay::new(options(FillModel::SameBook));
        replay.step(&frame(t(0), t(120), "0.55"), &outcomes);
        let report = replay.finish(t(120 + 5 * 60), &outcomes);
        assert_eq!(report.frames_recomputed, 1);
        assert_eq!(report.rows_by_class.get("actionable"), Some(&1));
        assert_eq!(report.paper.filled, 1);
        assert_eq!(report.paper.settled, 1);
        assert!(report.paper.realized_pnl > Decimal::ZERO, "{report:?}");
        assert_eq!(report.efficiency.graded_markets, 1);
        assert_eq!(report.paper.closing_line_value.as_ref().unwrap().count, 1);
        // Entered at 0.55, closed at 0.60: five cents of closing-line value.
        assert_eq!(
            report.paper.closing_line_value.as_ref().unwrap().mean,
            d("0.05")
        );
    }

    #[test]
    fn next_book_fills_only_at_or_below_the_decision_price() {
        let outcomes = settled(Decimal::ONE);
        let mut replay = Replay::new(options(FillModel::NextBook));
        replay.step(&frame(t(0), t(120), "0.55"), &outcomes);
        assert_eq!(replay.pending.len(), 1, "order waits for the next book");
        // Price moved up: no fill.
        replay.step(&frame(t(5), t(120), "0.58"), &outcomes);
        assert_eq!(replay.filled, 0);
        assert_eq!(replay.unfilled.get("price moved through limit"), Some(&1));
        // The row re-qualifies at 0.58 and is resubmitted; the next book at
        // 0.57 is at or below that decision price, so it fills.
        replay.step(&frame(t(10), t(120), "0.57"), &outcomes);
        assert_eq!(replay.filled, 1);
        let report = replay.finish(t(600), &outcomes);
        assert_eq!(report.paper.settled, 1);
    }

    #[test]
    fn unsettled_positions_stay_open_and_are_reported() {
        let outcomes = HashMap::new();
        let mut replay = Replay::new(options(FillModel::SameBook));
        replay.step(&frame(t(0), t(120), "0.55"), &outcomes);
        let report = replay.finish(t(600), &outcomes);
        assert_eq!(report.paper.unsettled_open, 1);
        assert_eq!(report.paper.settled, 0);
        assert_eq!(report.graded_observations, 0);
        assert!(
            report
                .limitations
                .iter()
                .any(|l| l.contains("never settled"))
        );
    }
}
