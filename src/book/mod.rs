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
#[derive(Debug, Clone)]
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

    /// The symbol this order book tracks.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// The sequence number of the most recently applied delta, if any.
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
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
}
