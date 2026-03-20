//! Order book — delta streaming with full reconstruction.
//!
//! ## Responsibility
//! Maintain a live order book per symbol by applying incremental deltas
//! received from exchange WebSocket feeds. Supports full snapshot reset
//! and crossed-book detection.
//!
//! ## Guarantees
//! - Deterministic: applying the same delta sequence always yields the same book
//! - Non-panicking: all mutations return Result
//! - Single-owner mutable access: all mutation methods take `&mut self`; wrap
//!   in `Arc<Mutex<OrderBook>>` or `Arc<RwLock<OrderBook>>` for shared
//!   concurrent access across threads

use crate::error::StreamError;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// Side of the order book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BookSide {
    /// Bid (buy) side.
    Bid,
    /// Ask (sell) side.
    Ask,
}

/// A single price level in the order book.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PriceLevel {
    /// Price of this level.
    pub price: Decimal,
    /// Resting quantity at this price.
    pub quantity: Decimal,
}

impl PriceLevel {
    /// Construct a price level from a price and resting quantity.
    pub fn new(price: Decimal, quantity: Decimal) -> Self {
        Self { price, quantity }
    }
}

/// Incremental order book update.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BookDelta {
    /// Symbol this delta applies to.
    pub symbol: String,
    /// Side of the book (bid or ask).
    pub side: BookSide,
    /// Price level to update. `quantity == 0` means remove the level.
    pub price: Decimal,
    /// New resting quantity at this price. Zero removes the level.
    pub quantity: Decimal,
    /// Optional exchange-assigned sequence number for gap detection.
    pub sequence: Option<u64>,
}

impl BookDelta {
    /// Construct a delta without a sequence number.
    ///
    /// Use [`BookDelta::with_sequence`] to attach the exchange sequence number
    /// when available; sequenced deltas enable gap detection.
    pub fn new(
        symbol: impl Into<String>,
        side: BookSide,
        price: Decimal,
        quantity: Decimal,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            price,
            quantity,
            sequence: None,
        }
    }

    /// Attach an exchange sequence number to this delta.
    pub fn with_sequence(mut self, seq: u64) -> Self {
        self.sequence = Some(seq);
        self
    }

    /// Returns `true` if this delta signals a level deletion (`quantity == 0`).
    ///
    /// Exchanges signal the removal of a price level by sending a delta with
    /// zero quantity. Checking `is_delete()` is clearer than comparing with
    /// `Decimal::ZERO` at every call site.
    pub fn is_delete(&self) -> bool {
        self.quantity.is_zero()
    }
}

impl std::fmt::Display for BookDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let side = match self.side {
            BookSide::Bid => "Bid",
            BookSide::Ask => "Ask",
        };
        match self.sequence {
            Some(seq) => write!(
                f,
                "{} {} {} x {} seq={}",
                self.symbol, side, self.price, self.quantity, seq
            ),
            None => write!(
                f,
                "{} {} {} x {}",
                self.symbol, side, self.price, self.quantity
            ),
        }
    }
}

/// Live order book for a single symbol.
pub struct OrderBook {
    symbol: String,
    bids: BTreeMap<Decimal, Decimal>, // price → quantity
    asks: BTreeMap<Decimal, Decimal>, // price → quantity
    last_sequence: Option<u64>,
}

impl OrderBook {
    /// Create an empty order book for the given symbol.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_sequence: None,
        }
    }

    /// Apply an incremental delta. quantity == 0 removes the level.
    ///
    /// # Errors
    ///
    /// - [`StreamError::BookReconstructionFailed`] if the delta's symbol does
    ///   not match this book.
    /// - [`StreamError::SequenceGap`] if the delta carries a sequence number
    ///   that is not exactly one greater than the last applied sequence.
    /// - [`StreamError::BookCrossed`] if applying the delta would leave the
    ///   best bid >= best ask.
    #[must_use = "errors from apply() must be handled to avoid missed gaps or crossed-book state"]
    pub fn apply(&mut self, delta: BookDelta) -> Result<(), StreamError> {
        if delta.symbol != self.symbol {
            return Err(StreamError::BookReconstructionFailed {
                symbol: self.symbol.clone(),
                reason: format!(
                    "delta symbol '{}' does not match book '{}'",
                    delta.symbol, self.symbol
                ),
            });
        }

        // Sequence gap detection: if both the book and the delta carry a
        // sequence number, they must be consecutive.
        if let (Some(last), Some(incoming)) = (self.last_sequence, delta.sequence) {
            let expected = last + 1;
            if incoming != expected {
                return Err(StreamError::SequenceGap {
                    symbol: self.symbol.clone(),
                    expected,
                    got: incoming,
                });
            }
        }

        let map = match delta.side {
            BookSide::Bid => &mut self.bids,
            BookSide::Ask => &mut self.asks,
        };
        if delta.quantity.is_zero() {
            map.remove(&delta.price);
        } else {
            map.insert(delta.price, delta.quantity);
        }
        if let Some(seq) = delta.sequence {
            self.last_sequence = Some(seq);
        }
        self.check_crossed()
    }

    /// Reset the book from a full snapshot, also resetting the sequence counter.
    #[must_use = "errors from reset() indicate a crossed snapshot and must be handled"]
    pub fn reset(
        &mut self,
        bids: Vec<PriceLevel>,
        asks: Vec<PriceLevel>,
    ) -> Result<(), StreamError> {
        self.bids.clear();
        self.asks.clear();
        self.last_sequence = None;
        for lvl in bids {
            if !lvl.quantity.is_zero() {
                self.bids.insert(lvl.price, lvl.quantity);
            }
        }
        for lvl in asks {
            if !lvl.quantity.is_zero() {
                self.asks.insert(lvl.price, lvl.quantity);
            }
        }
        self.check_crossed()
    }

    /// Best bid (highest).
    pub fn best_bid(&self) -> Option<PriceLevel> {
        self.bids
            .iter()
            .next_back()
            .map(|(p, q)| PriceLevel::new(*p, *q))
    }

    /// Best ask (lowest).
    pub fn best_ask(&self) -> Option<PriceLevel> {
        self.asks
            .iter()
            .next()
            .map(|(p, q)| PriceLevel::new(*p, *q))
    }

    /// Mid price.
    pub fn mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / Decimal::from(2))
    }

    /// Spread.
    pub fn spread(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some(ask - bid)
    }

    /// Number of bid levels.
    pub fn bid_depth(&self) -> usize {
        self.bids.len()
    }

    /// Number of ask levels.
    pub fn ask_depth(&self) -> usize {
        self.asks.len()
    }

    /// Total resting quantity across all bid levels.
    pub fn bid_volume_total(&self) -> Decimal {
        self.bids.values().copied().sum()
    }

    /// Total resting quantity across all ask levels.
    pub fn ask_volume_total(&self) -> Decimal {
        self.asks.values().copied().sum()
    }

    /// Spread as a percentage of the mid-price: `spread / mid × 100`.
    ///
    /// Returns `None` if either best bid or best ask is absent, or if the
    /// mid-price is zero.
    pub fn spread_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mid = self.mid_price()?;
        if mid.is_zero() {
            return None;
        }
        let spread = self.spread()?;
        (spread / mid * Decimal::from(100)).to_f64()
    }

    /// Total number of price levels across both sides of the book.
    ///
    /// Equivalent to `bid_depth() + ask_depth()`.
    pub fn total_depth(&self) -> usize {
        self.bids.len() + self.asks.len()
    }

    /// Total resting quantity across both sides of the book.
    ///
    /// Equivalent to `bid_volume_total() + ask_volume_total()`.
    pub fn total_volume(&self) -> Decimal {
        self.bid_volume_total() + self.ask_volume_total()
    }

    /// The symbol this order book tracks.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// The sequence number of the most recently applied delta, if any.
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    /// Returns `true` if a bid level exists at exactly `price`.
    pub fn contains_bid(&self, price: Decimal) -> bool {
        self.bids.contains_key(&price)
    }

    /// Returns `true` if an ask level exists at exactly `price`.
    pub fn contains_ask(&self, price: Decimal) -> bool {
        self.asks.contains_key(&price)
    }

    /// Returns the resting quantity at `price` on the bid side, or `None` if absent.
    pub fn volume_at_bid(&self, price: Decimal) -> Option<Decimal> {
        self.bids.get(&price).copied()
    }

    /// Returns the resting quantity at `price` on the ask side, or `None` if absent.
    pub fn volume_at_ask(&self, price: Decimal) -> Option<Decimal> {
        self.asks.get(&price).copied()
    }

    /// Number of resting price levels on the given side.
    ///
    /// Unified version of [`bid_depth`](Self::bid_depth) /
    /// [`ask_depth`](Self::ask_depth) for runtime dispatch by side.
    pub fn level_count(&self, side: BookSide) -> usize {
        match side {
            BookSide::Bid => self.bids.len(),
            BookSide::Ask => self.asks.len(),
        }
    }

    /// Resting quantity at an exact price level on the given side.
    ///
    /// Returns `None` if there is no resting order at that price. This is a
    /// unified alternative to calling `volume_at_bid` / `volume_at_ask`
    /// separately when the side is determined at runtime.
    pub fn depth_at_price(&self, price: Decimal, side: BookSide) -> Option<Decimal> {
        match side {
            BookSide::Bid => self.bids.get(&price).copied(),
            BookSide::Ask => self.asks.get(&price).copied(),
        }
    }

    /// Ratio of total bid volume to total ask volume: `bid_volume_total / ask_volume_total`.
    ///
    /// Returns `None` if the ask side is empty (to avoid division by zero).
    /// Values > 1.0 indicate more buy-side depth; < 1.0 indicates more sell-side depth.
    pub fn bid_ask_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ask = self.ask_volume_total();
        if ask.is_zero() {
            return None;
        }
        (self.bid_volume_total() / ask).to_f64()
    }

    /// All bid levels, sorted descending by price (highest first).
    ///
    /// Equivalent to `top_bids(usize::MAX)` but more expressive when you want
    /// the complete depth without specifying a level count.
    pub fn all_bids(&self) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// All ask levels, sorted ascending by price (lowest first).
    ///
    /// Equivalent to `top_asks(usize::MAX)` but more expressive when you want
    /// the complete depth without specifying a level count.
    pub fn all_asks(&self) -> Vec<PriceLevel> {
        self.asks
            .iter()
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// Top N bids (descending by price).
    pub fn top_bids(&self, n: usize) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// Top N asks (ascending by price).
    pub fn top_asks(&self, n: usize) -> Vec<PriceLevel> {
        self.asks
            .iter()
            .take(n)
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// Order-book imbalance at the best bid/ask: `(bid_qty - ask_qty) / (bid_qty + ask_qty)`.
    ///
    /// Returns a value in `[-1.0, 1.0]`:
    /// - `+1.0` means the entire resting quantity is on the bid side (maximum buy pressure).
    /// - `-1.0` means the entire resting quantity is on the ask side (maximum sell pressure).
    /// - `0.0` means perfectly balanced.
    ///
    /// Returns `None` if either side has no best level.
    pub fn imbalance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid_qty = self.best_bid()?.quantity;
        let ask_qty = self.best_ask()?.quantity;
        let total = bid_qty + ask_qty;
        if total.is_zero() {
            return None;
        }
        let imb = (bid_qty - ask_qty) / total;
        imb.to_f64()
    }

    /// Order-book imbalance using the top `n` levels on each side.
    ///
    /// `(Σ bid_qty - Σ ask_qty) / (Σ bid_qty + Σ ask_qty)` in `[-1.0, 1.0]`.
    ///
    /// Returns `None` if either side has no levels or total volume is zero.
    pub fn bid_ask_imbalance(&self, n: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid_vol: Decimal = self.top_bids(n).iter().map(|l| l.quantity).sum();
        let ask_vol: Decimal = self.top_asks(n).iter().map(|l| l.quantity).sum();
        if bid_vol.is_zero() || ask_vol.is_zero() {
            return None;
        }
        let total = bid_vol + ask_vol;
        if total.is_zero() {
            return None;
        }
        ((bid_vol - ask_vol) / total).to_f64()
    }

    /// Volume-weighted average price (VWAP) of the top `n` resting levels on `side`.
    ///
    /// `Σ(price × qty) / Σ(qty)`. Returns `None` if the side has no levels or
    /// total volume is zero.
    pub fn vwap(&self, side: BookSide, n: usize) -> Option<Decimal> {
        let levels = match side {
            BookSide::Bid => self.top_bids(n),
            BookSide::Ask => self.top_asks(n),
        };
        let total_vol: Decimal = levels.iter().map(|l| l.quantity).sum();
        if total_vol.is_zero() {
            return None;
        }
        let price_vol_sum: Decimal = levels.iter().map(|l| l.price * l.quantity).sum();
        Some(price_vol_sum / total_vol)
    }

    /// Walk the book on `side` and return the average fill price to absorb `target_volume`.
    ///
    /// Sweeps levels from best to worst until `target_volume` is consumed, computing
    /// the VWAP of the executed portion. If the book has less total volume than
    /// `target_volume`, returns the VWAP of all available liquidity anyway.
    ///
    /// Returns `None` if the side is empty or `target_volume` is zero.
    pub fn price_at_volume(&self, side: BookSide, target_volume: Decimal) -> Option<Decimal> {
        if target_volume.is_zero() {
            return None;
        }
        let levels: Vec<(Decimal, Decimal)> = match side {
            BookSide::Bid => self.bids.iter().rev().map(|(p, q)| (*p, *q)).collect(),
            BookSide::Ask => self.asks.iter().map(|(p, q)| (*p, *q)).collect(),
        };
        if levels.is_empty() {
            return None;
        }
        let mut remaining = target_volume;
        let mut notional = Decimal::ZERO;
        let mut filled = Decimal::ZERO;
        for (price, qty) in &levels {
            if remaining.is_zero() {
                break;
            }
            let take = (*qty).min(remaining);
            notional += price * take;
            filled += take;
            remaining -= take;
        }
        if filled.is_zero() {
            return None;
        }
        Some(notional / filled)
    }

    /// Volume imbalance over the top-`n` price levels on each side: `(bid_vol - ask_vol) / (bid_vol + ask_vol)`.
    ///
    /// Returns a value in `[-1, 1]`: positive means more resting bid volume, negative means
    /// more resting ask volume. Returns `None` if both sides have zero volume or `n == 0`.
    ///
    /// Unlike [`imbalance`](Self::imbalance) which only uses the best bid/ask quantity,
    /// `depth_imbalance` aggregates across up to `n` levels providing a broader picture of
    /// order book pressure.
    pub fn depth_imbalance(&self, n: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if n == 0 {
            return None;
        }
        let bid_vol: Decimal = self.bids.values().rev().take(n).copied().sum();
        let ask_vol: Decimal = self.asks.values().take(n).copied().sum();
        let total = bid_vol + ask_vol;
        if total.is_zero() {
            return None;
        }
        ((bid_vol - ask_vol) / total).to_f64()
    }

    /// Return a full snapshot of all bid and ask levels.
    ///
    /// The returned tuple is `(bids, asks)`:
    /// - `bids` are sorted descending by price (highest first).
    /// - `asks` are sorted ascending by price (lowest first).
    ///
    /// Use this after receiving a [`StreamError::SequenceGap`] to rebuild the
    /// book from a fresh exchange snapshot: call [`reset`](Self::reset) with
    /// the snapshot levels, then resume applying deltas from the new sequence.
    pub fn snapshot(&self) -> (Vec<PriceLevel>, Vec<PriceLevel>) {
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect();
        let asks = self
            .asks
            .iter()
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect();
        (bids, asks)
    }

    fn check_crossed(&self) -> Result<(), StreamError> {
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if bid.price >= ask.price {
                return Err(StreamError::BookCrossed {
                    symbol: self.symbol.clone(),
                    bid: bid.price,
                    ask: ask.price,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn book(symbol: &str) -> OrderBook {
        OrderBook::new(symbol)
    }

    fn delta(symbol: &str, side: BookSide, price: Decimal, qty: Decimal) -> BookDelta {
        BookDelta::new(symbol, side, price, qty)
    }

    #[test]
    fn test_order_book_apply_bid_level() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(50000));
    }

    #[test]
    fn test_order_book_apply_ask_level() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2)))
            .unwrap();
        assert_eq!(b.best_ask().unwrap().price, dec!(50100));
    }

    #[test]
    fn test_order_book_remove_level_with_zero_qty() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(0)))
            .unwrap();
        assert!(b.best_bid().is_none());
    }

    #[test]
    fn test_order_book_best_bid_is_highest() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(2)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(3)))
            .unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(50000));
    }

    #[test]
    fn test_order_book_best_ask_is_lowest() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50300), dec!(3)))
            .unwrap();
        assert_eq!(b.best_ask().unwrap().price, dec!(50100));
    }

    #[test]
    fn test_order_book_mid_price() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)))
            .unwrap();
        assert_eq!(b.mid_price().unwrap(), dec!(50050));
    }

    #[test]
    fn test_order_book_spread() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)))
            .unwrap();
        assert_eq!(b.spread().unwrap(), dec!(100));
    }

    #[test]
    fn test_order_book_crossed_returns_error() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50000), dec!(1)))
            .unwrap();
        let result = b.apply(delta("BTC-USD", BookSide::Bid, dec!(50001), dec!(1)));
        assert!(matches!(result, Err(StreamError::BookCrossed { .. })));
    }

    #[test]
    fn test_order_book_wrong_symbol_delta_rejected() {
        let mut b = book("BTC-USD");
        let result = b.apply(delta("ETH-USD", BookSide::Bid, dec!(3000), dec!(1)));
        assert!(matches!(
            result,
            Err(StreamError::BookReconstructionFailed { .. })
        ));
    }

    #[test]
    fn test_order_book_reset_clears_and_reloads() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49000), dec!(5)))
            .unwrap();
        b.reset(
            vec![PriceLevel::new(dec!(50000), dec!(1))],
            vec![PriceLevel::new(dec!(50100), dec!(1))],
        )
        .unwrap();
        assert_eq!(b.bid_depth(), 1);
        assert_eq!(b.best_bid().unwrap().price, dec!(50000));
    }

    #[test]
    fn test_order_book_reset_ignores_zero_qty_levels() {
        let mut b = book("BTC-USD");
        b.reset(
            vec![
                PriceLevel::new(dec!(50000), dec!(1)),
                PriceLevel::new(dec!(49900), dec!(0)),
            ],
            vec![PriceLevel::new(dec!(50100), dec!(1))],
        )
        .unwrap();
        assert_eq!(b.bid_depth(), 1);
    }

    #[test]
    fn test_order_book_reset_clears_sequence() {
        let mut b = book("BTC-USD");
        b.apply(
            delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1)).with_sequence(5),
        )
        .unwrap();
        assert_eq!(b.last_sequence(), Some(5));
        b.reset(
            vec![PriceLevel::new(dec!(50000), dec!(1))],
            vec![PriceLevel::new(dec!(50100), dec!(1))],
        )
        .unwrap();
        assert_eq!(b.last_sequence(), None);
    }

    #[test]
    fn test_order_book_depth_counts() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)))
            .unwrap();
        assert_eq!(b.bid_depth(), 2);
        assert_eq!(b.ask_depth(), 1);
    }

    #[test]
    fn test_order_book_top_bids_descending() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(3)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(2)))
            .unwrap();
        let top = b.top_bids(2);
        assert_eq!(top[0].price, dec!(50000));
        assert_eq!(top[1].price, dec!(49900));
    }

    #[test]
    fn test_order_book_top_asks_ascending() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50300), dec!(3)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(2)))
            .unwrap();
        let top = b.top_asks(2);
        assert_eq!(top[0].price, dec!(50100));
        assert_eq!(top[1].price, dec!(50200));
    }

    #[test]
    fn test_book_delta_with_sequence() {
        let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)).with_sequence(42);
        assert_eq!(d.sequence, Some(42));
    }

    #[test]
    fn test_order_book_sequence_tracking() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)).with_sequence(7))
            .unwrap();
        assert_eq!(b.last_sequence(), Some(7));
    }

    #[test]
    fn test_order_book_sequence_gap_detected() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1)).with_sequence(1))
            .unwrap();
        // Skip sequence 2, send 3 → gap
        let result =
            b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)).with_sequence(3));
        assert!(matches!(
            result,
            Err(StreamError::SequenceGap { expected: 2, got: 3, .. })
        ));
    }

    #[test]
    fn test_order_book_sequential_deltas_accepted() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1)).with_sequence(1))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1)).with_sequence(2))
            .unwrap();
        assert_eq!(b.last_sequence(), Some(2));
    }

    #[test]
    fn test_order_book_mid_price_empty_returns_none() {
        let b = book("BTC-USD");
        assert!(b.mid_price().is_none());
    }

    #[test]
    fn test_price_level_new() {
        let lvl = PriceLevel::new(dec!(100), dec!(5));
        assert_eq!(lvl.price, dec!(100));
        assert_eq!(lvl.quantity, dec!(5));
    }

    #[test]
    fn test_contains_bid_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        assert!(b.contains_bid(dec!(50000)));
        assert!(!b.contains_bid(dec!(49999)));
    }

    #[test]
    fn test_contains_ask_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2)))
            .unwrap();
        assert!(b.contains_ask(dec!(50100)));
        assert!(!b.contains_ask(dec!(50200)));
    }

    #[test]
    fn test_contains_bid_removed_after_zero_qty() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(0)))
            .unwrap();
        assert!(!b.contains_bid(dec!(50000)));
    }

    #[test]
    fn test_book_delta_serde_roundtrip() {
        let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1))
            .with_sequence(42);
        let json = serde_json::to_string(&d).unwrap();
        let d2: BookDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(d2.symbol, "BTC-USD");
        assert_eq!(d2.price, dec!(50000));
        assert_eq!(d2.sequence, Some(42));
    }

    #[test]
    fn test_volume_at_bid_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(3)))
            .unwrap();
        assert_eq!(b.volume_at_bid(dec!(50000)), Some(dec!(3)));
        assert_eq!(b.volume_at_bid(dec!(49999)), None);
    }

    #[test]
    fn test_volume_at_ask_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(5)))
            .unwrap();
        assert_eq!(b.volume_at_ask(dec!(50100)), Some(dec!(5)));
        assert_eq!(b.volume_at_ask(dec!(50200)), None);
    }

    #[test]
    fn test_book_delta_display_with_sequence() {
        let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1))
            .with_sequence(42);
        let s = d.to_string();
        assert!(s.contains("BTC-USD"));
        assert!(s.contains("Bid"));
        assert!(s.contains("seq=42"));
    }

    #[test]
    fn test_book_delta_display_without_sequence() {
        let d = BookDelta::new("ETH-USD", BookSide::Ask, dec!(3000), dec!(2));
        let s = d.to_string();
        assert!(s.contains("ETH-USD"));
        assert!(s.contains("Ask"));
        assert!(!s.contains("seq="));
    }

    #[test]
    fn test_book_delta_is_delete_zero_qty() {
        let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(0));
        assert!(d.is_delete());
    }

    #[test]
    fn test_book_delta_is_delete_nonzero_qty() {
        let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1));
        assert!(!d.is_delete());
    }

    #[test]
    fn test_snapshot_bids_descending_asks_ascending() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(2)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(1)))
            .unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(3)))
            .unwrap();
        let (bids, asks) = b.snapshot();
        assert_eq!(bids[0].price, dec!(50000));
        assert_eq!(bids[1].price, dec!(49800));
        assert_eq!(asks[0].price, dec!(50100));
        assert_eq!(asks[1].price, dec!(50200));
    }

    // ── bid_ask_imbalance ─────────────────────────────────────────────────────

    #[test]
    fn test_bid_ask_imbalance_balanced() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1))).unwrap();
        let imb = b.bid_ask_imbalance(1).unwrap();
        assert!((imb).abs() < 1e-9, "equal qty → ~0");
    }

    #[test]
    fn test_bid_ask_imbalance_full_bid_pressure() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(10))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(0))).unwrap();
        // ask qty = 0 → None
        assert!(b.bid_ask_imbalance(1).is_none());
    }

    #[test]
    fn test_bid_ask_imbalance_two_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(2))).unwrap();
        // bid_vol = 4, ask_vol = 4 → imbalance = 0
        let imb = b.bid_ask_imbalance(2).unwrap();
        assert!((imb).abs() < 1e-9);
    }

    // ── vwap ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_vwap_single_level_equals_price() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(5))).unwrap();
        assert_eq!(b.vwap(BookSide::Ask, 1), Some(dec!(50100)));
    }

    #[test]
    fn test_vwap_two_equal_qty_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(1))).unwrap();
        // vwap = (50000 + 49800) / 2 = 49900
        assert_eq!(b.vwap(BookSide::Bid, 2), Some(dec!(49900)));
    }

    #[test]
    fn test_vwap_empty_side_returns_none() {
        let b = book("BTC-USD");
        assert!(b.vwap(BookSide::Ask, 5).is_none());
    }

    // ── OrderBook::depth_at_price / bid_ask_ratio ─────────────────────────────

    #[test]
    fn test_depth_at_price_bid_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(3))).unwrap();
        assert_eq!(b.depth_at_price(dec!(50000), BookSide::Bid), Some(dec!(3)));
    }

    #[test]
    fn test_depth_at_price_ask_present() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2))).unwrap();
        assert_eq!(b.depth_at_price(dec!(50100), BookSide::Ask), Some(dec!(2)));
    }

    #[test]
    fn test_depth_at_price_absent_returns_none() {
        let b = book("BTC-USD");
        assert!(b.depth_at_price(dec!(99999), BookSide::Bid).is_none());
        assert!(b.depth_at_price(dec!(99999), BookSide::Ask).is_none());
    }

    #[test]
    fn test_bid_ask_ratio_equal_sides() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(5))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(5))).unwrap();
        let ratio = b.bid_ask_ratio().unwrap();
        assert!((ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_bid_ask_ratio_more_bids() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(10))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(5))).unwrap();
        let ratio = b.bid_ask_ratio().unwrap();
        assert!((ratio - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_bid_ask_ratio_no_asks_returns_none() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(5))).unwrap();
        assert!(b.bid_ask_ratio().is_none());
    }

    // ── OrderBook::all_bids / all_asks ────────────────────────────────────────

    #[test]
    fn test_all_bids_sorted_descending() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(3))).unwrap();
        let bids = b.all_bids();
        assert_eq!(bids.len(), 3);
        assert_eq!(bids[0].price, dec!(50000));
        assert_eq!(bids[1].price, dec!(49900));
        assert_eq!(bids[2].price, dec!(49800));
    }

    #[test]
    fn test_all_asks_sorted_ascending() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50300), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(3))).unwrap();
        let asks = b.all_asks();
        assert_eq!(asks.len(), 3);
        assert_eq!(asks[0].price, dec!(50100));
        assert_eq!(asks[1].price, dec!(50200));
        assert_eq!(asks[2].price, dec!(50300));
    }

    #[test]
    fn test_all_bids_empty_returns_empty() {
        let b = book("BTC-USD");
        assert!(b.all_bids().is_empty());
    }

    // ── spread_pct / total_depth / total_volume ──────────────────────────────

    #[test]
    fn test_spread_pct_basic() {
        // bid=100, ask=101 → spread=1, mid=100.5 → pct ≈ 0.995%
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        let pct = b.spread_pct().unwrap();
        assert!((pct - 100.0 / 100.5).abs() < 1e-9, "got {pct}");
    }

    #[test]
    fn test_spread_pct_empty_book_returns_none() {
        let b = book("X");
        assert!(b.spread_pct().is_none());
    }

    #[test]
    fn test_total_depth_counts_both_sides() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(98), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert_eq!(b.total_depth(), 3);
    }

    #[test]
    fn test_total_depth_empty_is_zero() {
        let b = book("X");
        assert_eq!(b.total_depth(), 0);
    }

    #[test]
    fn test_total_volume_sums_both_sides() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(3))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(5))).unwrap();
        assert_eq!(b.total_volume(), dec!(8));
    }

    #[test]
    fn test_total_volume_empty_is_zero() {
        let b = book("X");
        assert_eq!(b.total_volume(), dec!(0));
    }

    // ── OrderBook::level_count ────────────────────────────────────────────────

    #[test]
    fn test_level_count_empty() {
        let b = book("BTC-USD");
        assert_eq!(b.level_count(BookSide::Bid), 0);
        assert_eq!(b.level_count(BookSide::Ask), 0);
    }

    #[test]
    fn test_level_count_matches_depth_methods() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        assert_eq!(b.level_count(BookSide::Bid), b.bid_depth());
        assert_eq!(b.level_count(BookSide::Ask), b.ask_depth());
    }
}
