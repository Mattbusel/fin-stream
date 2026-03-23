//! # Module: funding
//!
//! ## Responsibility
//! Perpetual-futures funding rate tracking and cash-and-carry arbitrage detection.
//!
//! ## Architecture
//!
//! ```text
//! FundingRate observations
//!      │
//!      ▼
//! FundingTracker (DashMap<symbol, VecDeque<FundingRate>>, max 500 per symbol)
//!      │
//!      ├──► current_rate / annualized_rate / average_rate
//!      ├──► funding_trend (OLS slope)
//!      └──► high_funding_symbols (|rate| > threshold)
//!                │
//!                ▼
//!          FundingArbitrage::check_arb → Option<ArbOpportunity>
//! ```
//!
//! ## Guarantees
//! - Zero panics; all divisions are guarded
//! - DashMap for concurrent access without a global lock
//! - `VecDeque` rolling buffer capped at 500 entries per symbol

use dashmap::DashMap;
use std::collections::VecDeque;

const MAX_HISTORY: usize = 500;

// ─────────────────────────────────────────
//  FundingRate
// ─────────────────────────────────────────

/// A single perpetual-futures funding rate record.
#[derive(Debug, Clone)]
pub struct FundingRate {
    /// Trading pair symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// 8-hour period funding rate (e.g. 0.0001 = 0.01%).
    pub rate: f64,
    /// Unix timestamp (ms) of the *next* scheduled funding settlement.
    pub next_funding_ms: u64,
    /// Unix timestamp (ms) when this record was observed.
    pub timestamp_ms: u64,
}

// ─────────────────────────────────────────
//  FundingTracker
// ─────────────────────────────────────────

/// Concurrent per-symbol funding rate tracker.
///
/// Internally stores up to [`MAX_HISTORY`] (500) records per symbol in a `VecDeque`,
/// evicting the oldest when the cap is reached.
pub struct FundingTracker {
    data: DashMap<String, VecDeque<FundingRate>>,
}

impl FundingTracker {
    /// Create a new empty `FundingTracker`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    /// Record a new funding rate observation for its symbol.
    pub fn record(&self, rate: FundingRate) {
        let mut entry = self
            .data
            .entry(rate.symbol.clone())
            .or_insert_with(|| VecDeque::with_capacity(MAX_HISTORY));
        if entry.len() >= MAX_HISTORY {
            entry.pop_front();
        }
        entry.push_back(rate);
    }

    /// Return the most recent funding rate for `symbol`, or `None` if unknown.
    #[must_use]
    pub fn current_rate(&self, symbol: &str) -> Option<f64> {
        self.data.get(symbol)?.back().map(|r| r.rate)
    }

    /// Return the most recent rate annualised: `rate × 3 × 365` (8-hour periods).
    ///
    /// Returns `None` if the symbol is unknown.
    #[must_use]
    pub fn annualized_rate(&self, symbol: &str) -> Option<f64> {
        Some(self.current_rate(symbol)? * 3.0 * 365.0)
    }

    /// Compute the simple average of the last `last_n` rates for `symbol`.
    ///
    /// Returns `None` if the symbol is unknown or has no data.
    #[must_use]
    pub fn average_rate(&self, symbol: &str, last_n: usize) -> Option<f64> {
        let entry = self.data.get(symbol)?;
        let window: Vec<f64> = entry.iter().rev().take(last_n).map(|r| r.rate).collect();
        if window.is_empty() {
            return None;
        }
        Some(window.iter().sum::<f64>() / window.len() as f64)
    }

    /// Compute the OLS slope of the rate time series over the last `last_n` observations.
    ///
    /// Positive slope means funding rates are trending upward.
    /// Returns `None` if the symbol is unknown or has fewer than 2 observations.
    #[must_use]
    pub fn funding_trend(&self, symbol: &str, last_n: usize) -> Option<f64> {
        let entry = self.data.get(symbol)?;
        // Collect in chronological order (oldest first)
        let window: Vec<f64> = entry
            .iter()
            .rev()
            .take(last_n)
            .map(|r| r.rate)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if window.len() < 2 {
            return None;
        }
        Some(ols_slope(&window))
    }

    /// Return all symbols where `|current_rate| > threshold`, sorted by `|rate|` descending.
    ///
    /// Returns an empty `Vec` if no symbols exceed the threshold.
    #[must_use]
    pub fn high_funding_symbols(&self, threshold: f64) -> Vec<(String, f64)> {
        let mut result: Vec<(String, f64)> = self
            .data
            .iter()
            .filter_map(|entry| {
                let rate = entry.back()?.rate;
                if rate.abs() > threshold {
                    Some((entry.key().clone(), rate))
                } else {
                    None
                }
            })
            .collect();
        result.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap_or(std::cmp::Ordering::Equal));
        result
    }
}

impl Default for FundingTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────
//  FundingArbitrage
// ─────────────────────────────────────────

/// A detected cash-and-carry arbitrage opportunity.
#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    /// The long leg of the trade (e.g. `"spot"`).
    pub long_leg: String,
    /// The short leg of the trade (e.g. `"perp"`).
    pub short_leg: String,
    /// Gross annualised yield before financing costs.
    pub gross_yield: f64,
    /// Net annualised yield after borrow costs.
    pub net_yield: f64,
    /// Days to break even on the trade (365 / (net_yield × 100)).
    pub breakeven_days: f64,
}

/// Detects cash-and-carry arbitrage opportunities between spot and perp markets.
pub struct FundingArbitrage;

impl FundingArbitrage {
    /// Check whether a cash-and-carry opportunity exists.
    ///
    /// The trade is:
    /// - Long spot, short perp → collect funding while hedged.
    ///
    /// # Arguments
    /// * `perp_price`   - Current perpetual futures price.
    /// * `spot_price`   - Current spot price.
    /// * `annual_rate`  - Annualised funding rate (e.g. from [`FundingTracker::annualized_rate`]).
    /// * `borrow_rate`  - Annual cost to borrow/finance the spot leg (e.g. 0.05 = 5%).
    ///
    /// # Returns
    /// `Some(ArbOpportunity)` when `net_yield > 0`; `None` otherwise.
    #[must_use]
    pub fn check_arb(
        perp_price: f64,
        spot_price: f64,
        annual_rate: f64,
        borrow_rate: f64,
    ) -> Option<ArbOpportunity> {
        let basis = if spot_price != 0.0 {
            (perp_price - spot_price) / spot_price
        } else {
            0.0
        };

        // Gross yield: collect funding, adjust for basis (perp premium erodes if positive)
        let gross_yield = annual_rate - basis;
        let net_yield = gross_yield - borrow_rate;

        if net_yield <= 0.0 {
            return None;
        }

        let breakeven_days = if net_yield > 0.0 {
            365.0 / (net_yield * 100.0)
        } else {
            f64::INFINITY
        };

        Some(ArbOpportunity {
            long_leg: "spot".to_owned(),
            short_leg: "perp".to_owned(),
            gross_yield,
            net_yield,
            breakeven_days,
        })
    }
}

// ─────────────────────────────────────────
//  OLS helper
// ─────────────────────────────────────────

fn ols_slope(y: &[f64]) -> f64 {
    let n = y.len();
    if n < 2 {
        return 0.0;
    }
    let n_f = n as f64;
    let mean_x = (n_f - 1.0) / 2.0;
    let mean_y = y.iter().sum::<f64>() / n_f;
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (i, &yi) in y.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (yi - mean_y);
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den }
}

// ─────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rate(symbol: &str, rate: f64, ts: u64) -> FundingRate {
        FundingRate {
            symbol: symbol.to_owned(),
            rate,
            next_funding_ms: ts + 28_800_000,
            timestamp_ms: ts,
        }
    }

    #[test]
    fn annualized_rate_calculation() {
        let tracker = FundingTracker::new();
        tracker.record(make_rate("BTCUSDT", 0.0001, 0));
        let ann = tracker.annualized_rate("BTCUSDT").expect("should have rate");
        assert!((ann - 0.1095).abs() < 1e-10);
    }

    #[test]
    fn current_rate_returns_latest() {
        let tracker = FundingTracker::new();
        tracker.record(make_rate("BTCUSDT", 0.0001, 1000));
        tracker.record(make_rate("BTCUSDT", 0.0005, 2000));
        assert_eq!(tracker.current_rate("BTCUSDT"), Some(0.0005));
    }

    #[test]
    fn average_rate_last_n() {
        let tracker = FundingTracker::new();
        for i in 1..=10_u64 {
            tracker.record(make_rate("X", i as f64 * 0.0001, i * 1000));
        }
        // Last 4: 0.0007, 0.0008, 0.0009, 0.0010 → avg = 0.00085
        let avg = tracker.average_rate("X", 4).expect("should have avg");
        assert!((avg - 0.00085).abs() < 1e-10);
    }

    #[test]
    fn funding_trend_increasing() {
        let tracker = FundingTracker::new();
        for i in 0..10_u64 {
            tracker.record(make_rate("BTC", i as f64 * 0.0001, i * 1000));
        }
        let trend = tracker.funding_trend("BTC", 10).expect("should have trend");
        assert!(trend > 0.0, "trend should be positive for increasing rates");
    }

    #[test]
    fn funding_trend_decreasing() {
        let tracker = FundingTracker::new();
        for i in (0..10_u64).rev() {
            tracker.record(make_rate("ETH", i as f64 * 0.0001, (9 - i) * 1000));
        }
        let trend = tracker.funding_trend("ETH", 10).expect("should have trend");
        assert!(trend < 0.0, "trend should be negative for decreasing rates");
    }

    #[test]
    fn high_funding_symbols_filter() {
        let tracker = FundingTracker::new();
        tracker.record(make_rate("HIGH", 0.005, 0));   // above threshold
        tracker.record(make_rate("LOW", 0.0001, 0));   // below threshold
        tracker.record(make_rate("NEG", -0.006, 0));   // above threshold (negative)

        let high = tracker.high_funding_symbols(0.001);
        let symbols: Vec<&str> = high.iter().map(|(s, _)| s.as_str()).collect();
        assert!(symbols.contains(&"HIGH"));
        assert!(symbols.contains(&"NEG"));
        assert!(!symbols.contains(&"LOW"));
    }

    #[test]
    fn high_funding_sorted_by_abs_rate_desc() {
        let tracker = FundingTracker::new();
        tracker.record(make_rate("A", 0.003, 0));
        tracker.record(make_rate("B", 0.007, 0));
        tracker.record(make_rate("C", 0.005, 0));

        let high = tracker.high_funding_symbols(0.002);
        assert_eq!(high[0].0, "B");
        assert_eq!(high[1].0, "C");
        assert_eq!(high[2].0, "A");
    }

    #[test]
    fn arb_opportunity_detected() {
        // annual_rate=0.20, borrow_rate=0.05, perp=spot (no basis)
        let opp = FundingArbitrage::check_arb(30_000.0, 30_000.0, 0.20, 0.05);
        let opp = opp.expect("should find arb");
        assert!((opp.gross_yield - 0.20).abs() < 1e-10);
        assert!((opp.net_yield - 0.15).abs() < 1e-10);
        assert!(opp.breakeven_days > 0.0 && opp.breakeven_days.is_finite());
        assert_eq!(opp.long_leg, "spot");
        assert_eq!(opp.short_leg, "perp");
    }

    #[test]
    fn arb_no_opportunity_negative_net_yield() {
        // annual_rate=0.02 is less than borrow_rate=0.05
        let opp = FundingArbitrage::check_arb(30_000.0, 30_000.0, 0.02, 0.05);
        assert!(opp.is_none());
    }

    #[test]
    fn arb_basis_reduces_gross_yield() {
        // perp premium of 1% reduces gross yield by 1%
        let opp_no_basis = FundingArbitrage::check_arb(30_000.0, 30_000.0, 0.20, 0.05);
        let opp_with_basis = FundingArbitrage::check_arb(30_300.0, 30_000.0, 0.20, 0.05);
        let g_no = opp_no_basis.unwrap().gross_yield;
        let g_with = opp_with_basis.unwrap().gross_yield;
        // With perp premium (basis > 0), gross_yield = annual_rate - basis → lower
        assert!(g_with < g_no);
    }

    #[test]
    fn unknown_symbol_returns_none() {
        let tracker = FundingTracker::new();
        assert!(tracker.current_rate("UNKNOWN").is_none());
        assert!(tracker.annualized_rate("UNKNOWN").is_none());
        assert!(tracker.average_rate("UNKNOWN", 10).is_none());
        assert!(tracker.funding_trend("UNKNOWN", 10).is_none());
    }

    #[test]
    fn eviction_at_max_capacity() {
        let tracker = FundingTracker::new();
        for i in 0..=(MAX_HISTORY + 10) as u64 {
            tracker.record(make_rate("BTC", 0.0001, i));
        }
        // Should not panic; history stays at MAX_HISTORY
        let entry = tracker.data.get("BTC").expect("should exist");
        assert_eq!(entry.len(), MAX_HISTORY);
    }
}
