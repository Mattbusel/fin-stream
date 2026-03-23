//! # Synthetic instrument construction and pricing.
//!
//! ## Responsibility
//! Build and price synthetic instruments composed of weighted components,
//! compute portfolio-level delta, beta, and replication error.

use std::collections::HashMap;

// ─── SyntheticComponent ───────────────────────────────────────────────────────

/// A single component of a synthetic instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticComponent {
    /// Underlying symbol (e.g. "SPY", "AAPL").
    pub symbol: String,
    /// Portfolio weight in [0, 1]. Weights across all components should sum to 1.
    pub weight: f64,
    /// Price multiplier for contract-based components (e.g. 100 for options).
    pub price_multiplier: f64,
}

// ─── SyntheticInstrument ──────────────────────────────────────────────────────

/// A synthetic instrument made up of weighted component positions.
///
/// # Example
/// ```rust
/// use fin_stream::synthetic::instrument::{SyntheticInstrument, SyntheticComponent};
///
/// let basket = SyntheticInstrument::equal_weight(&[
///     "SPY".to_string(), "QQQ".to_string(), "IWM".to_string(),
/// ]);
/// assert_eq!(basket.components.len(), 3);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticInstrument {
    /// Unique instrument identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Component legs.
    pub components: Vec<SyntheticComponent>,
}

impl SyntheticInstrument {
    /// Construct an equal-weight basket from the given symbols.
    ///
    /// Each component receives weight `1 / n` and `price_multiplier = 1.0`.
    pub fn equal_weight(symbols: &[String]) -> SyntheticInstrument {
        let n = symbols.len();
        let weight = if n == 0 { 0.0 } else { 1.0 / n as f64 };
        let components = symbols
            .iter()
            .map(|s| SyntheticComponent {
                symbol: s.clone(),
                weight,
                price_multiplier: 1.0,
            })
            .collect();
        SyntheticInstrument {
            id: "basket".to_string(),
            name: "Equal-Weight Basket".to_string(),
            components,
        }
    }
}

// ─── SyntheticPricer ─────────────────────────────────────────────────────────

/// Computes price and risk metrics for synthetic instruments.
///
/// # Example
/// ```rust
/// use fin_stream::synthetic::instrument::{SyntheticInstrument, SyntheticComponent, SyntheticPricer};
/// use std::collections::HashMap;
///
/// let inst = SyntheticInstrument {
///     id: "test".to_string(),
///     name: "Test".to_string(),
///     components: vec![
///         SyntheticComponent { symbol: "A".to_string(), weight: 0.5, price_multiplier: 1.0 },
///         SyntheticComponent { symbol: "B".to_string(), weight: 0.5, price_multiplier: 1.0 },
///     ],
/// };
/// let mut prices = HashMap::new();
/// prices.insert("A".to_string(), 100.0_f64);
/// prices.insert("B".to_string(), 200.0_f64);
///
/// let price = SyntheticPricer::compute_price(&inst, &prices).unwrap();
/// assert!((price - 150.0).abs() < 1e-9);
/// ```
pub struct SyntheticPricer;

impl SyntheticPricer {
    /// Compute the synthetic price as the weighted sum of component prices * multiplier.
    ///
    /// Returns `None` if any component price is missing from `prices`.
    pub fn compute_price(
        instrument: &SyntheticInstrument,
        prices: &HashMap<String, f64>,
    ) -> Option<f64> {
        let mut total = 0.0;
        for component in &instrument.components {
            let price = prices.get(&component.symbol)?;
            total += component.weight * component.price_multiplier * price;
        }
        Some(total)
    }

    /// Compute portfolio-level delta as the weighted sum of component deltas.
    ///
    /// Missing deltas are treated as 0.0.
    pub fn compute_delta(
        instrument: &SyntheticInstrument,
        deltas: &HashMap<String, f64>,
    ) -> f64 {
        instrument
            .components
            .iter()
            .map(|c| {
                let d = deltas.get(&c.symbol).copied().unwrap_or(0.0);
                c.weight * d
            })
            .sum()
    }

    /// Compute portfolio-level beta as the weighted sum of component betas.
    ///
    /// Missing betas are treated as 0.0.
    pub fn compute_beta(
        instrument: &SyntheticInstrument,
        betas: &HashMap<String, f64>,
    ) -> f64 {
        instrument
            .components
            .iter()
            .map(|c| {
                let b = betas.get(&c.symbol).copied().unwrap_or(0.0);
                c.weight * b
            })
            .sum()
    }

    /// Compute replication error as `|synthetic_price - target_price| / target_price`.
    ///
    /// Returns 0.0 if `target_price` is zero or any price is missing.
    pub fn replication_error(
        instrument: &SyntheticInstrument,
        target_price: f64,
        prices: &HashMap<String, f64>,
    ) -> f64 {
        if target_price == 0.0 {
            return 0.0;
        }
        match Self::compute_price(instrument, prices) {
            Some(synthetic) => (synthetic - target_price).abs() / target_price,
            None => 0.0,
        }
    }
}

// ─── PairSpread ───────────────────────────────────────────────────────────────

/// A long/short pair spread instrument.
///
/// # Example
/// ```rust
/// use fin_stream::synthetic::instrument::PairSpread;
///
/// let pair = PairSpread {
///     long_symbol: "SPY".to_string(),
///     short_symbol: "IVV".to_string(),
///     hedge_ratio: 1.02,
/// };
/// let spread = pair.spread_price(450.0, 440.0);
/// // 450 - 1.02 * 440 = 450 - 448.8 = 1.2
/// assert!((spread - 1.2).abs() < 1e-9);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PairSpread {
    /// Long leg symbol.
    pub long_symbol: String,
    /// Short leg symbol.
    pub short_symbol: String,
    /// Hedge ratio: units of short per unit of long.
    pub hedge_ratio: f64,
}

impl PairSpread {
    /// Compute the spread price: `long_price - hedge_ratio * short_price`.
    pub fn spread_price(&self, long_price: f64, short_price: f64) -> f64 {
        long_price - self.hedge_ratio * short_price
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn two_component_instrument() -> SyntheticInstrument {
        SyntheticInstrument {
            id: "test".to_string(),
            name: "Test".to_string(),
            components: vec![
                SyntheticComponent {
                    symbol: "A".to_string(),
                    weight: 0.6,
                    price_multiplier: 1.0,
                },
                SyntheticComponent {
                    symbol: "B".to_string(),
                    weight: 0.4,
                    price_multiplier: 2.0,
                },
            ],
        }
    }

    #[test]
    fn test_equal_weight_basket() {
        let syms: Vec<String> = ["SPY", "QQQ", "IWM"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let basket = SyntheticInstrument::equal_weight(&syms);
        assert_eq!(basket.components.len(), 3);
        for c in &basket.components {
            assert!((c.weight - 1.0 / 3.0).abs() < 1e-9);
            assert_eq!(c.price_multiplier, 1.0);
        }
    }

    #[test]
    fn test_equal_weight_empty() {
        let basket = SyntheticInstrument::equal_weight(&[]);
        assert!(basket.components.is_empty());
    }

    #[test]
    fn test_compute_price_all_present() {
        let inst = two_component_instrument();
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 100.0_f64);
        prices.insert("B".to_string(), 50.0_f64);
        // 0.6 * 1.0 * 100 + 0.4 * 2.0 * 50 = 60 + 40 = 100
        let p = SyntheticPricer::compute_price(&inst, &prices).unwrap();
        assert!((p - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_price_missing_returns_none() {
        let inst = two_component_instrument();
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 100.0_f64);
        // "B" missing
        assert!(SyntheticPricer::compute_price(&inst, &prices).is_none());
    }

    #[test]
    fn test_compute_delta() {
        let inst = two_component_instrument();
        let mut deltas = HashMap::new();
        deltas.insert("A".to_string(), 0.5_f64);
        deltas.insert("B".to_string(), 0.8_f64);
        // 0.6 * 0.5 + 0.4 * 0.8 = 0.30 + 0.32 = 0.62
        let d = SyntheticPricer::compute_delta(&inst, &deltas);
        assert!((d - 0.62).abs() < 1e-9);
    }

    #[test]
    fn test_compute_delta_missing_treated_as_zero() {
        let inst = two_component_instrument();
        let mut deltas = HashMap::new();
        deltas.insert("A".to_string(), 1.0_f64);
        // "B" missing -> treated as 0
        // 0.6 * 1.0 + 0.4 * 0 = 0.6
        let d = SyntheticPricer::compute_delta(&inst, &deltas);
        assert!((d - 0.6).abs() < 1e-9);
    }

    #[test]
    fn test_compute_beta() {
        let inst = two_component_instrument();
        let mut betas = HashMap::new();
        betas.insert("A".to_string(), 1.2_f64);
        betas.insert("B".to_string(), 0.8_f64);
        // 0.6 * 1.2 + 0.4 * 0.8 = 0.72 + 0.32 = 1.04
        let b = SyntheticPricer::compute_beta(&inst, &betas);
        assert!((b - 1.04).abs() < 1e-9);
    }

    #[test]
    fn test_replication_error_exact() {
        let inst = two_component_instrument();
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 100.0_f64);
        prices.insert("B".to_string(), 50.0_f64);
        // synthetic = 100, target = 100 -> error = 0
        let err = SyntheticPricer::replication_error(&inst, 100.0, &prices);
        assert!(err.abs() < 1e-9);
    }

    #[test]
    fn test_replication_error_nonzero() {
        let inst = two_component_instrument();
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 100.0_f64);
        prices.insert("B".to_string(), 50.0_f64);
        // synthetic = 100, target = 110 -> error = 10/110 = 0.0909...
        let err = SyntheticPricer::replication_error(&inst, 110.0, &prices);
        assert!((err - 10.0 / 110.0).abs() < 1e-9);
    }

    #[test]
    fn test_replication_error_zero_target() {
        let inst = two_component_instrument();
        let prices = HashMap::new();
        assert_eq!(SyntheticPricer::replication_error(&inst, 0.0, &prices), 0.0);
    }

    #[test]
    fn test_pair_spread_price() {
        let pair = PairSpread {
            long_symbol: "SPY".to_string(),
            short_symbol: "IVV".to_string(),
            hedge_ratio: 1.02,
        };
        // 450 - 1.02 * 440 = 450 - 448.8 = 1.2
        let spread = pair.spread_price(450.0, 440.0);
        assert!((spread - 1.2).abs() < 1e-9);
    }

    #[test]
    fn test_pair_spread_negative() {
        let pair = PairSpread {
            long_symbol: "SPY".to_string(),
            short_symbol: "IVV".to_string(),
            hedge_ratio: 1.0,
        };
        // 100 - 1.0 * 105 = -5
        let spread = pair.spread_price(100.0, 105.0);
        assert!((spread - (-5.0)).abs() < 1e-9);
    }
}
