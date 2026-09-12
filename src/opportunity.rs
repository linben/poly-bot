use chrono::Utc;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::{
    config::Settings,
    domain::{
        ConsensusPrice, MarketBook, Opportunity, OutcomeSide, PaperPortfolio, RecommendationClass,
        UsMoneylineMarket,
    },
    risk::{ExecutionLevel, exact_fee, quarter_kelly_fraction, size_position_from_levels},
};

pub struct OpportunityEngine {
    settings: Settings,
}

impl OpportunityEngine {
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }

    pub fn evaluate(
        &self,
        market: &UsMoneylineMarket,
        book: &MarketBook,
        consensus: &ConsensusPrice,
        portfolio: &PaperPortfolio,
    ) -> Vec<Opportunity> {
        vec![
            self.evaluate_side(market, book, consensus, portfolio, OutcomeSide::Long),
            self.evaluate_side(market, book, consensus, portfolio, OutcomeSide::Short),
        ]
    }

    fn evaluate_side(
        &self,
        market: &UsMoneylineMarket,
        book: &MarketBook,
        consensus: &ConsensusPrice,
        portfolio: &PaperPortfolio,
        side: OutcomeSide,
    ) -> Opportunity {
        let mut reasons = Vec::new();
        let (participant, fair, dispersion, top_price, top_yes_price, levels, maker_price) =
            match side {
                OutcomeSide::Long => {
                    let offer = book.offers.first();
                    let maker = maker_long(book, market.tick_size);
                    (
                        market.long_participant.name.clone(),
                        consensus.probability_a,
                        consensus.dispersion_a,
                        offer.map(|level| level.yes_price).unwrap_or(Decimal::ONE),
                        offer.map(|level| level.yes_price).unwrap_or(Decimal::ONE),
                        book.offers
                            .iter()
                            .map(|level| ExecutionLevel {
                                side_price: level.yes_price,
                                yes_price: level.yes_price,
                                quantity: level.quantity,
                            })
                            .collect::<Vec<_>>(),
                        maker,
                    )
                }
                OutcomeSide::Short => {
                    let bid = book.bids.first();
                    let yes_price = bid.map(|level| level.yes_price).unwrap_or(Decimal::ZERO);
                    let maker = maker_short(book, market.tick_size);
                    (
                        market.short_participant.name.clone(),
                        consensus.probability_b,
                        consensus.dispersion_a,
                        Decimal::ONE - yes_price,
                        yes_price,
                        book.bids
                            .iter()
                            .map(|level| ExecutionLevel {
                                side_price: Decimal::ONE - level.yes_price,
                                yes_price: level.yes_price,
                                quantity: level.quantity,
                            })
                            .collect::<Vec<_>>(),
                        maker,
                    )
                }
            };

        if book.state != "MARKET_STATE_OPEN" {
            reasons.push(format!("book state is {}", book.state));
        }
        if market.ep3_status != "OPEN" {
            reasons.push(format!("market status is {}", market.ep3_status));
        }
        let book_age = Utc::now()
            .signed_duration_since(book.fetched_at)
            .num_seconds();
        if book_age < 0 || book_age > self.settings.book_max_age.as_secs() as i64 {
            reasons.push(format!("book snapshot is {book_age}s old"));
        }
        if book.transact_time > Utc::now() + chrono::Duration::minutes(1) {
            reasons.push("book transact time is in the future".into());
        }
        if top_price < self.settings.minimum_price || top_price > self.settings.maximum_price {
            reasons.push(format!(
                "executable price {} is outside {}-{}",
                top_price.round_dp(4),
                self.settings.minimum_price,
                self.settings.maximum_price
            ));
        }
        if consensus.family_count < self.settings.watchlist_source_families {
            reasons.push(format!(
                "fewer than {} independent source families",
                self.settings.watchlist_source_families
            ));
        }
        if portfolio.has_market_side(&market.market_id, side) {
            reasons.push("paper portfolio already has this market side".into());
        }

        let conservative = (fair - dispersion).max(Decimal::ZERO);
        let exact_fee_per_contract = exact_fee(market.fee_coefficient, Decimal::ONE, top_yes_price);

        let kelly = quarter_kelly_fraction(
            conservative,
            top_price + exact_fee_per_contract,
            self.settings.kelly_fraction,
        )
        .min(self.settings.maximum_position_fraction);
        if kelly < Decimal::new(1, 2) {
            reasons.push("quarter-Kelly size is below one percent".into());
        }
        let event_exposure = portfolio.exposure_for_event(&market.event_id);
        let portfolio_remaining =
            (self.settings.maximum_total_exposure - portfolio.open_exposure).max(Decimal::ZERO);
        let event_remaining =
            (self.settings.maximum_total_exposure - event_exposure).max(Decimal::ZERO);
        let exposure_remaining = portfolio_remaining.min(event_remaining);
        let risk_budget = (self.settings.bankroll * kelly).min(exposure_remaining);
        let sized = size_position_from_levels(
            risk_budget,
            market.fee_coefficient,
            market.minimum_quantity,
            self.settings.maximum_price,
            &levels,
        );
        if sized.quantity < market.minimum_quantity {
            reasons.push("insufficient eligible book depth".into());
        }
        if sized.maximum_loss < self.settings.bankroll * Decimal::new(1, 2) {
            reasons.push("depth-limited maximum loss is below one percent".into());
        }
        let executable = if sized.quantity > Decimal::ZERO {
            sized.average_side_price
        } else {
            top_price
        };
        if executable < self.settings.minimum_price || executable > self.settings.maximum_price {
            reasons.push(format!(
                "depth-weighted price {} is outside {}-{}",
                executable.round_dp(4),
                self.settings.minimum_price,
                self.settings.maximum_price
            ));
        }
        let fee_per_contract = if sized.quantity > Decimal::ZERO {
            sized.estimated_fee / sized.quantity
        } else {
            exact_fee_per_contract
        };
        // Six places is far below any tick or fee; keeps records legible.
        let raw_edge = (fair - executable).round_dp(6);
        let net_edge = (conservative - executable - fee_per_contract).round_dp(6);
        if raw_edge < self.settings.minimum_raw_edge {
            reasons.push(format!(
                "raw edge {raw_edge} is below {}",
                self.settings.minimum_raw_edge
            ));
        }
        if net_edge < self.settings.minimum_net_edge {
            reasons.push(format!(
                "conservative after-fee edge {net_edge} is below {}",
                self.settings.minimum_net_edge
            ));
        }

        let hard_rejection = !reasons.is_empty();
        let class = if hard_rejection {
            RecommendationClass::Rejected
        } else if consensus.family_count >= self.settings.minimum_source_families
            && consensus.has_reference
        {
            RecommendationClass::Actionable
        } else {
            RecommendationClass::Watchlist
        };
        if class == RecommendationClass::Watchlist {
            if consensus.family_count < self.settings.minimum_source_families {
                reasons.push(format!(
                    "requires {} independent source families",
                    self.settings.minimum_source_families
                ));
            }
            if !consensus.has_reference {
                reasons.push("requires a reference sportsbook".into());
            }
        }

        Opportunity {
            id: Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("polybot:{}:{side:?}", market.market_id).as_bytes(),
            ),
            generated_at: Utc::now(),
            class,
            sport: market.sport,
            event_id: market.event_id.clone(),
            market_id: market.market_id.clone(),
            market_slug: market.market_slug.clone(),
            participant,
            side,
            fair_probability: fair.round_dp(6),
            conservative_probability: conservative.round_dp(6),
            executable_price: executable.round_dp(6),
            maker_price,
            raw_edge,
            net_edge,
            quantity: sized.quantity,
            maximum_loss: sized.maximum_loss,
            estimated_fee: sized.estimated_fee,
            source_count: consensus.source_count,
            family_count: consensus.family_count,
            source_ids: consensus.source_ids.clone(),
            book_time: book.transact_time,
            reasons,
        }
    }
}

fn maker_long(book: &MarketBook, tick: Decimal) -> Option<Decimal> {
    let bid = book.bids.first()?.yes_price;
    let ask = book.offers.first()?.yes_price;
    let price = bid + tick;
    (price < ask).then_some(price)
}

fn maker_short(book: &MarketBook, tick: Decimal) -> Option<Decimal> {
    let bid = book.bids.first()?.yes_price;
    let ask = book.offers.first()?.yes_price;
    let yes_sell_price = ask - tick;
    (yes_sell_price > bid).then_some(Decimal::ONE - yes_sell_price)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::{BookLevel, MarketParticipant, Sport};

    fn market() -> UsMoneylineMarket {
        UsMoneylineMarket {
            event_id: "event".into(),
            game_id: None,
            sportradar_game_id: None,
            sport: Sport::Nba,
            start_time: Utc::now() + chrono::Duration::hours(2),
            market_id: "market".into(),
            market_slug: "market".into(),
            description: "full game winner".into(),
            sports_market_type: "basketball_team_full_game_winner".into(),
            long_participant: MarketParticipant {
                side_id: "a".into(),
                name: "Team A".into(),
                long: true,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            short_participant: MarketParticipant {
                side_id: "b".into(),
                name: "Team B".into(),
                long: false,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            tick_size: Decimal::new(1, 2),
            minimum_quantity: Decimal::ONE,
            fee_coefficient: Decimal::ZERO,
            ep3_status: "OPEN".into(),
            ep3_synced_at: None,
        }
    }

    fn book() -> MarketBook {
        MarketBook {
            market_slug: "market".into(),
            bids: vec![BookLevel {
                yes_price: Decimal::new(55, 2),
                quantity: Decimal::new(20, 0),
            }],
            offers: vec![BookLevel {
                yes_price: Decimal::new(57, 2),
                quantity: Decimal::new(20, 0),
            }],
            state: "MARKET_STATE_OPEN".into(),
            transact_time: Utc::now(),
            fetched_at: Utc::now(),
        }
    }

    fn consensus() -> ConsensusPrice {
        ConsensusPrice {
            probability_a: Decimal::new(30, 2),
            probability_b: Decimal::new(70, 2),
            probability_neutral: Decimal::ZERO,
            dispersion_a: Decimal::ZERO,
            source_count: 5,
            family_count: 5,
            has_reference: true,
            source_ids: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            newest_source_timestamp: Utc::now(),
        }
    }

    fn portfolio() -> PaperPortfolio {
        PaperPortfolio {
            bankroll: Decimal::ONE_HUNDRED,
            open_exposure: Decimal::ZERO,
            open_positions: Vec::new(),
        }
    }

    #[test]
    fn short_side_uses_one_minus_yes_bid() {
        let opportunities = OpportunityEngine::new(Settings::default()).evaluate(
            &market(),
            &book(),
            &consensus(),
            &portfolio(),
        );
        let short = opportunities
            .iter()
            .find(|item| item.side == OutcomeSide::Short)
            .unwrap();
        assert_eq!(short.executable_price, Decimal::new(45, 2));
        assert_eq!(short.maker_price, Some(Decimal::new(44, 2)));
        assert_eq!(short.class, RecommendationClass::Actionable);
    }

    #[test]
    fn stale_or_suspended_book_is_rejected() {
        let mut book = book();
        book.state = "MARKET_STATE_SUSPENDED".into();
        book.fetched_at = Utc::now() - chrono::Duration::minutes(2);
        let opportunities = OpportunityEngine::new(Settings::default()).evaluate(
            &market(),
            &book,
            &consensus(),
            &portfolio(),
        );
        assert!(
            opportunities
                .iter()
                .all(|item| item.class == RecommendationClass::Rejected)
        );
        assert!(opportunities.iter().any(|item| {
            item.reasons
                .iter()
                .any(|reason| reason.contains("SUSPENDED"))
        }));
    }
}
