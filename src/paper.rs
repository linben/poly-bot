//! Paper-position workflow shared by the `paper` CLI and the terminal UI.
//! Opening requires an actionable, news-reviewed opportunity from the latest
//! scan; closing recomputes exposure from the remaining positions.

use chrono::Utc;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::{
    Error, Result,
    config::Settings,
    domain::{PaperPortfolio, PaperPosition, RecommendationClass, ResearchOpportunity},
    storage::Store,
};

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
    let position = PaperPosition {
        opportunity_id: research.opportunity.id,
        event_id: research.opportunity.event_id,
        market_id: research.opportunity.market_id,
        side: research.opportunity.side,
        maximum_loss: research.opportunity.maximum_loss,
        opened_at: Utc::now(),
    };
    if !portfolio.open(position, settings.maximum_total_exposure) {
        return Err(Error::InvalidData(
            "paper position violates bankroll or exposure limits".into(),
        ));
    }
    store.save_portfolio(&portfolio).await?;
    Ok(portfolio)
}

pub async fn close_position(
    store: &dyn Store,
    settings: &Settings,
    opportunity_id: Uuid,
) -> Result<PaperPortfolio> {
    let mut portfolio = store.load_portfolio(settings.bankroll).await?;
    let before = portfolio.open_positions.len();
    portfolio
        .open_positions
        .retain(|position| position.opportunity_id != opportunity_id);
    if portfolio.open_positions.len() == before {
        return Err(Error::InvalidData("paper position was not found".into()));
    }
    portfolio.open_exposure = portfolio
        .open_positions
        .iter()
        .map(|position| position.maximum_loss)
        .sum::<Decimal>();
    store.save_portfolio(&portfolio).await?;
    Ok(portfolio)
}
