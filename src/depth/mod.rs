//! # Module: depth
//!
//! ## Responsibility
//! Order book depth imbalance signals: raw and weighted imbalance metrics,
//! bid-ask spread, market-impact (slippage) estimation, and a per-symbol
//! rolling tracker backed by DashMap.
//!
//! ## Guarantees
//! - Zero panics; divisions are guarded; empty books return 0.0
//! - `VecDeque` rolling window (capacity 100) — no unbounded growth
//! - All public items documented

use dashmap::DashMap;
use std::collections::VecDeque;

// ─────────────────────────────────────────
//  DepthLevel
// ─────────────────────────────────────────

/// A single price level in the order book.
#[derive(Debug, Clone, Copy)]
pub struct DepthLevel {
    /// Price of this level.
    pub price: f64,
    /// Resting quantity at this price.
    pub quantity: f64,
}

// ─────────────────────────────────────────
//  DepthSnapshot
// ─────────────────────────────────────────

/// A full L2 snapshot of the order book at a point in time.
///
/// Convention:
/// - `bids` sorted **descending** by price (best bid first).
/// - `asks` sorted **ascending** by price (best ask first).
#[derive(Debug, Clone)]
pub struct DepthSnapshot {
    /// Bid side, sorted descending by price.
    pub bids: Vec<DepthLevel>,
    /// Ask side, sorted ascending by price.
    pub asks: Vec<DepthLevel>,
    /// Snapshot time in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

impl DepthSnapshot {
    /// Best bid price, or `None` if the bid side is empty.
    #[must_use]
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|l| l.price)
    }

    /// Best ask price, or `None` if the ask side is empty.
    #[must_use]
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|l| l.price)
    }

    /// Mid price = (best bid + best ask) / 2, or `None` if either side is empty.
    #[must_use]
    pub fn mid_price(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }

    /// Bid-ask spread = best ask − best bid, or `None` if either side is empty.
    #[must_use]
    pub fn spread(&self) -> Option<f64> {
        Some(self.best_ask()? - self.best_bid()?)
    }
}

// ─────────────────────────────────────────
//  DepthPressure
// ─────────────────────────────────────────

/// Directional pressure signal derived from depth imbalance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DepthPressure {
    /// Bid volume dominates (imbalance > 0.2): buy-side pressure.
    Buying,
    /// Ask volume dominates (imbalance < −0.2): sell-side pressure.
    Selling,
    /// Neither side dominates.
    Neutral,
}

// ─────────────────────────────────────────
//  DepthImbalance
// ─────────────────────────────────────────

/// Depth imbalance metrics for a given number of book levels.
#[derive(Debug, Clone, Copy)]
pub struct DepthImbalance {
    /// Total bid-side volume across the analysed levels.
    pub bid_volume: f64,
    /// Total ask-side volume across the analysed levels.
    pub ask_volume: f64,
    /// Normalised imbalance in [−1, 1]:
    /// `(bid_vol − ask_vol) / (bid_vol + ask_vol)`.
    /// Zero when both sides are equal or empty.
    pub imbalance: f64,
    /// Directional signal derived from `imbalance`.
    pub pressure: DepthPressure,
}

fn classify(imbalance: f64) -> DepthPressure {
    if imbalance > 0.2 {
        DepthPressure::Buying
    } else if imbalance < -0.2 {
        DepthPressure::Selling
    } else {
        DepthPressure::Neutral
    }
}

/// Compute raw (unweighted) depth imbalance from the top `levels` of each side.
///
/// Returns all-zero `DepthImbalance` if the book is empty.
#[must_use]
pub fn compute_imbalance(snapshot: &DepthSnapshot, levels: usize) -> DepthImbalance {
    let bid_volume: f64 = snapshot.bids.iter().take(levels).map(|l| l.quantity).sum();
    let ask_volume: f64 = snapshot.asks.iter().take(levels).map(|l| l.quantity).sum();
    let total = bid_volume + ask_volume;
    let imbalance = if total > 0.0 {
        (bid_volume - ask_volume) / total
    } else {
        0.0
    };
    DepthImbalance {
        bid_volume,
        ask_volume,
        imbalance,
        pressure: classify(imbalance),
    }
}

// ─────────────────────────────────────────
//  WeightedImbalance
// ─────────────────────────────────────────

/// Compute **weighted** depth imbalance, giving higher weight to levels closer to the mid price.
///
/// Weight for level `i` (0-indexed from best) = `1 / (i + 1)`.
/// If the book is empty or the mid price is unavailable, returns all-zero `DepthImbalance`.
#[must_use]
pub fn compute_weighted(snapshot: &DepthSnapshot, levels: usize) -> DepthImbalance {
    let _mid = match snapshot.mid_price() {
        Some(m) => m,
        None => return DepthImbalance { bid_volume: 0.0, ask_volume: 0.0, imbalance: 0.0, pressure: DepthPressure::Neutral },
    };

    let mut bid_volume = 0.0_f64;
    let mut ask_volume = 0.0_f64;

    for (i, level) in snapshot.bids.iter().take(levels).enumerate() {
        let weight = 1.0 / (i as f64 + 1.0);
        bid_volume += level.quantity * weight;
    }
    for (i, level) in snapshot.asks.iter().take(levels).enumerate() {
        let weight = 1.0 / (i as f64 + 1.0);
        ask_volume += level.quantity * weight;
    }

    let total = bid_volume + ask_volume;
    let imbalance = if total > 0.0 {
        (bid_volume - ask_volume) / total
    } else {
        0.0
    };

    DepthImbalance {
        bid_volume,
        ask_volume,
        imbalance,
        pressure: classify(imbalance),
    }
}

// ─────────────────────────────────────────
//  Market impact (slippage)
// ─────────────────────────────────────────

/// Walk the ask side to fill `qty` units and return average execution price minus best ask.
///
/// Returns `0.0` if `qty` ≤ 0, the ask side is empty, or there is insufficient liquidity
/// (fills as much as available and returns slippage on what was filled).
#[must_use]
pub fn market_impact_bid(snapshot: &DepthSnapshot, qty: f64) -> f64 {
    if qty <= 0.0 || snapshot.asks.is_empty() {
        return 0.0;
    }
    let best_ask = snapshot.asks[0].price;
    let mut remaining = qty;
    let mut cost = 0.0_f64;
    let mut filled = 0.0_f64;

    for level in &snapshot.asks {
        if remaining <= 0.0 {
            break;
        }
        let take = remaining.min(level.quantity);
        cost += take * level.price;
        filled += take;
        remaining -= take;
    }

    if filled == 0.0 {
        return 0.0;
    }
    let avg_price = cost / filled;
    avg_price - best_ask
}

/// Walk the bid side to fill `qty` units and return best bid minus average execution price.
///
/// Returns `0.0` if `qty` ≤ 0 or the bid side is empty.
#[must_use]
pub fn market_impact_ask(snapshot: &DepthSnapshot, qty: f64) -> f64 {
    if qty <= 0.0 || snapshot.bids.is_empty() {
        return 0.0;
    }
    let best_bid = snapshot.bids[0].price;
    let mut remaining = qty;
    let mut proceeds = 0.0_f64;
    let mut filled = 0.0_f64;

    for level in &snapshot.bids {
        if remaining <= 0.0 {
            break;
        }
        let take = remaining.min(level.quantity);
        proceeds += take * level.price;
        filled += take;
        remaining -= take;
    }

    if filled == 0.0 {
        return 0.0;
    }
    let avg_price = proceeds / filled;
    best_bid - avg_price
}

// ─────────────────────────────────────────
//  DepthSignal
// ─────────────────────────────────────────

/// Composite depth signal derived from a single [`DepthSnapshot`].
#[derive(Debug, Clone, Copy)]
pub struct DepthSignal {
    /// Raw depth imbalance in [−1, 1].
    pub imbalance: f64,
    /// Weighted depth imbalance in [−1, 1].
    pub weighted_imbalance: f64,
    /// Bid-ask spread (best ask − best bid).
    pub bid_ask_spread: f64,
    /// Mid price = (best bid + best ask) / 2.
    pub mid_price: f64,
    /// Estimated buy-side slippage for `impact_qty` units (avg fill − best ask).
    pub market_impact_bid: f64,
    /// Estimated sell-side slippage for `impact_qty` units (best bid − avg fill).
    pub market_impact_ask: f64,
}

/// Compute a composite [`DepthSignal`] from a snapshot.
///
/// Uses all available levels for imbalance and weighted imbalance.
/// Market impact is calculated for `impact_qty` units.
#[must_use]
pub fn compute_signal(snapshot: &DepthSnapshot, impact_qty: f64) -> DepthSignal {
    let levels = snapshot.bids.len().max(snapshot.asks.len());
    let raw = compute_imbalance(snapshot, levels);
    let weighted = compute_weighted(snapshot, levels);

    DepthSignal {
        imbalance: raw.imbalance,
        weighted_imbalance: weighted.imbalance,
        bid_ask_spread: snapshot.spread().unwrap_or(0.0),
        mid_price: snapshot.mid_price().unwrap_or(0.0),
        market_impact_bid: market_impact_bid(snapshot, impact_qty),
        market_impact_ask: market_impact_ask(snapshot, impact_qty),
    }
}

// ─────────────────────────────────────────
//  DepthTracker
// ─────────────────────────────────────────

const ROLLING_WINDOW: usize = 100;

/// Per-symbol rolling depth imbalance tracker backed by a [`DashMap`].
///
/// Stores the last 100 [`DepthImbalance`] snapshots per symbol.
pub struct DepthTracker {
    /// Internal map: symbol → rolling window of imbalance values.
    history: DashMap<String, VecDeque<f64>>,
}

impl DepthTracker {
    /// Create a new empty `DepthTracker`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: DashMap::new(),
        }
    }

    /// Process a new depth snapshot for `symbol`, updating the rolling window.
    pub fn update(&self, symbol: &str, snapshot: DepthSnapshot, levels: usize) {
        let imb = compute_imbalance(&snapshot, levels);
        let mut entry = self.history.entry(symbol.to_owned()).or_insert_with(|| VecDeque::with_capacity(ROLLING_WINDOW));
        if entry.len() >= ROLLING_WINDOW {
            entry.pop_front();
        }
        entry.push_back(imb.imbalance);
    }

    /// Compute the OLS slope of the imbalance time series for `symbol`.
    ///
    /// Returns `None` if the symbol is unknown or has fewer than 2 observations.
    #[must_use]
    pub fn imbalance_trend(&self, symbol: &str) -> Option<f64> {
        let entry = self.history.get(symbol)?;
        let data: Vec<f64> = entry.iter().copied().collect();
        if data.len() < 2 {
            return None;
        }
        Some(ols_slope(&data))
    }
}

impl Default for DepthTracker {
    fn default() -> Self {
        Self::new()
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

    fn make_book(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> DepthSnapshot {
        DepthSnapshot {
            bids: bids.iter().map(|&(p, q)| DepthLevel { price: p, quantity: q }).collect(),
            asks: asks.iter().map(|&(p, q)| DepthLevel { price: p, quantity: q }).collect(),
            timestamp_ms: 0,
        }
    }

    #[test]
    fn imbalance_buying_pressure() {
        // 80 bid vs 20 ask → imbalance = 0.6
        let snap = make_book(&[(100.0, 80.0)], &[(101.0, 20.0)]);
        let di = compute_imbalance(&snap, 5);
        assert!((di.imbalance - 0.6).abs() < 1e-10);
        assert_eq!(di.pressure, DepthPressure::Buying);
    }

    #[test]
    fn imbalance_selling_pressure() {
        let snap = make_book(&[(100.0, 10.0)], &[(101.0, 90.0)]);
        let di = compute_imbalance(&snap, 5);
        assert!(di.imbalance < -0.2);
        assert_eq!(di.pressure, DepthPressure::Selling);
    }

    #[test]
    fn imbalance_neutral() {
        let snap = make_book(&[(100.0, 50.0)], &[(101.0, 50.0)]);
        let di = compute_imbalance(&snap, 5);
        assert_eq!(di.imbalance, 0.0);
        assert_eq!(di.pressure, DepthPressure::Neutral);
    }

    #[test]
    fn market_impact_bid_zero_qty() {
        let snap = make_book(&[], &[(101.0, 100.0)]);
        assert_eq!(market_impact_bid(&snap, 0.0), 0.0);
    }

    #[test]
    fn market_impact_bid_single_level() {
        let snap = make_book(&[(100.0, 100.0)], &[(101.0, 100.0)]);
        // Filling 50 units at 101.0: avg = 101.0, best ask = 101.0 → slippage = 0
        let impact = market_impact_bid(&snap, 50.0);
        assert!((impact - 0.0).abs() < 1e-10);
    }

    #[test]
    fn market_impact_bid_multi_level() {
        let snap = make_book(
            &[(100.0, 100.0)],
            &[(101.0, 10.0), (102.0, 10.0), (103.0, 100.0)],
        );
        // Fill 25: 10@101 + 10@102 + 5@103 = (1010+1020+515)/25 = 2545/25 = 101.8
        // slippage = 101.8 - 101.0 = 0.8
        let impact = market_impact_bid(&snap, 25.0);
        assert!((impact - 0.8).abs() < 1e-10);
    }

    #[test]
    fn market_impact_ask_single_level() {
        let snap = make_book(&[(100.0, 100.0)], &[(101.0, 100.0)]);
        let impact = market_impact_ask(&snap, 50.0);
        assert!((impact - 0.0).abs() < 1e-10);
    }

    #[test]
    fn bid_ask_spread_and_mid() {
        let snap = make_book(&[(100.0, 1.0)], &[(101.0, 1.0)]);
        assert_eq!(snap.spread(), Some(1.0));
        assert_eq!(snap.mid_price(), Some(100.5));
    }

    #[test]
    fn depth_tracker_trend_increasing() {
        let tracker = DepthTracker::new();
        for i in 1..=10_u64 {
            let snap = make_book(
                &[(100.0, i as f64 * 10.0)],
                &[(101.0, 100.0)],
            );
            tracker.update("BTC", snap, 5);
        }
        let trend = tracker.imbalance_trend("BTC").expect("should have trend");
        assert!(trend > 0.0, "imbalance should be increasing");
    }

    #[test]
    fn depth_tracker_unknown_symbol_returns_none() {
        let tracker = DepthTracker::new();
        assert!(tracker.imbalance_trend("UNKNOWN").is_none());
    }

    #[test]
    fn compute_signal_returns_finite_values() {
        let snap = make_book(
            &[(100.0, 50.0), (99.0, 30.0)],
            &[(101.0, 40.0), (102.0, 20.0)],
        );
        let sig = compute_signal(&snap, 10.0);
        assert!(sig.imbalance.is_finite());
        assert!(sig.weighted_imbalance.is_finite());
        assert!(sig.bid_ask_spread.is_finite());
        assert!(sig.mid_price.is_finite());
    }

    #[test]
    fn weighted_imbalance_closer_levels_weighted_more() {
        // Huge volume at best bid, small volume at best ask → buying pressure
        let snap = make_book(
            &[(100.0, 1000.0), (99.0, 1.0)],
            &[(101.0, 10.0), (102.0, 1000.0)],
        );
        let wi = compute_weighted(&snap, 2);
        // Best bid has much more weight than distant ask level
        assert!(wi.imbalance > 0.0);
    }
}
