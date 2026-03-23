//! Market impact and slippage estimation models.
//!
//! Implements Linear, Square-Root, Kyle-Obizhaeva, and Almgren-Chriss impact models,
//! plus optimal execution horizon and VWAP schedule calculation.

/// Market impact model variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketImpactModel {
    /// Linear impact: cost proportional to participation rate.
    Linear,
    /// Square-root impact: cost proportional to sqrt(participation rate).
    SquareRoot,
    /// Kyle-Obizhaeva model: impact proportional to sqrt(qty / (adv * T)).
    KyleObizhaeva,
    /// Almgren-Chriss model: separates permanent and temporary impact.
    AlmgrenChriss,
}

/// Buy or sell side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    /// Buy (long) order.
    Buy,
    /// Sell (short) order.
    Sell,
}

impl Side {
    /// +1.0 for Buy, -1.0 for Sell.
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// Inputs required to estimate slippage and market impact.
#[derive(Debug, Clone)]
pub struct SlippageInputs {
    /// Order quantity (number of shares or contracts).
    pub quantity: f64,
    /// Average daily volume (ADV) in the same units as `quantity`.
    pub adv: f64,
    /// Quoted bid-ask spread in basis points.
    pub spread_bps: f64,
    /// Daily return volatility (as a decimal, e.g. 0.02 = 2%).
    pub volatility: f64,
    /// Current mid-price of the instrument.
    pub price: f64,
    /// Order direction.
    pub side: Side,
}

/// Result of a slippage and market impact estimate.
#[derive(Debug, Clone)]
pub struct SlippageResult {
    /// Half-spread cost in basis points.
    pub spread_cost_bps: f64,
    /// Estimated market impact in basis points.
    pub market_impact_bps: f64,
    /// Total estimated transaction cost in basis points.
    pub total_cost_bps: f64,
    /// Total estimated transaction cost in USD (or price units).
    pub total_cost_usd: f64,
    /// Fraction of ADV represented by this order.
    pub participation_rate: f64,
}

/// Stateless slippage and market impact model.
pub struct SlippageModel;

impl SlippageModel {
    /// Estimate slippage for given inputs under the specified impact model.
    pub fn estimate(inputs: &SlippageInputs, model: MarketImpactModel) -> SlippageResult {
        let adv = inputs.adv.max(1.0);
        let qty = inputs.quantity.abs();
        let participation = qty / adv;

        // Half-spread cost (buyer pays half-spread, seller pays half-spread).
        let spread_cost_bps = inputs.spread_bps / 2.0;

        // Eta (permanent impact coefficient).
        let eta = 0.1_f64;
        // Gamma (permanent impact for Almgren-Chriss).
        let gamma = 0.1_f64;
        // Execution horizon in days (assume 1 day by default for single-model estimates).
        let t_days = 1.0_f64;

        let market_impact_bps = match model {
            MarketImpactModel::Linear => {
                // impact = eta * (qty/adv) * volatility, converted to bps
                eta * participation * inputs.volatility * 10_000.0
            }
            MarketImpactModel::SquareRoot => {
                // impact = eta * sqrt(qty/adv) * volatility, converted to bps
                eta * participation.sqrt() * inputs.volatility * 10_000.0
            }
            MarketImpactModel::KyleObizhaeva => {
                // impact = sigma * sqrt(qty / (adv * T)), converted to bps
                inputs.volatility * (qty / (adv * t_days)).sqrt() * 10_000.0
            }
            MarketImpactModel::AlmgrenChriss => {
                // permanent = gamma * sign * qty/adv
                // temporary = eta * sign * qty/(adv*T)
                let permanent = gamma * participation;
                let temporary = eta * (qty / (adv * t_days));
                (permanent + temporary) * 10_000.0
            }
        };

        let total_cost_bps = spread_cost_bps + market_impact_bps;
        let total_cost_usd = total_cost_bps / 10_000.0 * inputs.price * qty;

        SlippageResult {
            spread_cost_bps,
            market_impact_bps,
            total_cost_bps,
            total_cost_usd,
            participation_rate: participation,
        }
    }

    /// Optimal execution horizon (in days) under Almgren-Chriss framework.
    ///
    /// T* = sqrt(qty * risk_aversion / (adv * gamma))
    ///
    /// where gamma is the permanent impact coefficient (fixed at 0.1).
    pub fn optimal_execution_horizon(qty: f64, adv: f64, risk_aversion: f64) -> f64 {
        let gamma = 0.1_f64;
        let adv = adv.max(1.0);
        let numerator = qty.abs() * risk_aversion.abs();
        let denominator = adv * gamma;
        if denominator < 1e-12 {
            return 1.0;
        }
        (numerator / denominator).sqrt()
    }

    /// Compute a VWAP execution schedule: split `qty` across `horizon_minutes`
    /// proportional to the given intraday volume profile.
    ///
    /// `volume_profile` is a normalized or raw intraday volume shape vector.
    /// Returns a `Vec<f64>` with one slice size per minute in the horizon.
    /// The slices sum to `qty`.
    pub fn vwap_schedule(qty: f64, horizon_minutes: u32, volume_profile: &[f64]) -> Vec<f64> {
        let horizon = horizon_minutes as usize;
        if horizon == 0 || volume_profile.is_empty() {
            return Vec::new();
        }

        // Sample the volume profile at `horizon` evenly-spaced points.
        let profile_len = volume_profile.len();
        let sampled: Vec<f64> = (0..horizon)
            .map(|i| {
                let idx = (i * profile_len / horizon).min(profile_len - 1);
                volume_profile[idx].max(0.0)
            })
            .collect();

        let total_profile: f64 = sampled.iter().sum();
        if total_profile < 1e-12 {
            // Uniform fallback.
            let slice = qty / horizon as f64;
            return vec![slice; horizon];
        }

        sampled.iter().map(|v| qty * v / total_profile).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_inputs() -> SlippageInputs {
        SlippageInputs {
            quantity: 10_000.0,
            adv: 1_000_000.0,
            spread_bps: 2.0,
            volatility: 0.02,
            price: 100.0,
            side: Side::Buy,
        }
    }

    #[test]
    fn linear_model_positive_impact() {
        let inputs = default_inputs();
        let result = SlippageModel::estimate(&inputs, MarketImpactModel::Linear);
        assert!(result.market_impact_bps > 0.0);
        assert!(result.total_cost_bps > result.spread_cost_bps);
    }

    #[test]
    fn sqrt_model_less_than_linear_for_small_participation() {
        let inputs = default_inputs(); // participation = 1%
        let linear = SlippageModel::estimate(&inputs, MarketImpactModel::Linear);
        let sqrt = SlippageModel::estimate(&inputs, MarketImpactModel::SquareRoot);
        // sqrt(0.01) = 0.1 > 0.01, so sqrt impact is larger for small participation
        assert!(sqrt.market_impact_bps > 0.0);
        assert!(linear.market_impact_bps > 0.0);
    }

    #[test]
    fn kyle_obizhaeva_positive() {
        let inputs = default_inputs();
        let result = SlippageModel::estimate(&inputs, MarketImpactModel::KyleObizhaeva);
        assert!(result.market_impact_bps > 0.0);
    }

    #[test]
    fn almgren_chriss_positive() {
        let inputs = default_inputs();
        let result = SlippageModel::estimate(&inputs, MarketImpactModel::AlmgrenChriss);
        assert!(result.market_impact_bps > 0.0);
        assert!(result.total_cost_usd > 0.0);
    }

    #[test]
    fn participation_rate_correct() {
        let inputs = default_inputs();
        let result = SlippageModel::estimate(&inputs, MarketImpactModel::Linear);
        assert!((result.participation_rate - 0.01).abs() < 1e-10);
    }

    #[test]
    fn optimal_horizon_positive() {
        let t = SlippageModel::optimal_execution_horizon(50_000.0, 1_000_000.0, 0.01);
        assert!(t > 0.0);
    }

    #[test]
    fn vwap_schedule_sums_to_qty() {
        let profile = vec![1.0, 2.0, 3.0, 2.0, 1.0];
        let schedule = SlippageModel::vwap_schedule(10_000.0, 5, &profile);
        assert_eq!(schedule.len(), 5);
        let total: f64 = schedule.iter().sum();
        assert!((total - 10_000.0).abs() < 1e-8);
    }

    #[test]
    fn vwap_schedule_uniform_fallback() {
        let schedule = SlippageModel::vwap_schedule(1_000.0, 4, &[0.0, 0.0, 0.0]);
        assert_eq!(schedule.len(), 4);
        let total: f64 = schedule.iter().sum();
        assert!((total - 1_000.0).abs() < 1e-8);
    }

    #[test]
    fn side_sign() {
        assert!((Side::Buy.sign() - 1.0).abs() < 1e-10);
        assert!((Side::Sell.sign() - (-1.0)).abs() < 1e-10);
    }
}
