//! # Module: hft — High-Frequency Trading Signals
//!
//! ## Responsibility
//! Real-time HFT signals derived from raw trade and quote data streams:
//! order flow imbalance, micro-price estimation, toxic flow detection,
//! and quote/trade speed ratios.
//!
//! ## NOT Responsible For
//! - Order routing or execution
//! - Persistence or replay

use std::collections::VecDeque;

// ─────────────────────────────────────────
//  OrderFlowImbalance
// ─────────────────────────────────────────

/// Instantaneous order-flow imbalance from buy and sell volumes.
///
/// `imbalance = (buy_volume - sell_volume) / (buy_volume + sell_volume)`
///
/// Range `[-1.0, 1.0]`. Returns `0.0` when both volumes are zero.
///
/// # Example
/// ```rust
/// use fin_stream::hft::OrderFlowImbalance;
///
/// let ofi = OrderFlowImbalance { buy_volume: 600.0, sell_volume: 400.0 };
/// let imb = ofi.imbalance();
/// assert!((imb - 0.2).abs() < 1e-9, "imbalance={imb}");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct OrderFlowImbalance {
    /// Aggressive buy volume in the period.
    pub buy_volume: f64,
    /// Aggressive sell volume in the period.
    pub sell_volume: f64,
}

impl OrderFlowImbalance {
    /// Compute the order-flow imbalance ratio.
    ///
    /// Returns `0.0` when both `buy_volume` and `sell_volume` are zero.
    pub fn imbalance(&self) -> f64 {
        let total = self.buy_volume + self.sell_volume;
        if total == 0.0 {
            return 0.0;
        }
        (self.buy_volume - self.sell_volume) / total
    }
}

// ─────────────────────────────────────────
//  TradeSignAggregator
// ─────────────────────────────────────────

/// Accumulates signed trades in a rolling window and computes net order flow.
///
/// Each trade is submitted as a signed volume (`> 0` = buy, `< 0` = sell).
/// `net_flow()` returns the sum of all signed volumes in the current window.
///
/// # Example
/// ```rust
/// use fin_stream::hft::TradeSignAggregator;
///
/// let mut agg = TradeSignAggregator::new(3);
/// agg.push(100.0);   // buy
/// agg.push(-50.0);   // sell
/// agg.push(200.0);   // buy
/// assert!((agg.net_flow() - 250.0).abs() < 1e-9);
/// ```
#[derive(Debug)]
pub struct TradeSignAggregator {
    window: usize,
    buf: VecDeque<f64>,
}

impl TradeSignAggregator {
    /// Create a new aggregator with the given rolling window size.
    ///
    /// # Panics
    /// Panics if `window == 0`.
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "window must be > 0");
        Self { window, buf: VecDeque::with_capacity(window) }
    }

    /// Push a signed volume (`+` = buy, `-` = sell) into the window.
    pub fn push(&mut self, signed_volume: f64) {
        self.buf.push_back(signed_volume);
        if self.buf.len() > self.window {
            self.buf.pop_front();
        }
    }

    /// Return the net order flow (sum of signed volumes) in the current window.
    pub fn net_flow(&self) -> f64 {
        self.buf.iter().sum()
    }

    /// Return the number of observations currently held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Return `true` if the window is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Reset the aggregator, clearing all observations.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

// ─────────────────────────────────────────
//  MicroPriceEstimate
// ─────────────────────────────────────────

/// Volume-weighted mid price (micro-price) from top-of-book quantities.
///
/// `micro_price = (bid * ask_qty + ask * bid_qty) / (bid_qty + ask_qty)`
///
/// This weighting shifts the mid toward whichever side has less quantity
/// (thinner side implies more likely execution at that level).
///
/// # Example
/// ```rust
/// use fin_stream::hft::MicroPriceEstimate;
///
/// let mp = MicroPriceEstimate { bid: 99.0, ask: 101.0, bid_qty: 100.0, ask_qty: 300.0 };
/// let price = mp.micro_price();
/// // Asks are deeper, so micro-price is pulled toward the bid
/// assert!(price < 100.0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct MicroPriceEstimate {
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Quantity available at the best bid.
    pub bid_qty: f64,
    /// Quantity available at the best ask.
    pub ask_qty: f64,
}

impl MicroPriceEstimate {
    /// Compute the volume-weighted mid price.
    ///
    /// Returns the simple mid `(bid + ask) / 2` when both quantities are zero.
    pub fn micro_price(&self) -> f64 {
        let total_qty = self.bid_qty + self.ask_qty;
        if total_qty == 0.0 {
            return (self.bid + self.ask) / 2.0;
        }
        // Weight bid by ask_qty fraction and ask by bid_qty fraction
        (self.bid * self.ask_qty + self.ask * self.bid_qty) / total_qty
    }
}

// ─────────────────────────────────────────
//  ToxicFlowDetector
// ─────────────────────────────────────────

/// Detects informed (toxic) order flow via sign-change / price-impact correlations.
///
/// The toxicity score is the fraction of trades where the trade sign aligns with
/// the subsequent price direction (high = informed flow, low = noise).
///
/// `toxicity_score = price-moving_trades / total_trades`
///
/// # Example
/// ```rust
/// use fin_stream::hft::ToxicFlowDetector;
///
/// let mut det = ToxicFlowDetector::new();
/// // Buy followed by up-move: price-moving
/// det.update(1, 0.05);
/// // Sell followed by down-move: price-moving
/// det.update(-1, -0.05);
/// // Buy followed by no move: not price-moving
/// det.update(1, 0.0);
/// let score = det.toxicity_score();
/// assert!((score - 2.0 / 3.0).abs() < 1e-9, "score={score}");
/// ```
#[derive(Debug, Default)]
pub struct ToxicFlowDetector {
    total_trades: u64,
    price_moving_trades: u64,
}

impl ToxicFlowDetector {
    /// Create a new detector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a trade.
    ///
    /// - `trade_sign`: `+1` for buy-initiated, `-1` for sell-initiated, `0` for unknown.
    /// - `price_change`: price move after the trade (signed).
    ///
    /// A trade is "price-moving" when `trade_sign` and `price_change` have the same sign.
    pub fn update(&mut self, trade_sign: i8, price_change: f64) {
        self.total_trades += 1;
        let sign_consistent = match trade_sign {
            s if s > 0 => price_change > 0.0,
            s if s < 0 => price_change < 0.0,
            _ => false,
        };
        if sign_consistent {
            self.price_moving_trades += 1;
        }
    }

    /// Return the fraction of trades in the price-moving direction.
    ///
    /// Returns `0.0` when no trades have been recorded.
    pub fn toxicity_score(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.price_moving_trades as f64 / self.total_trades as f64
    }

    /// Total number of trades observed.
    pub fn total_trades(&self) -> u64 {
        self.total_trades
    }

    /// Number of trades classified as price-moving.
    pub fn price_moving_trades(&self) -> u64 {
        self.price_moving_trades
    }

    /// Reset all counters.
    pub fn reset(&mut self) {
        self.total_trades = 0;
        self.price_moving_trades = 0;
    }
}

// ─────────────────────────────────────────
//  SpeedOfQuote
// ─────────────────────────────────────────

/// Quote update and trade arrival speed metrics.
///
/// A high quote-to-trade ratio indicates market maker activity or quote stuffing.
/// A low ratio (close to 1) suggests heavy informed trading.
///
/// # Example
/// ```rust
/// use fin_stream::hft::SpeedOfQuote;
///
/// let sq = SpeedOfQuote { quote_rate_per_ms: 50.0, trade_rate_per_ms: 5.0 };
/// assert!((sq.quote_to_trade_ratio() - 10.0).abs() < 1e-9);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct SpeedOfQuote {
    /// Number of quote updates per millisecond.
    pub quote_rate_per_ms: f64,
    /// Number of trades per millisecond.
    pub trade_rate_per_ms: f64,
}

impl SpeedOfQuote {
    /// Compute the quote-to-trade ratio.
    ///
    /// Returns `0.0` when `trade_rate_per_ms == 0.0` to avoid division by zero.
    pub fn quote_to_trade_ratio(&self) -> f64 {
        if self.trade_rate_per_ms == 0.0 {
            return 0.0;
        }
        self.quote_rate_per_ms / self.trade_rate_per_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OrderFlowImbalance ──────────────────────────────────────────────────

    #[test]
    fn test_ofi_positive_buy_pressure() {
        let ofi = OrderFlowImbalance { buy_volume: 600.0, sell_volume: 400.0 };
        let imb = ofi.imbalance();
        assert!((imb - 0.2).abs() < 1e-9, "imbalance={imb}");
    }

    #[test]
    fn test_ofi_negative_sell_pressure() {
        let ofi = OrderFlowImbalance { buy_volume: 300.0, sell_volume: 700.0 };
        let imb = ofi.imbalance();
        assert!((imb - (-0.4)).abs() < 1e-9, "imbalance={imb}");
    }

    #[test]
    fn test_ofi_zero_both_volumes() {
        let ofi = OrderFlowImbalance { buy_volume: 0.0, sell_volume: 0.0 };
        assert_eq!(ofi.imbalance(), 0.0);
    }

    #[test]
    fn test_ofi_equal_volumes() {
        let ofi = OrderFlowImbalance { buy_volume: 500.0, sell_volume: 500.0 };
        assert_eq!(ofi.imbalance(), 0.0);
    }

    #[test]
    fn test_ofi_all_buys() {
        let ofi = OrderFlowImbalance { buy_volume: 1000.0, sell_volume: 0.0 };
        assert_eq!(ofi.imbalance(), 1.0);
    }

    #[test]
    fn test_ofi_all_sells() {
        let ofi = OrderFlowImbalance { buy_volume: 0.0, sell_volume: 1000.0 };
        assert_eq!(ofi.imbalance(), -1.0);
    }

    // ── TradeSignAggregator ─────────────────────────────────────────────────

    #[test]
    fn test_trade_sign_aggregator_net_flow() {
        let mut agg = TradeSignAggregator::new(5);
        agg.push(100.0);
        agg.push(-50.0);
        agg.push(200.0);
        assert!((agg.net_flow() - 250.0).abs() < 1e-9);
    }

    #[test]
    fn test_trade_sign_aggregator_rolling_eviction() {
        let mut agg = TradeSignAggregator::new(2);
        agg.push(100.0);
        agg.push(200.0);
        agg.push(50.0); // evicts 100
        // window: [200, 50] → net = 250
        assert!((agg.net_flow() - 250.0).abs() < 1e-9);
        assert_eq!(agg.len(), 2);
    }

    #[test]
    fn test_trade_sign_aggregator_empty() {
        let agg = TradeSignAggregator::new(3);
        assert_eq!(agg.net_flow(), 0.0);
        assert!(agg.is_empty());
    }

    #[test]
    fn test_trade_sign_aggregator_reset() {
        let mut agg = TradeSignAggregator::new(3);
        agg.push(100.0);
        agg.reset();
        assert!(agg.is_empty());
        assert_eq!(agg.net_flow(), 0.0);
    }

    // ── MicroPriceEstimate ──────────────────────────────────────────────────

    #[test]
    fn test_micro_price_pulls_toward_thin_side() {
        // More ask qty than bid qty → micro-price pulled toward bid (below mid)
        let mp = MicroPriceEstimate { bid: 99.0, ask: 101.0, bid_qty: 100.0, ask_qty: 300.0 };
        let price = mp.micro_price();
        let mid = 100.0;
        assert!(price < mid, "micro-price={price} should be below mid={mid}");
    }

    #[test]
    fn test_micro_price_equal_qty_is_mid() {
        let mp = MicroPriceEstimate { bid: 99.0, ask: 101.0, bid_qty: 200.0, ask_qty: 200.0 };
        let price = mp.micro_price();
        assert!((price - 100.0).abs() < 1e-9, "micro-price={price}");
    }

    #[test]
    fn test_micro_price_zero_qty_returns_simple_mid() {
        let mp = MicroPriceEstimate { bid: 99.0, ask: 101.0, bid_qty: 0.0, ask_qty: 0.0 };
        let price = mp.micro_price();
        assert!((price - 100.0).abs() < 1e-9, "micro-price={price}");
    }

    // ── ToxicFlowDetector ───────────────────────────────────────────────────

    #[test]
    fn test_toxic_flow_all_price_moving() {
        let mut det = ToxicFlowDetector::new();
        det.update(1, 0.05);
        det.update(-1, -0.03);
        assert_eq!(det.toxicity_score(), 1.0);
    }

    #[test]
    fn test_toxic_flow_none_price_moving() {
        let mut det = ToxicFlowDetector::new();
        det.update(1, -0.05);  // buy but price fell
        det.update(-1, 0.03);  // sell but price rose
        assert_eq!(det.toxicity_score(), 0.0);
    }

    #[test]
    fn test_toxic_flow_mixed() {
        let mut det = ToxicFlowDetector::new();
        det.update(1, 0.05);   // moving
        det.update(-1, -0.05); // moving
        det.update(1, 0.0);    // not moving (no price change)
        let score = det.toxicity_score();
        assert!((score - 2.0 / 3.0).abs() < 1e-9, "score={score}");
    }

    #[test]
    fn test_toxic_flow_no_trades() {
        let det = ToxicFlowDetector::new();
        assert_eq!(det.toxicity_score(), 0.0);
    }

    #[test]
    fn test_toxic_flow_reset() {
        let mut det = ToxicFlowDetector::new();
        det.update(1, 0.05);
        det.reset();
        assert_eq!(det.total_trades(), 0);
        assert_eq!(det.toxicity_score(), 0.0);
    }

    // ── SpeedOfQuote ────────────────────────────────────────────────────────

    #[test]
    fn test_speed_of_quote_ratio() {
        let sq = SpeedOfQuote { quote_rate_per_ms: 50.0, trade_rate_per_ms: 5.0 };
        assert!((sq.quote_to_trade_ratio() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_speed_of_quote_zero_trade_rate() {
        let sq = SpeedOfQuote { quote_rate_per_ms: 100.0, trade_rate_per_ms: 0.0 };
        assert_eq!(sq.quote_to_trade_ratio(), 0.0);
    }

    #[test]
    fn test_speed_of_quote_equal_rates() {
        let sq = SpeedOfQuote { quote_rate_per_ms: 10.0, trade_rate_per_ms: 10.0 };
        assert!((sq.quote_to_trade_ratio() - 1.0).abs() < 1e-9);
    }
}
