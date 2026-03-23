//! Trade Classifier — Lee-Ready Algorithm
//!
//! Classifies each tick as buyer-initiated or seller-initiated using the
//! Lee-Ready (1991) algorithm:
//! - Price > quote midpoint → BUY (buyer-initiated)
//! - Price < quote midpoint → SELL (seller-initiated)
//! - Price == quote midpoint → tick test (up-tick → BUY, down-tick → SELL)
//!
//! The [`TradeFlowAccumulator`] maintains a rolling window of classified ticks
//! and exposes aggregate [`TradeFlowMetrics`].

use std::collections::VecDeque;
use crate::tick::NormalizedTick;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// A bid-ask quote snapshot at the time of a trade.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// Best bid price.
    pub bid: Decimal,
    /// Best ask price.
    pub ask: Decimal,
    /// Quote midpoint: `(bid + ask) / 2`.
    pub mid: Decimal,
}

impl Quote {
    /// Construct a [`Quote`] and compute the midpoint.
    pub fn new(bid: Decimal, ask: Decimal) -> Self {
        let mid = (bid + ask) / Decimal::TWO;
        Self { bid, ask, mid }
    }
}

/// Classification of whether a trade was buyer- or seller-initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeClass {
    /// Trade price was above the quote midpoint (buyer aggressor).
    BuyInitiated,
    /// Trade price was below the quote midpoint (seller aggressor).
    SellInitiated,
    /// Classification could not be determined (e.g. no quote available and no previous trade).
    Unknown,
}

/// A tick enriched with its Lee-Ready classification and the prevailing quote.
#[derive(Debug, Clone)]
pub struct ClassifiedTick {
    /// The underlying normalized tick.
    pub tick: NormalizedTick,
    /// Lee-Ready classification.
    pub class: TradeClass,
    /// The quote in effect at trade time, if available.
    pub quote_at_trade: Option<Quote>,
}

/// The Lee-Ready trade classifier.
///
/// Maintains the last trade price for tick-test fallback.
pub struct LeeReadyClassifier {
    prev_price: Option<Decimal>,
}

impl LeeReadyClassifier {
    /// Create a new [`LeeReadyClassifier`].
    pub fn new() -> Self {
        Self { prev_price: None }
    }

    /// Classify `tick` given an optional prevailing `quote`.
    ///
    /// Algorithm:
    /// 1. If `quote` is `Some`, compare `tick.price` to `quote.mid`.
    ///    - `price > mid` → [`TradeClass::BuyInitiated`]
    ///    - `price < mid` → [`TradeClass::SellInitiated`]
    ///    - `price == mid` → fall through to tick test
    /// 2. Tick test: compare to `prev_price`.
    ///    - `price > prev_price` → [`TradeClass::BuyInitiated`]
    ///    - `price < prev_price` → [`TradeClass::SellInitiated`]
    ///    - equal or no previous trade → [`TradeClass::Unknown`]
    pub fn classify(&mut self, tick: &NormalizedTick, quote: Option<Quote>) -> ClassifiedTick {
        let price = tick.price;

        let class = if let Some(ref q) = quote {
            if price > q.mid {
                TradeClass::BuyInitiated
            } else if price < q.mid {
                TradeClass::SellInitiated
            } else {
                // Price at midpoint: apply tick test
                self.tick_test(price)
            }
        } else {
            // No quote: apply tick test
            self.tick_test(price)
        };

        self.prev_price = Some(price);

        ClassifiedTick {
            tick: tick.clone(),
            class,
            quote_at_trade: quote,
        }
    }

    fn tick_test(&self, price: Decimal) -> TradeClass {
        match self.prev_price {
            Some(prev) if price > prev => TradeClass::BuyInitiated,
            Some(prev) if price < prev => TradeClass::SellInitiated,
            _ => TradeClass::Unknown,
        }
    }

    /// Reset the classifier state (forget the previous trade price).
    pub fn reset(&mut self) {
        self.prev_price = None;
    }
}

impl Default for LeeReadyClassifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Aggregate trade flow metrics computed over a window of classified ticks.
#[derive(Debug, Clone, Default)]
pub struct TradeFlowMetrics {
    /// Total volume of buyer-initiated trades (sum of quantities).
    pub buy_volume: f64,
    /// Total volume of seller-initiated trades (sum of quantities).
    pub sell_volume: f64,
    /// Number of buyer-initiated trades.
    pub buy_count: u64,
    /// Number of seller-initiated trades.
    pub sell_count: u64,
    /// Order imbalance: `(buy_volume - sell_volume) / (buy_volume + sell_volume)`.
    /// Ranges from -1 (all sells) to +1 (all buys); `0.0` when total is zero.
    pub order_imbalance: f64,
}

/// Accumulates classified ticks in a rolling window and produces [`TradeFlowMetrics`].
pub struct TradeFlowAccumulator {
    window: VecDeque<ClassifiedTick>,
    capacity: usize,
}

impl TradeFlowAccumulator {
    /// Create a new accumulator with the given rolling window capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity + 1),
            capacity,
        }
    }

    /// Push a classified tick; evicts oldest if at capacity.
    pub fn push(&mut self, ct: ClassifiedTick) {
        self.window.push_back(ct);
        if self.window.len() > self.capacity {
            self.window.pop_front();
        }
    }

    /// Compute aggregate [`TradeFlowMetrics`] over the current window.
    pub fn metrics(&self) -> TradeFlowMetrics {
        let mut buy_volume = 0.0f64;
        let mut sell_volume = 0.0f64;
        let mut buy_count = 0u64;
        let mut sell_count = 0u64;

        for ct in &self.window {
            let qty = ct.tick.quantity.to_f64().unwrap_or(0.0);
            match ct.class {
                TradeClass::BuyInitiated => {
                    buy_volume += qty;
                    buy_count += 1;
                }
                TradeClass::SellInitiated => {
                    sell_volume += qty;
                    sell_count += 1;
                }
                TradeClass::Unknown => {}
            }
        }

        let total = buy_volume + sell_volume;
        let order_imbalance = if total > 1e-15 {
            (buy_volume - sell_volume) / total
        } else {
            0.0
        };

        TradeFlowMetrics {
            buy_volume,
            sell_volume,
            buy_count,
            sell_count,
            order_imbalance,
        }
    }

    /// Number of ticks in the current window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Returns `true` if the window is empty.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{Exchange, NormalizedTick, TradeSide};
    use rust_decimal_macros::dec;

    fn make_tick(price: Decimal, qty: Decimal) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price,
            quantity: qty,
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    fn make_quote(bid: Decimal, ask: Decimal) -> Quote {
        Quote::new(bid, ask)
    }

    #[test]
    fn test_quote_mid_computed_correctly() {
        let q = Quote::new(dec!(100), dec!(102));
        assert_eq!(q.mid, dec!(101));
    }

    #[test]
    fn test_buy_above_mid() {
        let mut clf = LeeReadyClassifier::new();
        let tick = make_tick(dec!(101.5), dec!(1));
        let q = make_quote(dec!(100), dec!(102)); // mid = 101
        let ct = clf.classify(&tick, Some(q));
        assert_eq!(ct.class, TradeClass::BuyInitiated);
    }

    #[test]
    fn test_sell_below_mid() {
        let mut clf = LeeReadyClassifier::new();
        let tick = make_tick(dec!(100.5), dec!(1));
        let q = make_quote(dec!(100), dec!(102)); // mid = 101
        let ct = clf.classify(&tick, Some(q));
        assert_eq!(ct.class, TradeClass::SellInitiated);
    }

    #[test]
    fn test_at_mid_falls_to_tick_test_uptick() {
        let mut clf = LeeReadyClassifier::new();
        // Set prev price
        let prev_tick = make_tick(dec!(100), dec!(1));
        clf.classify(&prev_tick, None);
        // Now trade at mid, price > prev → BUY
        let tick = make_tick(dec!(101), dec!(1));
        let q = make_quote(dec!(100), dec!(102)); // mid = 101
        let ct = clf.classify(&tick, Some(q));
        assert_eq!(ct.class, TradeClass::BuyInitiated);
    }

    #[test]
    fn test_at_mid_falls_to_tick_test_downtick() {
        let mut clf = LeeReadyClassifier::new();
        let prev_tick = make_tick(dec!(102), dec!(1));
        clf.classify(&prev_tick, None);
        // Now trade at mid, price < prev → SELL
        let tick = make_tick(dec!(101), dec!(1));
        let q = make_quote(dec!(100), dec!(102)); // mid = 101
        let ct = clf.classify(&tick, Some(q));
        assert_eq!(ct.class, TradeClass::SellInitiated);
    }

    #[test]
    fn test_no_quote_up_tick() {
        let mut clf = LeeReadyClassifier::new();
        let t1 = make_tick(dec!(100), dec!(1));
        clf.classify(&t1, None);
        let t2 = make_tick(dec!(101), dec!(1));
        let ct = clf.classify(&t2, None);
        assert_eq!(ct.class, TradeClass::BuyInitiated);
    }

    #[test]
    fn test_no_quote_down_tick() {
        let mut clf = LeeReadyClassifier::new();
        let t1 = make_tick(dec!(101), dec!(1));
        clf.classify(&t1, None);
        let t2 = make_tick(dec!(100), dec!(1));
        let ct = clf.classify(&t2, None);
        assert_eq!(ct.class, TradeClass::SellInitiated);
    }

    #[test]
    fn test_no_quote_no_prev_is_unknown() {
        let mut clf = LeeReadyClassifier::new();
        let tick = make_tick(dec!(100), dec!(1));
        let ct = clf.classify(&tick, None);
        assert_eq!(ct.class, TradeClass::Unknown);
    }

    #[test]
    fn test_equal_price_no_prev_is_unknown() {
        let mut clf = LeeReadyClassifier::new();
        let q = make_quote(dec!(99), dec!(101)); // mid = 100
        let tick = make_tick(dec!(100), dec!(1));
        let ct = clf.classify(&tick, Some(q));
        assert_eq!(ct.class, TradeClass::Unknown);
    }

    #[test]
    fn test_prev_price_updated_after_classify() {
        let mut clf = LeeReadyClassifier::new();
        let t1 = make_tick(dec!(100), dec!(1));
        clf.classify(&t1, None);
        assert_eq!(clf.prev_price, Some(dec!(100)));
        let t2 = make_tick(dec!(105), dec!(1));
        clf.classify(&t2, None);
        assert_eq!(clf.prev_price, Some(dec!(105)));
    }

    #[test]
    fn test_reset_clears_prev_price() {
        let mut clf = LeeReadyClassifier::new();
        let t1 = make_tick(dec!(100), dec!(1));
        clf.classify(&t1, None);
        clf.reset();
        assert!(clf.prev_price.is_none());
        let t2 = make_tick(dec!(105), dec!(1));
        let ct = clf.classify(&t2, None);
        assert_eq!(ct.class, TradeClass::Unknown);
    }

    #[test]
    fn test_classified_tick_preserves_original_tick() {
        let mut clf = LeeReadyClassifier::new();
        let tick = make_tick(dec!(150), dec!(2.5));
        let ct = clf.classify(&tick, None);
        assert_eq!(ct.tick.price, dec!(150));
        assert_eq!(ct.tick.quantity, dec!(2.5));
    }

    #[test]
    fn test_accumulator_caps_at_capacity() {
        let mut acc = TradeFlowAccumulator::new(5);
        let mut clf = LeeReadyClassifier::new();
        let base = make_tick(dec!(100), dec!(1));
        for i in 0..10i32 {
            let mut t = base.clone();
            t.price = Decimal::from(100 + i);
            let ct = clf.classify(&t, None);
            acc.push(ct);
        }
        assert_eq!(acc.len(), 5);
    }

    #[test]
    fn test_metrics_buy_sell_counts() {
        let mut acc = TradeFlowAccumulator::new(20);
        let mut clf = LeeReadyClassifier::new();
        // 5 upticks (BUY) then 3 downticks (SELL)
        for i in 0..5i32 {
            let t = make_tick(Decimal::from(100 + i), dec!(1));
            acc.push(clf.classify(&t, None));
        }
        for i in 0..3i32 {
            let t = make_tick(Decimal::from(104 - i), dec!(1));
            acc.push(clf.classify(&t, None));
        }
        let m = acc.metrics();
        assert!(m.buy_count >= 4, "expected >=4 buys, got {}", m.buy_count);
        assert!(m.sell_count >= 2, "expected >=2 sells, got {}", m.sell_count);
    }

    #[test]
    fn test_order_imbalance_range() {
        let mut acc = TradeFlowAccumulator::new(100);
        let mut clf = LeeReadyClassifier::new();
        for i in 0..20i32 {
            let sign = if i % 2 == 0 { 1 } else { -1 };
            let t = make_tick(Decimal::from(100 + sign), dec!(1));
            acc.push(clf.classify(&t, None));
        }
        let m = acc.metrics();
        assert!(m.order_imbalance >= -1.0 && m.order_imbalance <= 1.0,
            "imbalance out of range: {}", m.order_imbalance);
    }

    #[test]
    fn test_order_imbalance_all_buys() {
        let mut acc = TradeFlowAccumulator::new(20);
        let mut clf = LeeReadyClassifier::new();
        for i in 0..10i32 {
            let t = make_tick(Decimal::from(100 + i), dec!(2));
            acc.push(clf.classify(&t, None));
        }
        let m = acc.metrics();
        // All trades are buy-initiated (monotone rising) except the first (Unknown)
        // After first tick sets prev, rest are buys
        assert!(m.order_imbalance > 0.5, "expected strong buy imbalance, got {}", m.order_imbalance);
    }

    #[test]
    fn test_empty_accumulator_metrics() {
        let acc = TradeFlowAccumulator::new(10);
        let m = acc.metrics();
        assert_eq!(m.buy_count, 0);
        assert_eq!(m.sell_count, 0);
        assert_eq!(m.order_imbalance, 0.0);
    }

    #[test]
    fn test_trade_side_with_quote_no_tick_test_needed() {
        let mut clf = LeeReadyClassifier::new();
        // Ensure quote classification takes precedence over tick test
        let t1 = make_tick(dec!(200), dec!(1)); // set prev price high
        clf.classify(&t1, None);
        // New tick at 100, but quote says it's below mid 150 → SELL
        let t2 = make_tick(dec!(100), dec!(1));
        let q = make_quote(dec!(140), dec!(160)); // mid = 150, price 100 < 150 → SELL
        let ct = clf.classify(&t2, Some(q));
        assert_eq!(ct.class, TradeClass::SellInitiated);
    }

    #[test]
    fn test_volume_accumulated_correctly() {
        let mut acc = TradeFlowAccumulator::new(20);
        let mut clf = LeeReadyClassifier::new();
        // 3 buy ticks each with qty 2.0, from monotone rising prices
        for i in 0..4i32 {
            let t = make_tick(Decimal::from(100 + i), dec!(2));
            acc.push(clf.classify(&t, None));
        }
        let m = acc.metrics();
        // First tick is Unknown (no prev), so 3 buys × 2.0 = 6.0
        assert!((m.buy_volume - 6.0).abs() < 1e-6, "buy_volume = {}", m.buy_volume);
    }
}
