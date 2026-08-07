use rust_decimal::{Decimal, RoundingStrategy};

use crate::domain::{OutcomeSide, PaperPortfolio, PaperPosition};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutionLevel {
    pub side_price: Decimal,
    pub yes_price: Decimal,
    pub quantity: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizedPosition {
    pub quantity: Decimal,
    pub average_side_price: Decimal,
    pub maximum_loss: Decimal,
    pub estimated_fee: Decimal,
}

impl SizedPosition {
    fn empty() -> Self {
        Self {
            quantity: Decimal::ZERO,
            average_side_price: Decimal::ZERO,
            maximum_loss: Decimal::ZERO,
            estimated_fee: Decimal::ZERO,
        }
    }
}

pub fn exact_fee(coefficient: Decimal, quantity: Decimal, yes_trade_price: Decimal) -> Decimal {
    coefficient * quantity * yes_trade_price * (Decimal::ONE - yes_trade_price)
}

pub fn rounded_fee(coefficient: Decimal, quantity: Decimal, yes_trade_price: Decimal) -> Decimal {
    exact_fee(coefficient, quantity, yes_trade_price)
        .round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven)
}

pub fn quarter_kelly_fraction(
    fair_probability: Decimal,
    effective_cost: Decimal,
    fraction: Decimal,
) -> Decimal {
    if fair_probability <= effective_cost || effective_cost >= Decimal::ONE {
        return Decimal::ZERO;
    }
    (((fair_probability - effective_cost) / (Decimal::ONE - effective_cost)) * fraction)
        .clamp(Decimal::ZERO, Decimal::ONE)
}

pub fn floor_to_increment(value: Decimal, increment: Decimal) -> Decimal {
    if increment <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (value / increment).floor() * increment
}

pub fn size_position(
    risk_budget: Decimal,
    side_cost: Decimal,
    yes_trade_price: Decimal,
    fee_coefficient: Decimal,
    minimum_quantity: Decimal,
    available_depth: Decimal,
) -> (Decimal, Decimal, Decimal) {
    let result = size_position_from_levels(
        risk_budget,
        fee_coefficient,
        minimum_quantity,
        Decimal::ONE,
        &[ExecutionLevel {
            side_price: side_cost,
            yes_price: yes_trade_price,
            quantity: available_depth,
        }],
    );
    (result.quantity, result.maximum_loss, result.estimated_fee)
}

pub fn size_position_from_levels(
    risk_budget: Decimal,
    fee_coefficient: Decimal,
    minimum_quantity: Decimal,
    maximum_side_price: Decimal,
    levels: &[ExecutionLevel],
) -> SizedPosition {
    if risk_budget <= Decimal::ZERO
        || minimum_quantity <= Decimal::ZERO
        || maximum_side_price <= Decimal::ZERO
    {
        return SizedPosition::empty();
    }

    let mut quantity = Decimal::ZERO;
    let mut gross_cost = Decimal::ZERO;
    let mut estimated_fee = Decimal::ZERO;
    for level in levels.iter().filter(|level| {
        level.side_price > Decimal::ZERO
            && level.side_price <= maximum_side_price
            && level.yes_price > Decimal::ZERO
            && level.yes_price < Decimal::ONE
            && level.quantity > Decimal::ZERO
    }) {
        let available = floor_to_increment(level.quantity, minimum_quantity);
        let unit_cost =
            level.side_price + exact_fee(fee_coefficient, Decimal::ONE, level.yes_price);
        let remaining_budget = risk_budget - gross_cost - estimated_fee;
        if available <= Decimal::ZERO
            || unit_cost <= Decimal::ZERO
            || remaining_budget <= Decimal::ZERO
        {
            continue;
        }
        let mut take = floor_to_increment(
            available.min(remaining_budget / unit_cost),
            minimum_quantity,
        );
        while take > Decimal::ZERO {
            let level_fee = rounded_fee(fee_coefficient, take, level.yes_price);
            let candidate_loss = gross_cost + take * level.side_price + estimated_fee + level_fee;
            if candidate_loss <= risk_budget {
                gross_cost += take * level.side_price;
                estimated_fee += level_fee;
                quantity += take;
                break;
            }
            take -= minimum_quantity;
        }
    }

    if quantity <= Decimal::ZERO {
        return SizedPosition::empty();
    }
    SizedPosition {
        quantity,
        average_side_price: gross_cost / quantity,
        maximum_loss: gross_cost + estimated_fee,
        estimated_fee,
    }
}

impl PaperPortfolio {
    pub fn can_open(&self, maximum_loss: Decimal, maximum_total_exposure: Decimal) -> bool {
        maximum_loss > Decimal::ZERO
            && self.open_exposure + maximum_loss <= maximum_total_exposure
            && maximum_loss <= self.bankroll
    }

    pub fn open(&mut self, position: PaperPosition, maximum_total_exposure: Decimal) -> bool {
        if !self.can_open(position.maximum_loss, maximum_total_exposure) {
            return false;
        }
        self.open_exposure += position.maximum_loss;
        self.open_positions.push(position);
        true
    }

    pub fn exposure_for_event(&self, event_id: &str) -> Decimal {
        self.open_positions
            .iter()
            .filter(|position| position.event_id == event_id)
            .map(|position| position.maximum_loss)
            .sum()
    }

    pub fn has_market_side(&self, market_id: &str, side: OutcomeSide) -> bool {
        self.open_positions
            .iter()
            .any(|position| position.market_id == market_id && position.side == side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_uses_bankers_rounding() {
        let rounded =
            Decimal::new(25, 3).round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven);
        assert_eq!(rounded, Decimal::new(2, 2));
        let rounded =
            Decimal::new(35, 3).round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven);
        assert_eq!(rounded, Decimal::new(4, 2));
    }

    #[test]
    fn sizing_never_exceeds_risk_budget() {
        let (_, loss, _) = size_position(
            Decimal::new(5, 0),
            Decimal::new(50, 2),
            Decimal::new(50, 2),
            Decimal::new(6, 2),
            Decimal::new(1, 2),
            Decimal::new(100, 0),
        );
        assert!(loss <= Decimal::new(5, 0));
        assert!(loss > Decimal::new(49, 1));
    }

    #[test]
    fn quarter_kelly_is_zero_without_edge() {
        assert_eq!(
            quarter_kelly_fraction(
                Decimal::new(50, 2),
                Decimal::new(51, 2),
                Decimal::new(25, 2)
            ),
            Decimal::ZERO
        );
    }

    #[test]
    fn sizing_walks_multiple_levels_and_reports_vwap() {
        let result = size_position_from_levels(
            Decimal::new(5, 0),
            Decimal::ZERO,
            Decimal::ONE,
            Decimal::new(65, 2),
            &[
                ExecutionLevel {
                    side_price: Decimal::new(40, 2),
                    yes_price: Decimal::new(40, 2),
                    quantity: Decimal::new(5, 0),
                },
                ExecutionLevel {
                    side_price: Decimal::new(50, 2),
                    yes_price: Decimal::new(50, 2),
                    quantity: Decimal::new(10, 0),
                },
            ],
        );
        assert_eq!(result.quantity, Decimal::new(11, 0));
        assert_eq!(result.maximum_loss, Decimal::new(5, 0));
        assert_eq!(
            result.average_side_price,
            Decimal::new(5, 0) / Decimal::new(11, 0)
        );
    }

    #[test]
    fn sizing_does_not_cross_price_ceiling() {
        let result = size_position_from_levels(
            Decimal::new(5, 0),
            Decimal::ZERO,
            Decimal::ONE,
            Decimal::new(65, 2),
            &[
                ExecutionLevel {
                    side_price: Decimal::new(60, 2),
                    yes_price: Decimal::new(60, 2),
                    quantity: Decimal::new(2, 0),
                },
                ExecutionLevel {
                    side_price: Decimal::new(66, 2),
                    yes_price: Decimal::new(66, 2),
                    quantity: Decimal::new(100, 0),
                },
            ],
        );
        assert_eq!(result.quantity, Decimal::new(2, 0));
        assert_eq!(result.maximum_loss, Decimal::new(12, 1));
    }

    #[test]
    fn portfolio_enforces_total_exposure_and_duplicate_side() {
        let mut portfolio = PaperPortfolio {
            bankroll: Decimal::ONE_HUNDRED,
            open_exposure: Decimal::ZERO,
            open_positions: Vec::new(),
        };
        let position = PaperPosition {
            opportunity_id: uuid::Uuid::new_v4(),
            event_id: "event".into(),
            market_id: "market".into(),
            side: OutcomeSide::Long,
            maximum_loss: Decimal::new(3, 0),
            opened_at: chrono::Utc::now(),
        };
        assert!(portfolio.open(position, Decimal::new(5, 0)));
        assert!(portfolio.has_market_side("market", OutcomeSide::Long));
        assert!(!portfolio.can_open(Decimal::new(3, 0), Decimal::new(5, 0)));
        assert_eq!(portfolio.exposure_for_event("event"), Decimal::new(3, 0));
    }
}
