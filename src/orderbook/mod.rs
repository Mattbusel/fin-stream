//! Order Book Reconstruction.
//!
//! Maintains a live L2 order book per symbol by applying incremental
//! [`BookUpdate`] deltas received from exchange WebSocket feeds.
//!
//! ## Design
//!
//! - Bids stored in a `BTreeMap<OrdF64, f64>` (descending order via Reverse key).
//! - Asks stored in a `BTreeMap<OrdF64, f64>` (ascending order).
//! - Sequence validation: gaps and stale updates are returned as [`BookError`].
//! - Crossed-book detection after each update.
//! - [`OrderBookManager`] wraps a `DashMap` for concurrent multi-symbol access.
//!
//! ## Guarantees
//!
//! - Non-panicking: all fallible paths return `Result<_, BookError>`.
//! - Deterministic: same update sequence always yields the same book state.

use dashmap::DashMap;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

// ─── OrdF64 ───────────────────────────────────────────────────────────────────

/// A wrapper around `f64` that implements `Ord` via `f64::total_cmp`.
///
/// Used as the key type in the bid/ask `BTreeMap`s so that `f64` prices
/// can be compared without panicking on NaN.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrdF64(pub f64);

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

// ─── PriceLevel ───────────────────────────────────────────────────────────────

/// A single price level in the order book.
#[derive(Debug, Clone)]
pub struct PriceLevel {
    /// Price of this level.
    pub price: f64,
    /// Resting quantity at this price.
    pub quantity: f64,
}

// ─── BookError ────────────────────────────────────────────────────────────────

/// Errors returned by order book operations.
#[derive(Debug, thiserror::Error)]
pub enum BookError {
    /// A sequence gap was detected — one or more updates were skipped.
    #[error("Sequence gap for '{symbol}': expected {expected}, got {got}")]
    SequenceGap {
        /// Symbol whose sequence has a gap.
        symbol: String,
        /// Expected sequence number.
        expected: u64,
        /// Received sequence number.
        got: u64,
    },

    /// A stale update was received (sequence <= current).
    #[error("Stale update for '{symbol}': current sequence {current}, got {got}")]
    StaleUpdate {
        /// Symbol.
        symbol: String,
        /// Current (highest seen) sequence number.
        current: u64,
        /// Received sequence number.
        got: u64,
    },

    /// The book is crossed (best bid >= best ask) after applying the update.
    #[error("Crossed book for '{symbol}': best bid {bid} >= best ask {ask}")]
    CrossedBook {
        /// Symbol.
        symbol: String,
        /// Best bid price.
        bid: f64,
        /// Best ask price.
        ask: f64,
    },
}

// ─── BookUpdate ───────────────────────────────────────────────────────────────

/// An incremental order book update (delta) from an exchange feed.
///
/// Each `(price, qty)` pair in `bids` or `asks` is interpreted as:
/// - `qty > 0`: set the resting quantity at `price` to `qty`.
/// - `qty == 0`: remove the level at `price`.
#[derive(Debug, Clone)]
pub struct BookUpdate {
    /// Instrument symbol.
    pub symbol: String,
    /// Sequence number for gap detection. Set to 0 to skip sequence validation.
    pub sequence: u64,
    /// Bid-side changes: `(price, qty)`.
    pub bids: Vec<(f64, f64)>,
    /// Ask-side changes: `(price, qty)`.
    pub asks: Vec<(f64, f64)>,
}

// ─── OrderBook ────────────────────────────────────────────────────────────────

/// A live L2 order book for a single symbol.
///
/// Bids are stored with a `Reverse` wrapper so that iteration yields descending
/// order (highest bid first). Asks are stored in ascending order (lowest ask first).
#[derive(Debug, Clone)]
pub struct OrderBook {
    /// Instrument symbol.
    pub symbol: String,
    /// Bid side: key = `OrdF64(price)`, stored ascending but queried from the top
    /// via `iter().rev()`.
    pub bids: BTreeMap<OrdF64, f64>,
    /// Ask side: key = `OrdF64(price)`, ascending order (lowest ask first via `iter()`).
    pub asks: BTreeMap<OrdF64, f64>,
    /// Most recently applied sequence number.
    pub sequence: u64,
    /// Wall-clock ms timestamp of the last applied update.
    pub last_updated_ms: u64,
}

impl OrderBook {
    /// Creates a new, empty order book for `symbol`.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            sequence: 0,
            last_updated_ms: 0,
        }
    }

    /// Returns the best (highest) bid level, or `None` if the bid side is empty.
    pub fn best_bid(&self) -> Option<PriceLevel> {
        self.bids.iter().next_back().map(|(k, &q)| PriceLevel { price: k.0, quantity: q })
    }

    /// Returns the best (lowest) ask level, or `None` if the ask side is empty.
    pub fn best_ask(&self) -> Option<PriceLevel> {
        self.asks.iter().next().map(|(k, &q)| PriceLevel { price: k.0, quantity: q })
    }

    /// Returns the bid-ask spread (`best_ask.price - best_bid.price`).
    pub fn spread(&self) -> Option<f64> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some(ask - bid)
    }

    /// Returns the mid-price: `(best_bid + best_ask) / 2`.
    pub fn mid_price(&self) -> Option<f64> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / 2.0)
    }

    /// Returns the top `levels` bid levels (descending) and top `levels` ask levels (ascending).
    pub fn depth(&self, levels: usize) -> (Vec<PriceLevel>, Vec<PriceLevel>) {
        let bids: Vec<PriceLevel> = self
            .bids
            .iter()
            .rev()
            .take(levels)
            .map(|(k, &q)| PriceLevel { price: k.0, quantity: q })
            .collect();
        let asks: Vec<PriceLevel> = self
            .asks
            .iter()
            .take(levels)
            .map(|(k, &q)| PriceLevel { price: k.0, quantity: q })
            .collect();
        (bids, asks)
    }

    /// Order book imbalance using top-5 levels on each side.
    ///
    /// ```text
    /// imbalance = (bid_qty_top5 - ask_qty_top5) / (bid_qty_top5 + ask_qty_top5)
    /// ```
    ///
    /// Returns `0.0` if both sides are empty.
    pub fn imbalance(&self) -> f64 {
        let bid_qty: f64 = self.bids.iter().rev().take(5).map(|(_, &q)| q).sum();
        let ask_qty: f64 = self.asks.iter().take(5).map(|(_, &q)| q).sum();
        let total = bid_qty + ask_qty;
        if total < 1e-15 {
            0.0
        } else {
            (bid_qty - ask_qty) / total
        }
    }

    /// Apply an incremental [`BookUpdate`] to this book.
    ///
    /// ## Sequence validation
    ///
    /// If `update.sequence == 0` validation is skipped (snapshot/unsequenced feeds).
    /// Otherwise:
    /// - `update.sequence <= self.sequence` → `BookError::StaleUpdate`
    /// - `update.sequence > self.sequence + 1` → `BookError::SequenceGap`
    ///
    /// ## Crossed-book check
    ///
    /// After applying all deltas, if `best_bid >= best_ask` the update is
    /// rejected and `BookError::CrossedBook` is returned. The book state is
    /// NOT rolled back (updates are applied before the check) — callers
    /// managing critical state should clone before calling.
    pub fn apply_update(&mut self, update: &BookUpdate) -> Result<(), BookError> {
        // Sequence validation (skip if sequence == 0).
        if update.sequence != 0 {
            if update.sequence <= self.sequence {
                return Err(BookError::StaleUpdate {
                    symbol: self.symbol.clone(),
                    current: self.sequence,
                    got: update.sequence,
                });
            }
            if update.sequence > self.sequence + 1 && self.sequence != 0 {
                return Err(BookError::SequenceGap {
                    symbol: self.symbol.clone(),
                    expected: self.sequence + 1,
                    got: update.sequence,
                });
            }
        }

        // Apply bid deltas.
        for &(price, qty) in &update.bids {
            let key = OrdF64(price);
            if qty == 0.0 {
                self.bids.remove(&key);
            } else {
                self.bids.insert(key, qty);
            }
        }

        // Apply ask deltas.
        for &(price, qty) in &update.asks {
            let key = OrdF64(price);
            if qty == 0.0 {
                self.asks.remove(&key);
            } else {
                self.asks.insert(key, qty);
            }
        }

        // Update sequence and timestamp.
        if update.sequence != 0 {
            self.sequence = update.sequence;
        }
        self.last_updated_ms = update.sequence; // use sequence as proxy when no wall-clock given

        // Crossed-book check.
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if bid.price >= ask.price {
                return Err(BookError::CrossedBook {
                    symbol: self.symbol.clone(),
                    bid: bid.price,
                    ask: ask.price,
                });
            }
        }

        Ok(())
    }

    /// Resets the book to an empty state (e.g. on reconnect).
    pub fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.sequence = 0;
        self.last_updated_ms = 0;
    }
}

// ─── OrderBookManager ─────────────────────────────────────────────────────────

/// Manages a collection of [`OrderBook`]s, one per symbol.
///
/// Thread-safe via `DashMap`; suitable for use from multiple async tasks.
#[derive(Clone, Default)]
pub struct OrderBookManager {
    books: Arc<DashMap<String, OrderBook>>,
}

impl OrderBookManager {
    /// Creates a new, empty [`OrderBookManager`].
    pub fn new() -> Self {
        Self { books: Arc::new(DashMap::new()) }
    }

    /// Applies a [`BookUpdate`] to the appropriate symbol's book.
    ///
    /// Creates the book if it does not yet exist.
    pub fn apply(&self, update: &BookUpdate) -> Result<(), BookError> {
        let mut entry = self
            .books
            .entry(update.symbol.clone())
            .or_insert_with(|| OrderBook::new(&update.symbol));
        entry.apply_update(update)
    }

    /// Returns a cloned snapshot of the book for `symbol`, or `None` if unknown.
    pub fn get(&self, symbol: &str) -> Option<OrderBook> {
        self.books.get(symbol).map(|b| b.clone())
    }

    /// Returns the number of symbols tracked.
    pub fn symbol_count(&self) -> usize {
        self.books.len()
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn update(seq: u64, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)>) -> BookUpdate {
        BookUpdate { symbol: "BTC".to_string(), sequence: seq, bids, asks }
    }

    fn snapshot_book() -> OrderBook {
        let mut book = OrderBook::new("BTC");
        // Bids: 100@10, 99@5, 98@8
        // Asks: 101@3, 102@7, 103@12
        book.apply_update(&BookUpdate {
            symbol: "BTC".to_string(),
            sequence: 0,
            bids: vec![(100.0, 10.0), (99.0, 5.0), (98.0, 8.0)],
            asks: vec![(101.0, 3.0), (102.0, 7.0), (103.0, 12.0)],
        }).unwrap();
        book.sequence = 1;
        book
    }

    // ── OrdF64 ────────────────────────────────────────────────────────────

    #[test]
    fn ord_f64_ordering() {
        assert!(OrdF64(1.0) < OrdF64(2.0));
        assert!(OrdF64(2.0) > OrdF64(1.0));
        assert_eq!(OrdF64(1.5), OrdF64(1.5));
    }

    // ── best_bid / best_ask ───────────────────────────────────────────────

    #[test]
    fn best_bid_returns_highest() {
        let book = snapshot_book();
        let bid = book.best_bid().expect("bid");
        assert_eq!(bid.price, 100.0);
        assert_eq!(bid.quantity, 10.0);
    }

    #[test]
    fn best_ask_returns_lowest() {
        let book = snapshot_book();
        let ask = book.best_ask().expect("ask");
        assert_eq!(ask.price, 101.0);
        assert_eq!(ask.quantity, 3.0);
    }

    #[test]
    fn best_bid_none_when_empty() {
        let book = OrderBook::new("X");
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn best_ask_none_when_empty() {
        let book = OrderBook::new("X");
        assert!(book.best_ask().is_none());
    }

    // ── spread / mid_price ───────────────────────────────────────────────

    #[test]
    fn spread_correct() {
        let book = snapshot_book();
        let spread = book.spread().expect("spread");
        assert!((spread - 1.0).abs() < 1e-9, "spread={spread}");
    }

    #[test]
    fn mid_price_correct() {
        let book = snapshot_book();
        let mid = book.mid_price().expect("mid");
        assert!((mid - 100.5).abs() < 1e-9, "mid={mid}");
    }

    #[test]
    fn spread_none_when_no_asks() {
        let mut book = OrderBook::new("X");
        book.bids.insert(OrdF64(100.0), 1.0);
        assert!(book.spread().is_none());
    }

    // ── depth ────────────────────────────────────────────────────────────

    #[test]
    fn depth_returns_top_n_levels() {
        let book = snapshot_book();
        let (bids, asks) = book.depth(2);
        assert_eq!(bids.len(), 2);
        assert_eq!(asks.len(), 2);
        assert_eq!(bids[0].price, 100.0);
        assert_eq!(bids[1].price, 99.0);
        assert_eq!(asks[0].price, 101.0);
        assert_eq!(asks[1].price, 102.0);
    }

    #[test]
    fn depth_returns_all_when_fewer_levels() {
        let book = snapshot_book();
        let (bids, asks) = book.depth(10);
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
    }

    #[test]
    fn depth_zero_returns_empty() {
        let book = snapshot_book();
        let (bids, asks) = book.depth(0);
        assert!(bids.is_empty());
        assert!(asks.is_empty());
    }

    // ── imbalance ────────────────────────────────────────────────────────

    #[test]
    fn imbalance_empty_book_is_zero() {
        let book = OrderBook::new("X");
        assert_eq!(book.imbalance(), 0.0);
    }

    #[test]
    fn imbalance_bid_heavy_positive() {
        let mut book = OrderBook::new("X");
        book.bids.insert(OrdF64(100.0), 80.0);
        book.asks.insert(OrdF64(101.0), 20.0);
        let imb = book.imbalance();
        assert!(imb > 0.0, "imbalance={imb}");
        assert!((imb - 0.6).abs() < 1e-9, "imbalance={imb}");
    }

    #[test]
    fn imbalance_ask_heavy_negative() {
        let mut book = OrderBook::new("X");
        book.bids.insert(OrdF64(100.0), 20.0);
        book.asks.insert(OrdF64(101.0), 80.0);
        let imb = book.imbalance();
        assert!(imb < 0.0, "imbalance={imb}");
        assert!((imb - (-0.6)).abs() < 1e-9);
    }

    #[test]
    fn imbalance_uses_top5_levels() {
        let mut book = OrderBook::new("X");
        // 10 bid levels each 10 qty = 100 total, but only top 5 count = 50
        for i in 0..10 {
            book.bids.insert(OrdF64(100.0 - i as f64), 10.0);
        }
        book.asks.insert(OrdF64(101.0), 50.0);
        let imb = book.imbalance(); // 50 vs 50 → 0
        assert!((imb).abs() < 1e-9, "imbalance={imb}");
    }

    // ── apply_update ─────────────────────────────────────────────────────

    #[test]
    fn apply_update_adds_levels() {
        let mut book = OrderBook::new("BTC");
        book.apply_update(&update(0, vec![(100.0, 5.0)], vec![(101.0, 3.0)])).unwrap();
        assert!(book.best_bid().is_some());
        assert!(book.best_ask().is_some());
    }

    #[test]
    fn apply_update_removes_level_when_qty_zero() {
        let mut book = snapshot_book();
        book.apply_update(&update(2, vec![(100.0, 0.0)], vec![])).unwrap();
        let bid = book.best_bid().expect("bid");
        assert_eq!(bid.price, 99.0); // 100 was removed
    }

    #[test]
    fn apply_update_updates_qty() {
        let mut book = snapshot_book();
        book.apply_update(&update(2, vec![(100.0, 99.0)], vec![])).unwrap();
        let bid = book.best_bid().expect("bid");
        assert_eq!(bid.quantity, 99.0);
    }

    #[test]
    fn apply_update_stale_rejected() {
        let mut book = snapshot_book(); // sequence = 1
        let err = book.apply_update(&update(1, vec![], vec![])).unwrap_err();
        assert!(matches!(err, BookError::StaleUpdate { .. }));
    }

    #[test]
    fn apply_update_gap_rejected() {
        let mut book = snapshot_book(); // sequence = 1
        let err = book.apply_update(&update(5, vec![], vec![(102.0, 1.0)])).unwrap_err();
        assert!(matches!(err, BookError::SequenceGap { .. }));
    }

    #[test]
    fn apply_update_sequence_zero_skips_validation() {
        let mut book = snapshot_book(); // sequence = 1
        // Sequence 0 should bypass validation (snapshot feed)
        book.apply_update(&update(0, vec![(100.0, 99.0)], vec![])).unwrap();
    }

    #[test]
    fn apply_update_crossed_book_rejected() {
        let mut book = OrderBook::new("X");
        let err = book.apply_update(&BookUpdate {
            symbol: "X".to_string(),
            sequence: 0,
            bids: vec![(105.0, 1.0)],
            asks: vec![(100.0, 1.0)], // ask < bid → crossed
        }).unwrap_err();
        assert!(matches!(err, BookError::CrossedBook { .. }));
    }

    #[test]
    fn apply_update_sequential_ok() {
        let mut book = snapshot_book(); // sequence = 1
        book.apply_update(&update(2, vec![], vec![(102.0, 99.0)])).unwrap();
        assert_eq!(book.sequence, 2);
    }

    #[test]
    fn reset_clears_book() {
        let mut book = snapshot_book();
        book.reset();
        assert!(book.best_bid().is_none());
        assert!(book.best_ask().is_none());
        assert_eq!(book.sequence, 0);
    }

    // ── OrderBookManager ──────────────────────────────────────────────────

    #[test]
    fn manager_creates_book_on_first_apply() {
        let mgr = OrderBookManager::new();
        mgr.apply(&BookUpdate {
            symbol: "ETH".to_string(),
            sequence: 0,
            bids: vec![(2000.0, 5.0)],
            asks: vec![(2001.0, 3.0)],
        }).unwrap();
        let book = mgr.get("ETH").expect("book present");
        assert!(book.best_bid().is_some());
    }

    #[test]
    fn manager_get_unknown_returns_none() {
        let mgr = OrderBookManager::new();
        assert!(mgr.get("UNKNOWN").is_none());
    }

    #[test]
    fn manager_tracks_multiple_symbols() {
        let mgr = OrderBookManager::new();
        for sym in &["AAPL", "MSFT", "GOOG"] {
            mgr.apply(&BookUpdate {
                symbol: sym.to_string(),
                sequence: 0,
                bids: vec![(100.0, 1.0)],
                asks: vec![(101.0, 1.0)],
            }).unwrap();
        }
        assert_eq!(mgr.symbol_count(), 3);
    }

    #[test]
    fn manager_propagates_book_error() {
        let mgr = OrderBookManager::new();
        mgr.apply(&BookUpdate {
            symbol: "X".to_string(),
            sequence: 0,
            bids: vec![(100.0, 5.0)],
            asks: vec![(101.0, 3.0)],
        }).unwrap();
        // Force a crossed book
        let err = mgr.apply(&BookUpdate {
            symbol: "X".to_string(),
            sequence: 0,
            bids: vec![(200.0, 1.0)],
            asks: vec![],
        });
        assert!(err.is_err());
    }

    #[test]
    fn mid_price_tracks_updates() {
        let mut book = OrderBook::new("X");
        book.apply_update(&BookUpdate {
            symbol: "X".to_string(),
            sequence: 0,
            bids: vec![(98.0, 5.0)],
            asks: vec![(102.0, 5.0)],
        }).unwrap();
        let mid = book.mid_price().expect("mid");
        assert!((mid - 100.0).abs() < 1e-9);
    }

    #[test]
    fn imbalance_balanced_is_zero() {
        let mut book = OrderBook::new("X");
        book.bids.insert(OrdF64(100.0), 50.0);
        book.asks.insert(OrdF64(101.0), 50.0);
        let imb = book.imbalance();
        assert!((imb).abs() < 1e-9);
    }

    #[test]
    fn depth_bid_descending_order() {
        let book = snapshot_book();
        let (bids, _) = book.depth(3);
        assert_eq!(bids.len(), 3);
        assert!(bids[0].price > bids[1].price);
        assert!(bids[1].price > bids[2].price);
    }

    #[test]
    fn depth_ask_ascending_order() {
        let book = snapshot_book();
        let (_, asks) = book.depth(3);
        assert_eq!(asks.len(), 3);
        assert!(asks[0].price < asks[1].price);
        assert!(asks[1].price < asks[2].price);
    }

    #[test]
    fn apply_update_increments_sequence() {
        let mut book = snapshot_book(); // sequence = 1
        book.apply_update(&update(2, vec![], vec![(102.0, 7.0)])).unwrap();
        assert_eq!(book.sequence, 2);
    }
}
