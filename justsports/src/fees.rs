//! Prediction-market fees.

use hftbacktest::{backtest::models::FeeModel, types::Order};

/// A fee of `rate * p * (1 - p)` dollars per contract at price `p`, with separate maker and
/// taker rates. Kalshi's sports schedules have this form: a taker pays `0.07 * multiplier`, a
/// maker `0.07 * maker_share * multiplier` (`just_sports.dataprocessor.kalshi.fee_per_contract`).
/// The fee is unrounded, the marginal-fee approximation our backtests use.
#[derive(Clone, Debug)]
pub struct ProbabilityFee {
    pub maker_rate: f64,
    pub taker_rate: f64,
}

impl ProbabilityFee {
    pub fn new(maker_rate: f64, taker_rate: f64) -> Self {
        Self {
            maker_rate,
            taker_rate,
        }
    }

    /// Dollars of fee per contract at `price` for a maker or a taker.
    pub fn per_contract(&self, price: f64, maker: bool) -> f64 {
        let rate = if maker { self.maker_rate } else { self.taker_rate };
        rate * price * (1.0 - price)
    }
}

impl FeeModel for ProbabilityFee {
    fn amount(&self, order: &Order, _amount: f64) -> f64 {
        self.per_contract(order.exec_price(), order.maker) * order.exec_qty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kalshi_quadratic() {
        // 0.07 * 0.25 * p(1-p) at 0.40 for a maker, 0.07 * p(1-p) for a taker
        let fee = ProbabilityFee::new(0.07 * 0.25, 0.07);
        assert!((fee.per_contract(0.40, true) - 0.0042).abs() < 1e-12);
        assert!((fee.per_contract(0.40, false) - 0.0168).abs() < 1e-12);
    }
}
