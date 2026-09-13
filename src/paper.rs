//! Paper-position workflow shared by the `paper` CLI, the local engine loop,
//! and the terminal UI. Opening requires an actionable, news-reviewed
//! opportunity from the latest scan; closing archives the position without a
//! result; settlement grades started positions against the venue's closing
//! line and settlement price.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    Error, Result,
    config::Settings,
    domain::{
        OutcomeSide, PaperPortfolio, PaperPosition, RecommendationClass, ResearchOpportunity,
    },
    polymarket::PolymarketUsClient,
    storage::Store,
};

/// What one settlement pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SettlementReport {
    pub closing_lines_recorded: usize,
    pub settled: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl: Decimal,
}

pub async fn open_position(
    store: &dyn Store,
    settings: &Settings,
    opportunity_id: Uuid,
) -> Result<PaperPortfolio> {
    let mut portfolio = store.load_portfolio(settings.bankroll).await?;
    let opportunity = store
        .latest_opportunities()
        .await?
        .into_iter()
        .find(|item| item.id == opportunity_id)
        .ok_or_else(|| Error::InvalidData("opportunity is not in the latest scan".into()))?;
    let news = store.news_for(opportunity.id).await?;
    let research = ResearchOpportunity::new(opportunity, news);
    if research.effective_class != RecommendationClass::Actionable {
        return Err(Error::InvalidData(
            "paper position requires an actionable, news-reviewed opportunity".into(),
        ));
    }
    if portfolio.has_market_side(&research.opportunity.market_id, research.opportunity.side) {
        return Err(Error::InvalidData(
            "paper portfolio already contains this market side".into(),
        ));
    }
    let position = PaperPosition::from_opportunity(&research.opportunity, Utc::now());
    if !portfolio.open(position, settings.maximum_total_exposure) {
        return Err(Error::InvalidData(
            "paper position violates bankroll or exposure limits".into(),
        ));
    }
    store.save_portfolio(&portfolio).await?;
    Ok(portfolio)
}

/// Manual close: the position is archived with no result (`realized_pnl`
/// stays `None`) so the record survives for review.
pub async fn close_position(
    store: &dyn Store,
    settings: &Settings,
    opportunity_id: Uuid,
) -> Result<PaperPortfolio> {
    let mut portfolio = store.load_portfolio(settings.bankroll).await?;
    let index = portfolio
        .open_positions
        .iter()
        .position(|position| position.opportunity_id == opportunity_id)
        .ok_or_else(|| Error::InvalidData("paper position was not found".into()))?;
    let mut position = portfolio.open_positions.remove(index);
    position.closed_at = Some(Utc::now());
    portfolio.closed_positions.push(position);
    portfolio.open_exposure = open_exposure(&portfolio);
    store.save_portfolio(&portfolio).await?;
    Ok(portfolio)
}

/// Grade every open position whose game has started: record the venue's
/// pre-start closing line once, then settle when the market has resolved.
/// Gateway errors for one position are logged and skipped so a single bad
/// market never blocks the others; the portfolio is saved only on change.
pub async fn settle_positions(
    store: &dyn Store,
    settings: &Settings,
    client: &PolymarketUsClient,
) -> Result<SettlementReport> {
    let mut portfolio = store.load_portfolio(settings.bankroll).await?;
    let now = Utc::now();
    let mut report = SettlementReport::default();
    let mut changed = false;
    let mut index = 0;
    while index < portfolio.open_positions.len() {
        let position = &mut portfolio.open_positions[index];
        let Some(start_time) = position
            .start_time
            .filter(|_| !position.market_slug.is_empty())
        else {
            warn!(
                opportunity = %position.opportunity_id,
                market = %position.market_id,
                "paper position lacks a start time or market slug; settle it manually"
            );
            index += 1;
            continue;
        };
        if start_time > now {
            index += 1;
            continue;
        }
        if position.closing_price.is_none() {
            match client
                .fetch_closing_price(&position.market_slug, start_time)
                .await
            {
                Ok(Some(closing)) => {
                    position.closing_price = Some(side_price(
                        position.side,
                        closing.long_price,
                        closing.short_price,
                    ));
                    report.closing_lines_recorded += 1;
                    changed = true;
                }
                Ok(None) => debug!(
                    market = %position.market_slug,
                    "no price history yet for closing line"
                ),
                Err(error) => warn!(
                    %error,
                    market = %position.market_slug,
                    "fetching closing line failed"
                ),
            }
        }
        let settlement = match client.fetch_settlement(&position.market_slug).await {
            Ok(settlement) => settlement,
            Err(error) => {
                warn!(
                    %error,
                    market = %position.market_slug,
                    "fetching settlement failed"
                );
                None
            }
        };
        let Some(settlement) = settlement else {
            index += 1;
            continue;
        };
        let mut position = portfolio.open_positions.remove(index);
        let realized = settle(&mut position, settlement, now);
        portfolio.realized_pnl += realized;
        report.realized_pnl += realized;
        report.settled += 1;
        changed = true;
        portfolio.closed_positions.push(position);
    }
    if changed {
        portfolio.open_exposure = open_exposure(&portfolio);
        portfolio.bankroll = settings.bankroll + portfolio.realized_pnl;
        store.save_portfolio(&portfolio).await?;
    }
    Ok(report)
}

/// Mark a position settled at the YES payout `settlement` and return its
/// realized P&L: `quantity * (payout - entry_price) - estimated_fee`.
fn settle(position: &mut PaperPosition, settlement: Decimal, closed_at: DateTime<Utc>) -> Decimal {
    let payout = side_price(position.side, settlement, Decimal::ONE - settlement);
    let realized = position.quantity * (payout - position.entry_price) - position.estimated_fee;
    position.settlement_payout = Some(payout);
    position.realized_pnl = Some(realized);
    position.closed_at = Some(closed_at);
    realized
}

fn side_price(side: OutcomeSide, long: Decimal, short: Decimal) -> Decimal {
    match side {
        OutcomeSide::Long => long,
        OutcomeSide::Short => short,
    }
}

fn open_exposure(portfolio: &PaperPortfolio) -> Decimal {
    portfolio
        .open_positions
        .iter()
        .map(|position| position.maximum_loss)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(side: OutcomeSide) -> PaperPosition {
        PaperPosition {
            opportunity_id: Uuid::new_v4(),
            event_id: "event".into(),
            market_id: "market".into(),
            market_slug: "slug".into(),
            side,
            quantity: Decimal::new(10, 0),
            entry_price: Decimal::new(40, 2),
            fair_probability: Decimal::new(45, 2),
            estimated_fee: Decimal::new(5, 2),
            maximum_loss: Decimal::new(405, 2),
            start_time: Some(Utc::now()),
            opened_at: Utc::now(),
            closing_price: None,
            settlement_payout: None,
            realized_pnl: None,
            closed_at: None,
        }
    }

    #[test]
    fn long_settlement_pays_yes_and_short_pays_complement() {
        let now = Utc::now();
        let mut long = position(OutcomeSide::Long);
        // 10 contracts at 0.40 win: 10 * (1 - 0.40) - 0.05 = 5.95.
        assert_eq!(settle(&mut long, Decimal::ONE, now), Decimal::new(595, 2));
        assert_eq!(long.settlement_payout, Some(Decimal::ONE));
        assert_eq!(long.closed_at, Some(now));

        let mut short = position(OutcomeSide::Short);
        // YES settled 1 means the short loses its stake: 10 * (0 - 0.40) - 0.05.
        assert_eq!(settle(&mut short, Decimal::ONE, now), Decimal::new(-405, 2));
        assert_eq!(short.settlement_payout, Some(Decimal::ZERO));
        assert_eq!(short.realized_pnl, Some(Decimal::new(-405, 2)));
    }
}
