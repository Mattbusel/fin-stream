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

    /// Notional value of this level: `price × quantity`.
    pub fn notional(&self) -> Decimal {
        self.price * self.quantity
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

    /// Returns `true` if this delta adds or updates a price level (`quantity > 0`).
    ///
    /// The logical complement of [`is_delete`](Self::is_delete).
    pub fn is_add(&self) -> bool {
        !self.is_delete()
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

    /// Resting quantity at the best bid level.
    ///
    /// Shorthand for `self.best_bid().map(|l| l.quantity)`.
    pub fn best_bid_qty(&self) -> Option<Decimal> {
        self.best_bid().map(|l| l.quantity)
    }

    /// Resting quantity at the best ask level.
    ///
    /// Shorthand for `self.best_ask().map(|l| l.quantity)`.
    pub fn best_ask_qty(&self) -> Option<Decimal> {
        self.best_ask().map(|l| l.quantity)
    }

    /// Mid price.
    pub fn mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / Decimal::from(2))
    }

    /// Quantity-weighted mid price.
    ///
    /// `(bid_price × ask_qty + ask_price × bid_qty) / (bid_qty + ask_qty)`.
    ///
    /// Gives a better estimate of fair value than the arithmetic mid when the
    /// best-bid and best-ask have very different resting quantities.
    /// Returns `None` if either side is empty or total quantity is zero.
    pub fn weighted_mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        let total_qty = bid.quantity + ask.quantity;
        if total_qty.is_zero() {
            return None;
        }
        Some((bid.price * ask.quantity + ask.price * bid.quantity) / total_qty)
    }

    /// Returns `(best_bid, best_ask)` in a single call, or `None` if either side is absent.
    pub fn top_of_book(&self) -> Option<(PriceLevel, PriceLevel)> {
        Some((self.best_bid()?, self.best_ask()?))
    }

    /// Full displayed price range: `best_ask - worst_displayed_bid`.
    ///
    /// Wider than `spread()` which only uses the top-of-book.
    /// Returns `None` if either side is empty.
    pub fn price_range(&self) -> Option<Decimal> {
        let worst_bid = *self.bids.iter().next()?.0; // lowest bid price
        let best_ask = *self.asks.iter().next()?.0;  // lowest ask price
        Some(best_ask - worst_bid)
    }

    /// Spread.
    pub fn spread(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some(ask - bid)
    }

    /// Returns `true` if both sides of the book have no resting orders.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Remove all price levels from both sides of the book.
    ///
    /// Also clears the last seen sequence number. Useful when reconnecting to
    /// an exchange feed and waiting for a fresh snapshot before applying deltas.
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_sequence = None;
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

    /// Total notional value (`Σ price × quantity`) across all levels on the
    /// given side.
    ///
    /// Useful for comparing the dollar value committed to each side of the
    /// book, rather than just the raw quantity.
    pub fn total_notional(&self, side: BookSide) -> Decimal {
        match side {
            BookSide::Bid => self.bids.iter().map(|(p, q)| *p * *q).sum(),
            BookSide::Ask => self.asks.iter().map(|(p, q)| *p * *q).sum(),
        }
    }

    /// Returns `true` if exactly one side (bids or asks) has levels and the
    /// other is empty. An empty book returns `false`.
    pub fn is_one_sided(&self) -> bool {
        let has_bids = !self.bids.is_empty();
        let has_asks = !self.asks.is_empty();
        has_bids != has_asks
    }

    /// Bid-ask spread expressed in basis points: `(ask − bid) / mid × 10 000`.
    ///
    /// Returns `None` if either side is empty or the mid price is zero.
    /// Imbalance between number of bid and ask price levels.
    ///
    /// Returns `(bid_levels - ask_levels) / (bid_levels + ask_levels)` as f64
    /// in the range `[-1.0, 1.0]`. Returns `None` when the book is empty.
    pub fn level_count_imbalance(&self) -> Option<f64> {
        let total = self.bids.len() + self.asks.len();
        if total == 0 {
            return None;
        }
        let diff = self.bids.len() as f64 - self.asks.len() as f64;
        Some(diff / total as f64)
    }

    /// Bid-ask spread in basis points: `(ask − bid) / mid × 10_000`.
    ///
    /// Returns `None` if either side of the book is empty or the mid-price is zero.
    pub fn bid_ask_spread_bps(&self) -> Option<f64> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        let mid = (bid + ask) / Decimal::from(2);
        if mid.is_zero() {
            return None;
        }
        let spread = ask - bid;
        let spread_f: f64 = spread.to_string().parse().ok()?;
        let mid_f: f64 = mid.to_string().parse().ok()?;
        Some(spread_f / mid_f * 10_000.0)
    }

    /// Sum of the top `n` bid levels' quantities (best `n` bids).
    ///
    /// Total quantity across all bid levels.
    pub fn total_bid_volume(&self) -> Decimal {
        self.bids.values().copied().sum()
    }

    /// Total quantity across all ask levels.
    pub fn total_ask_volume(&self) -> Decimal {
        self.asks.values().copied().sum()
    }

    /// Sum of the top `n` bid levels' quantities (best `n` bids).
    ///
    /// If fewer than `n` bid levels exist, sums all available levels. Returns
    /// `Decimal::ZERO` when the bid side is empty.
    pub fn cumulative_bid_volume(&self, n: usize) -> Decimal {
        self.bids.values().rev().take(n).copied().sum()
    }

    /// Sum of the top `n` ask levels' quantities (best `n` asks).
    ///
    /// If fewer than `n` ask levels exist, sums all available levels. Returns
    /// `Decimal::ZERO` when the ask side is empty.
    pub fn cumulative_ask_volume(&self, n: usize) -> Decimal {
        self.asks.values().take(n).copied().sum()
    }

    /// Best `n` bid levels as [`PriceLevel`]s, sorted best-first (highest price first).
    ///
    /// If fewer than `n` levels exist, all are returned.
    pub fn top_n_bids(&self, n: usize) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// Best `n` ask levels as [`PriceLevel`]s, sorted best-first (lowest price first).
    ///
    /// If fewer than `n` levels exist, all are returned.
    pub fn top_n_asks(&self, n: usize) -> Vec<PriceLevel> {
        self.asks
            .iter()
            .take(n)
            .map(|(p, q)| PriceLevel::new(*p, *q))
            .collect()
    }

    /// Ratio of cumulative bid volume to cumulative ask volume across top `n` levels.
    ///
    /// Returns `None` when the ask side is empty (avoids division by zero).
    /// Values > 1.0 indicate buy-side pressure; values < 1.0 indicate sell-side pressure.
    pub fn depth_ratio(&self, n: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ask_vol = self.cumulative_ask_volume(n);
        if ask_vol.is_zero() {
            return None;
        }
        (self.cumulative_bid_volume(n) / ask_vol).to_f64()
    }

    /// The cheapest ask level with quantity ≥ `min_qty`, or `None` if no such level exists.
    ///
    /// Scans ask levels from the tightest (best) price outward. Useful for
    /// detecting large sell walls sitting near the top of the book.
    pub fn ask_wall(&self, min_qty: Decimal) -> Option<PriceLevel> {
        self.asks
            .iter()
            .find(|(_, qty)| **qty >= min_qty)
            .map(|(price, qty)| PriceLevel::new(*price, *qty))
    }

    /// The highest bid level with quantity ≥ `min_qty`, or `None` if no such level exists.
    ///
    /// Scans bid levels from the best (highest) price downward. Useful for
    /// detecting large buy walls sitting near the top of the book.
    pub fn bid_wall(&self, min_qty: Decimal) -> Option<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .find(|(_, qty)| **qty >= min_qty)
            .map(|(price, qty)| PriceLevel::new(*price, *qty))
    }

    /// Number of bid levels with price strictly above `price`.
    ///
    /// Useful for measuring how much resting bid interest sits above a given
    /// reference price (e.g. the last trade price).
    pub fn bid_levels_above(&self, price: Decimal) -> usize {
        self.bids.range((std::ops::Bound::Excluded(&price), std::ops::Bound::Unbounded)).count()
    }

    /// Number of ask levels with price strictly below `price`.
    ///
    /// Useful for measuring how much resting ask interest sits below a given
    /// reference price (e.g. the last trade price).
    pub fn ask_levels_below(&self, price: Decimal) -> usize {
        self.asks.range(..price).count()
    }

    /// Ratio of total bid volume to total ask volume.
    ///
    /// Returns `None` when either side has zero volume (avoids division by
    /// zero and meaningless ratios on empty books).  A value > 1.0 means more
    /// buying interest; < 1.0 means more selling pressure.
    pub fn bid_ask_volume_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid = self.total_bid_volume();
        let ask = self.total_ask_volume();
        if bid.is_zero() || ask.is_zero() {
            return None;
        }
        let bid_f = bid.to_f64()?;
        let ask_f = ask.to_f64()?;
        Some(bid_f / ask_f)
    }

    /// Total volume across the top `n` bid price levels (best-to-worst order).
    ///
    /// If there are fewer than `n` levels, the volume of all existing levels is
    /// returned. Returns `Decimal::ZERO` for an empty bid side.
    pub fn top_n_bid_volume(&self, n: usize) -> Decimal {
        self.bids.iter().rev().take(n).map(|(_, qty)| *qty).sum()
    }

    /// Normalised order-book imbalance: `(bid_vol − ask_vol) / (bid_vol + ask_vol)`.
    ///
    /// Returns a value in `[-1.0, 1.0]`.  `+1.0` means all volume is on the
    /// bid side (strong buying pressure); `-1.0` means all volume is on the
    /// ask side (strong selling pressure).  Returns `None` when both sides are
    /// empty (sum is zero).
    pub fn imbalance_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid = self.total_bid_volume();
        let ask = self.total_ask_volume();
        let total = bid + ask;
        if total.is_zero() {
            return None;
        }
        let bid_f = bid.to_f64()?;
        let ask_f = ask.to_f64()?;
        let total_f = bid_f + ask_f;
        Some((bid_f - ask_f) / total_f)
    }

    /// Total volume across the top `n` ask price levels (best-to-worst order,
    /// i.e. lowest asks first).
    ///
    /// If there are fewer than `n` levels, the volume of all existing levels is
    /// returned. Returns `Decimal::ZERO` for an empty ask side.
    pub fn top_n_ask_volume(&self, n: usize) -> Decimal {
        self.asks.iter().take(n).map(|(_, qty)| *qty).sum()
    }

    /// Returns `true` if there is a non-zero ask entry at exactly `price`.
    pub fn has_ask_at(&self, price: Decimal) -> bool {
        self.asks.get(&price).map(|q| !q.is_zero()).unwrap_or(false)
    }

    /// Returns `(bid_levels, ask_levels)` — the number of distinct price levels
    /// on each side of the book.
    pub fn bid_ask_depth(&self) -> (usize, usize) {
        (self.bids.len(), self.asks.len())
    }

    /// Total volume across all bid and ask levels combined.
    pub fn total_book_volume(&self) -> Decimal {
        self.total_bid_volume() + self.total_ask_volume()
    }

    /// Price distance from the best bid to the worst (lowest) bid.
    ///
    /// Returns `None` if there are fewer than 2 bid levels.
    pub fn price_range_bids(&self) -> Option<Decimal> {
        if self.bids.len() < 2 {
            return None;
        }
        let best = *self.bids.keys().next_back()?;
        let worst = *self.bids.keys().next()?;
        Some(best - worst)
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

    /// Returns `true` if the bid-ask spread is at or below `threshold`.
    ///
    /// Returns `false` when either side is empty (no spread to compare).
    pub fn is_tight_spread(&self, threshold: Decimal) -> bool {
        match self.spread() {
            Some(s) => s <= threshold,
            None => false,
        }
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

    /// Best-bid quantity as a fraction of `(best_bid_qty + best_ask_qty)`.
    ///
    /// Values near `1.0` indicate the best bid has dominant size; near `0.0` the best ask
    /// dominates. Returns `None` when either side is empty or both quantities are zero.
    pub fn quote_imbalance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid_qty = *self.bids.iter().next_back()?.1;
        let ask_qty = *self.asks.iter().next()?.1;
        let total = bid_qty + ask_qty;
        if total.is_zero() {
            return None;
        }
        (bid_qty / total).to_f64()
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

    /// Number of distinct price levels per unit of price range on the given side.
    ///
    /// `quote_density = level_count / (max_price - min_price)`.
    /// Returns `None` if the side has fewer than 2 levels (range is zero).
    pub fn quote_density(&self, side: BookSide) -> Option<Decimal> {
        let map = match side {
            BookSide::Bid => &self.bids,
            BookSide::Ask => &self.asks,
        };
        if map.len() < 2 { return None; }
        let min_p = *map.keys().next()?;
        let max_p = *map.keys().next_back()?;
        let range = max_p - min_p;
        if range.is_zero() { return None; }
        Some(Decimal::from(map.len()) / range)
    }

    /// Ratio of total bid quantity to total ask quantity.
    ///
    /// Values > 1 indicate heavier buy-side resting volume; < 1 more sell-side.
    /// Returns `None` if the ask side has zero volume.
    pub fn bid_ask_qty_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ask_vol = self.ask_volume_total();
        if ask_vol.is_zero() { return None; }
        (self.bid_volume_total() / ask_vol).to_f64()
    }

    /// Ratio of ask levels to bid levels: `ask_count / bid_count`.
    ///
    /// Values > 1 indicate more ask granularity; < 1 more bid granularity.
    /// Returns `None` if the bid side is empty.
    pub fn ask_bid_level_ratio(&self) -> Option<f64> {
        if self.bids.is_empty() { return None; }
        Some(self.asks.len() as f64 / self.bids.len() as f64)
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

    /// Returns the top-`n` price levels for the given side, sorted best-first.
    ///
    /// For bids, levels are sorted descending (highest price first).
    /// For asks, levels are sorted ascending (lowest price first).
    /// If `n` exceeds the available levels, all levels are returned.
    pub fn levels(&self, side: BookSide, n: usize) -> Vec<PriceLevel> {
        match side {
            BookSide::Bid => self
                .bids
                .iter()
                .rev()
                .take(n)
                .map(|(p, q)| PriceLevel::new(*p, *q))
                .collect(),
            BookSide::Ask => self
                .asks
                .iter()
                .take(n)
                .map(|(p, q)| PriceLevel::new(*p, *q))
                .collect(),
        }
    }

    /// Returns the resting quantity at a specific bid price level, or `None` if absent.
    pub fn bid_volume_at_price(&self, price: Decimal) -> Option<Decimal> {
        self.bids.get(&price).copied()
    }

    /// Returns the resting quantity at a specific ask price level, or `None` if absent.
    pub fn ask_volume_at_price(&self, price: Decimal) -> Option<Decimal> {
        self.asks.get(&price).copied()
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

    /// Returns the best bid price, or `None` if the bid side is empty.
    pub fn best_bid_price(&self) -> Option<Decimal> {
        self.best_bid().map(|l| l.price)
    }

    /// Returns the best ask price, or `None` if the ask side is empty.
    pub fn best_ask_price(&self) -> Option<Decimal> {
        self.best_ask().map(|l| l.price)
    }

    /// Returns `true` if the book is crossed: best bid ≥ best ask.
    ///
    /// A crossed book indicates an invalid state (stale snapshot or missed
    /// delta). Under normal operation this should always be `false`.
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => bid.price >= ask.price,
            _ => false,
        }
    }

    /// Returns `true` if there is at least one bid level in the book.
    pub fn has_bids(&self) -> bool {
        !self.bids.is_empty()
    }

    /// Returns `true` if there is at least one ask level in the book.
    pub fn has_asks(&self) -> bool {
        !self.asks.is_empty()
    }

    /// Price distance from best ask to worst ask (highest ask price - lowest ask price).
    ///
    /// Returns `None` if the ask side is empty.
    pub fn ask_price_range(&self) -> Option<Decimal> {
        if self.asks.is_empty() {
            return None;
        }
        let best = *self.asks.keys().next()?;
        let worst = *self.asks.keys().next_back()?;
        Some(worst - best)
    }

    /// Price distance from best bid to worst bid (highest bid price - lowest bid price).
    ///
    /// Returns `None` if the bid side is empty.
    pub fn bid_price_range(&self) -> Option<Decimal> {
        if self.bids.is_empty() {
            return None;
        }
        let best = *self.bids.keys().next_back()?;
        let worst = *self.bids.keys().next()?;
        Some(best - worst)
    }

    /// Spread as a fraction of the mid price: `spread / mid_price`.
    ///
    /// Returns `None` if the book has no bid or ask, or mid price is zero.
    pub fn mid_spread_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let spread = self.spread()?;
        let mid = self.mid_price()?;
        if mid.is_zero() {
            return None;
        }
        (spread / mid).to_f64()
    }

    /// Bid-ask volume imbalance: `(bid_vol - ask_vol) / (bid_vol + ask_vol)`.
    ///
    /// Returns a value in `[-1.0, 1.0]`. Positive = more bid volume; negative = more ask volume.
    /// Returns `None` if both sides are empty.
    pub fn volume_imbalance(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bid = self.total_bid_volume();
        let ask = self.total_ask_volume();
        let total = bid + ask;
        if total.is_zero() {
            return None;
        }
        ((bid - ask) / total).to_f64()
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

    /// Spread expressed in basis points relative to mid-price: `(ask - bid) / mid × 10_000`.
    ///
    /// Returns `None` when either side is empty or mid-price is zero.
    pub fn spread_bps(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mid = self.mid_price()?;
        if mid.is_zero() {
            return None;
        }
        let spread = self.spread()?;
        (spread / mid * Decimal::from(10_000u32)).to_f64()
    }

    /// Cumulative volume within `pct` percent of the best price on a given side.
    ///
    /// For bids: sums quantity at all levels where `price >= best_bid * (1 - pct/100)`.
    /// For asks: sums quantity at all levels where `price <= best_ask * (1 + pct/100)`.
    ///
    /// Returns `None` if the side is empty or `pct` is negative.
    pub fn depth_at_pct(&self, side: BookSide, pct: f64) -> Option<Decimal> {
        use rust_decimal::prelude::FromPrimitive;
        if pct < 0.0 { return None; }
        let pct_dec = Decimal::from_f64(pct / 100.0)?;
        match side {
            BookSide::Bid => {
                let best = *self.bids.keys().next_back()?;
                let threshold = best * (Decimal::ONE - pct_dec);
                Some(self.bids.range(threshold..).map(|(_, q)| q).sum())
            }
            BookSide::Ask => {
                let best = *self.asks.keys().next()?;
                let threshold = best * (Decimal::ONE + pct_dec);
                Some(self.asks.range(..=threshold).map(|(_, q)| q).sum())
            }
        }
    }

    /// Microprice: volume-weighted mid using top-of-book quantities.
    ///
    /// `microprice = (ask_qty * best_bid + bid_qty * best_ask) / (bid_qty + ask_qty)`
    ///
    /// More accurate than simple mid when the order book is imbalanced.
    /// Returns `None` if either side is empty or total quantity is zero.
    pub fn microprice(&self) -> Option<Decimal> {
        let best_bid = *self.bids.keys().next_back()?;
        let best_ask = *self.asks.keys().next()?;
        let bid_qty = *self.bids.get(&best_bid)?;
        let ask_qty = *self.asks.get(&best_ask)?;
        let total_qty = bid_qty + ask_qty;
        if total_qty.is_zero() { return None; }
        Some((ask_qty * best_bid + bid_qty * best_ask) / total_qty)
    }

    /// Returns the top `n` price levels on a given side as `(price, quantity)` pairs.
    ///
    /// Bid levels are returned in descending price order (best bid first).
    /// Ask levels are returned in ascending price order (best ask first).
    /// Returns fewer than `n` entries if the side has fewer levels.
    pub fn best_n_levels(&self, side: BookSide, n: usize) -> Vec<(Decimal, Decimal)> {
        match side {
            BookSide::Bid => self.bids.iter().rev().take(n)
                .map(|(&p, &q)| (p, q)).collect(),
            BookSide::Ask => self.asks.iter().take(n)
                .map(|(&p, &q)| (p, q)).collect(),
        }
    }

    /// Estimated price impact of a market order of `qty` on the given side.
    ///
    /// Walks the book, consuming levels until `qty` is filled. Returns the
    /// weighted average fill price minus the best price (positive = adverse).
    /// Returns `None` if the side is empty or `qty` is zero/negative.
    pub fn price_impact(&self, side: BookSide, qty: Decimal) -> Option<Decimal> {
        if qty <= Decimal::ZERO { return None; }
        let best_price = match side {
            BookSide::Bid => self.bids.keys().next_back().copied()?,
            BookSide::Ask => self.asks.keys().next().copied()?,
        };
        let mut remaining = qty;
        let mut cost = Decimal::ZERO;
        let levels: Box<dyn Iterator<Item = (&Decimal, &Decimal)>> = match side {
            BookSide::Bid => Box::new(self.bids.iter().rev()),
            BookSide::Ask => Box::new(self.asks.iter()),
        };
        for (&price, &level_qty) in levels {
            if remaining <= Decimal::ZERO { break; }
            let filled = remaining.min(level_qty);
            cost += price * filled;
            remaining -= filled;
        }
        if remaining > Decimal::ZERO { return None; } // not enough liquidity
        let avg_fill = cost / qty;
        Some((avg_fill - best_price).abs())
    }

    /// Total notional value (price × quantity) at a specific price level on a given side.
    ///
    /// Returns `None` if no level exists at `price`.
    pub fn total_value_at_level(&self, side: BookSide, price: Decimal) -> Option<Decimal> {
        match side {
            BookSide::Bid => self.bids.get(&price).map(|&q| price * q),
            BookSide::Ask => self.asks.get(&price).map(|&q| price * q),
        }
    }

    /// Estimated volume-weighted average execution price for a market buy of `quantity`.
    ///
    /// Walks up the ask side. Returns `None` if insufficient liquidity.
    pub fn price_impact_buy(&self, quantity: Decimal) -> Option<Decimal> {
        if quantity <= Decimal::ZERO {
            return None;
        }
        let mut remaining = quantity;
        let mut cost = Decimal::ZERO;
        for (&price, &qty) in &self.asks {
            if remaining.is_zero() { break; }
            let fill = remaining.min(qty);
            cost += fill * price;
            remaining -= fill;
        }
        if !remaining.is_zero() { return None; }
        Some(cost / quantity)
    }

    /// Estimated volume-weighted average execution price for a market sell of `quantity`.
    ///
    /// Walks down the bid side. Returns `None` if insufficient liquidity.
    pub fn price_impact_sell(&self, quantity: Decimal) -> Option<Decimal> {
        if quantity <= Decimal::ZERO {
            return None;
        }
        let mut remaining = quantity;
        let mut proceeds = Decimal::ZERO;
        for (&price, &qty) in self.bids.iter().rev() {
            if remaining.is_zero() { break; }
            let fill = remaining.min(qty);
            proceeds += fill * price;
            remaining -= fill;
        }
        if !remaining.is_zero() { return None; }
        Some(proceeds / quantity)
    }

    /// Number of distinct price levels on the ask side.
    pub fn ask_level_count(&self) -> usize {
        self.asks.len()
    }

    /// Number of distinct price levels on the bid side.
    pub fn bid_level_count(&self) -> usize {
        self.bids.len()
    }

    /// Cumulative ask volume at levels within `price_range` of the best ask.
    ///
    /// Sums all ask quantities where `price <= best_ask + price_range`.
    /// Returns `Decimal::ZERO` if the ask side is empty.
    pub fn ask_volume_within(&self, price_range: Decimal) -> Decimal {
        match self.best_ask() {
            None => Decimal::ZERO,
            Some(best) => {
                let ceiling = best.price + price_range;
                self.asks.range(..=ceiling).map(|(_, &q)| q).sum()
            }
        }
    }

    /// Cumulative bid volume at levels within `price_range` of the best bid.
    ///
    /// Sums all bid quantities where `price >= best_bid - price_range`.
    /// Returns `Decimal::ZERO` if the bid side is empty.
    pub fn bid_volume_within(&self, price_range: Decimal) -> Decimal {
        match self.best_bid() {
            None => Decimal::ZERO,
            Some(best) => {
                let floor = best.price - price_range;
                self.bids.range(floor..).map(|(_, &q)| q).sum()
            }
        }
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

    // ── PriceLevel::notional ─────────────────────────────────────────────────

    #[test]
    fn test_price_level_notional() {
        let level = PriceLevel::new(dec!(50000), dec!(2));
        assert_eq!(level.notional(), dec!(100000));
    }

    #[test]
    fn test_price_level_notional_zero_qty() {
        let level = PriceLevel::new(dec!(100), dec!(0));
        assert_eq!(level.notional(), dec!(0));
    }

    // ── OrderBook::weighted_mid_price ────────────────────────────────────────

    #[test]
    fn test_weighted_mid_price_equal_qtys_is_arithmetic_mid() {
        // bid=100 qty=1, ask=102 qty=1 → wmid = (100*1 + 102*1) / 2 = 101
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(102), dec!(1))).unwrap();
        assert_eq!(b.weighted_mid_price().unwrap(), dec!(101));
    }

    #[test]
    fn test_weighted_mid_price_skews_toward_larger_qty() {
        // bid=100 qty=1, ask=102 qty=3 → wmid = (100*3 + 102*1) / 4 = 402/4 = 100.5
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(102), dec!(3))).unwrap();
        assert_eq!(b.weighted_mid_price().unwrap(), dec!(100.5));
    }

    #[test]
    fn test_weighted_mid_price_empty_returns_none() {
        let b = book("X");
        assert!(b.weighted_mid_price().is_none());
    }

    // ── OrderBook::is_empty ───────────────────────────────────────────────────

    #[test]
    fn test_is_empty_new_book() {
        let b = book("BTC-USD");
        assert!(b.is_empty());
    }

    #[test]
    fn test_is_empty_false_with_bid() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        assert!(!b.is_empty());
    }

    #[test]
    fn test_is_empty_false_with_ask() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(!b.is_empty());
    }

    #[test]
    fn test_is_empty_true_after_removing_all_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(0))).unwrap(); // remove
        assert!(b.is_empty());
    }

    // ── OrderBook::clear ──────────────────────────────────────────────────────

    #[test]
    fn test_clear_empty_book_is_noop() {
        let mut b = book("BTC-USD");
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn test_clear_removes_all_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn test_clear_allows_fresh_apply_after() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.clear();
        // After clear, a new bid should work without sequence issues
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(5))).unwrap();
        assert_eq!(b.bid_depth(), 1);
    }

    // ── OrderBook::total_notional ─────────────────────────────────────────────

    #[test]
    fn test_total_notional_bid_side() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(50000), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(3))).unwrap();
        // 50000*2 + 49900*3 = 100000 + 149700 = 249700
        assert_eq!(b.total_notional(BookSide::Bid), dec!(249700));
    }

    #[test]
    fn test_total_notional_ask_side() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(2))).unwrap();
        // 50100*1 + 50200*2 = 50100 + 100400 = 150500
        assert_eq!(b.total_notional(BookSide::Ask), dec!(150500));
    }

    #[test]
    fn test_total_notional_empty_side_is_zero() {
        let b = book("BTC-USD");
        assert_eq!(b.total_notional(BookSide::Bid), dec!(0));
        assert_eq!(b.total_notional(BookSide::Ask), dec!(0));
    }

    #[test]
    fn test_cumulative_bid_volume_top_two() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(5))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(98), dec!(2))).unwrap();
        // best 2 bids: 100 (qty=5), 99 (qty=3)
        assert_eq!(b.cumulative_bid_volume(2), dec!(8));
    }

    #[test]
    fn test_cumulative_ask_volume_top_two() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(4))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(6))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(103), dec!(1))).unwrap();
        // best 2 asks: 101 (qty=4), 102 (qty=6)
        assert_eq!(b.cumulative_ask_volume(2), dec!(10));
    }

    #[test]
    fn test_cumulative_volume_empty_returns_zero() {
        let b = book("BTC-USD");
        assert_eq!(b.cumulative_bid_volume(5), dec!(0));
        assert_eq!(b.cumulative_ask_volume(5), dec!(0));
    }

    #[test]
    fn test_top_n_bids_best_first() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(98), dec!(3))).unwrap();
        let top2 = b.top_n_bids(2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].price, dec!(100)); // best bid first
        assert_eq!(top2[1].price, dec!(99));
    }

    #[test]
    fn test_top_n_asks_best_first() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(103), dec!(3))).unwrap();
        let top2 = b.top_n_asks(2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].price, dec!(101)); // best ask first
        assert_eq!(top2[1].price, dec!(102));
    }

    #[test]
    fn test_depth_ratio_balanced_book() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(5))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(5))).unwrap();
        let ratio = b.depth_ratio(1).unwrap();
        assert!((ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_depth_ratio_empty_asks_returns_none() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(5))).unwrap();
        assert!(b.depth_ratio(1).is_none());
    }

    // ── OrderBook::is_one_sided ───────────────────────────────────────────────

    #[test]
    fn test_is_one_sided_bids_only() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        assert!(b.is_one_sided());
    }

    #[test]
    fn test_is_one_sided_asks_only() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(b.is_one_sided());
    }

    #[test]
    fn test_is_one_sided_false_with_both_sides() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(!b.is_one_sided());
    }

    #[test]
    fn test_is_one_sided_false_for_empty_book() {
        let b = book("BTC-USD");
        assert!(!b.is_one_sided());
    }

    // ── OrderBook::bid_ask_spread_bps ─────────────────────────────────────────

    #[test]
    fn test_bid_ask_spread_bps_known_value() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // spread=1, mid=100.5, bps = 1/100.5*10000 ≈ 99.5
        let bps = b.bid_ask_spread_bps().unwrap();
        assert!((bps - 1.0 / 100.5 * 10_000.0).abs() < 0.01);
    }

    #[test]
    fn test_bid_ask_spread_bps_none_when_one_sided() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        assert!(b.bid_ask_spread_bps().is_none());
    }

    #[test]
    fn test_bid_ask_spread_bps_none_for_empty_book() {
        let b = book("BTC-USD");
        assert!(b.bid_ask_spread_bps().is_none());
    }

    // --- ask_wall / bid_wall ---

    #[test]
    fn test_ask_wall_returns_cheapest_ask_above_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50200), dec!(10))).unwrap();
        // ask_wall(5) should find 50200 (qty=10 >= 5); 50100 has qty=2 < 5
        let wall = b.ask_wall(dec!(5)).unwrap();
        assert_eq!(wall.price, dec!(50200));
        assert_eq!(wall.quantity, dec!(10));
    }

    #[test]
    fn test_ask_wall_none_when_no_level_meets_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(50100), dec!(1))).unwrap();
        assert!(b.ask_wall(dec!(5)).is_none());
    }

    #[test]
    fn test_bid_wall_returns_highest_bid_above_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(10))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49800), dec!(2))).unwrap();
        // bid_wall(5) scans from best (49900) → qty=10 >= 5, so returns 49900
        let wall = b.bid_wall(dec!(5)).unwrap();
        assert_eq!(wall.price, dec!(49900));
        assert_eq!(wall.quantity, dec!(10));
    }

    #[test]
    fn test_bid_wall_none_when_no_level_meets_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(49900), dec!(1))).unwrap();
        assert!(b.bid_wall(dec!(5)).is_none());
    }

    // ── OrderBook::level_count_imbalance ──────────────────────────────────────

    #[test]
    fn test_level_count_imbalance_balanced_sides() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // 1 bid, 1 ask → (1-1)/(1+1) = 0.0
        assert_eq!(b.level_count_imbalance(), Some(0.0));
    }

    #[test]
    fn test_level_count_imbalance_bids_only() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(98), dec!(1))).unwrap();
        // 2 bids, 0 asks → (2-0)/(2+0) = 1.0
        assert_eq!(b.level_count_imbalance(), Some(1.0));
    }

    #[test]
    fn test_level_count_imbalance_none_for_empty_book() {
        let b = book("BTC-USD");
        assert!(b.level_count_imbalance().is_none());
    }

    // ── OrderBook::total_bid_volume / total_ask_volume ────────────────────────

    #[test]
    fn test_total_bid_volume_sums_all_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(98), dec!(2))).unwrap();
        assert_eq!(b.total_bid_volume(), dec!(5));
    }

    #[test]
    fn test_total_ask_volume_sums_all_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(4))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(1))).unwrap();
        assert_eq!(b.total_ask_volume(), dec!(5));
    }

    #[test]
    fn test_total_bid_volume_zero_for_empty_side() {
        let b = book("BTC-USD");
        assert_eq!(b.total_bid_volume(), dec!(0));
    }

    // --- bid_levels_above / ask_levels_below ---

    #[test]
    fn test_bid_levels_above_counts_strictly_above() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(101), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(102), dec!(1))).unwrap();
        // levels above 100: 101 and 102 → 2
        assert_eq!(b.bid_levels_above(dec!(100)), 2);
    }

    #[test]
    fn test_bid_levels_above_zero_when_none_above() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        assert_eq!(b.bid_levels_above(dec!(100)), 0);
    }

    #[test]
    fn test_ask_levels_below_counts_strictly_below() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(1))).unwrap();
        // levels below 102: 100 and 101 → 2
        assert_eq!(b.ask_levels_below(dec!(102)), 2);
    }

    #[test]
    fn test_ask_levels_below_zero_for_empty_book() {
        let b = book("BTC-USD");
        assert_eq!(b.ask_levels_below(dec!(100)), 0);
    }

    // --- bid_ask_volume_ratio / top_n_bid_volume ---

    #[test]
    fn test_bid_ask_volume_ratio_returns_correct_ratio() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // 3 / 1 = 3.0
        let ratio = b.bid_ask_volume_ratio().unwrap();
        assert!((ratio - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_bid_ask_volume_ratio_none_when_ask_empty() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        assert!(b.bid_ask_volume_ratio().is_none());
    }

    #[test]
    fn test_bid_ask_volume_ratio_none_when_bid_empty() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(b.bid_ask_volume_ratio().is_none());
    }

    #[test]
    fn test_top_n_bid_volume_sums_top_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap(); // worst
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(101), dec!(2))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(102), dec!(3))).unwrap(); // best
        // top 2: 102 (3) + 101 (2) = 5
        assert_eq!(b.top_n_bid_volume(2), dec!(5));
    }

    #[test]
    fn test_top_n_bid_volume_all_when_n_exceeds_levels() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(101), dec!(2))).unwrap();
        // n=5 but only 2 levels → total = 3
        assert_eq!(b.top_n_bid_volume(5), dec!(3));
    }

    #[test]
    fn test_top_n_bid_volume_zero_for_empty_book() {
        let b = book("BTC-USD");
        assert_eq!(b.top_n_bid_volume(3), dec!(0));
    }

    // --- imbalance_ratio / top_n_ask_volume ---

    #[test]
    fn test_imbalance_ratio_positive_when_more_bids() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // (3 - 1) / (3 + 1) = 0.5
        let ratio = b.imbalance_ratio().unwrap();
        assert!((ratio - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_imbalance_ratio_negative_when_more_asks() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(3))).unwrap();
        // (1 - 3) / (1 + 3) = -0.5
        let ratio = b.imbalance_ratio().unwrap();
        assert!((ratio - (-0.5)).abs() < 1e-10);
    }

    #[test]
    fn test_imbalance_ratio_none_when_both_empty() {
        let b = book("BTC-USD");
        assert!(b.imbalance_ratio().is_none());
    }

    #[test]
    fn test_top_n_ask_volume_sums_lowest_asks() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(100), dec!(2))).unwrap(); // best ask
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(5))).unwrap(); // worst
        // top 2: 100 (2) + 101 (3) = 5
        assert_eq!(b.top_n_ask_volume(2), dec!(5));
    }

    #[test]
    fn test_top_n_ask_volume_zero_for_empty_book() {
        let b = book("BTC-USD");
        assert_eq!(b.top_n_ask_volume(3), dec!(0));
    }

    // --- has_ask_at / bid_ask_depth ---

    #[test]
    fn test_has_ask_at_true_when_ask_exists() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        assert!(b.has_ask_at(dec!(101)));
    }

    #[test]
    fn test_has_ask_at_false_when_no_ask_at_price() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(1))).unwrap();
        assert!(!b.has_ask_at(dec!(101)));
    }

    #[test]
    fn test_bid_ask_depth_correct_counts() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert_eq!(b.bid_ask_depth(), (2, 1));
    }

    #[test]
    fn test_bid_ask_depth_zero_for_empty_book() {
        let b = book("BTC-USD");
        assert_eq!(b.bid_ask_depth(), (0, 0));
    }

    // --- OrderBook::best_bid_qty / best_ask_qty ---
    #[test]
    fn test_best_bid_qty_returns_top_bid_quantity() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(5))).unwrap();
        // best bid = 100 with qty=3
        assert_eq!(b.best_bid_qty(), Some(dec!(3)));
    }

    #[test]
    fn test_best_ask_qty_returns_top_ask_quantity() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(7))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(102), dec!(2))).unwrap();
        // best ask = 101 with qty=7
        assert_eq!(b.best_ask_qty(), Some(dec!(7)));
    }

    #[test]
    fn test_best_bid_qty_none_when_no_bids() {
        let b = book("BTC-USD");
        assert!(b.best_bid_qty().is_none());
    }

    #[test]
    fn test_best_ask_qty_none_when_no_asks() {
        let b = book("BTC-USD");
        assert!(b.best_ask_qty().is_none());
    }

    // --- OrderBook::total_book_volume ---
    #[test]
    fn test_total_book_volume_sum_of_bids_and_asks() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(3))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        assert_eq!(b.total_book_volume(), dec!(5));
    }

    #[test]
    fn test_total_book_volume_zero_on_empty_book() {
        let b = book("BTC-USD");
        assert_eq!(b.total_book_volume(), dec!(0));
    }

    // --- OrderBook::price_range_bids ---
    #[test]
    fn test_price_range_bids_correct_range() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(97), dec!(1))).unwrap();
        // best=100, worst=97 → range=3
        assert_eq!(b.price_range_bids(), Some(dec!(3)));
    }

    #[test]
    fn test_price_range_bids_none_with_single_bid() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        assert!(b.price_range_bids().is_none());
    }

    // ── OrderBook::is_tight_spread ────────────────────────────────────────────

    #[test]
    fn test_is_tight_spread_true_when_spread_at_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // spread = 1, threshold = 1 → tight
        assert!(b.is_tight_spread(dec!(1)));
    }

    #[test]
    fn test_is_tight_spread_false_when_spread_above_threshold() {
        let mut b = book("BTC-USD");
        b.apply(delta("BTC-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();
        b.apply(delta("BTC-USD", BookSide::Ask, dec!(103), dec!(1))).unwrap();
        // spread = 3, threshold = 1 → not tight
        assert!(!b.is_tight_spread(dec!(1)));
    }

    #[test]
    fn test_is_tight_spread_false_when_empty() {
        let b = book("BTC-USD");
        assert!(!b.is_tight_spread(dec!(10)));
    }

    // ── OrderBook::best_bid_price / best_ask_price ────────────────────────────

    #[test]
    fn test_best_bid_price_returns_price() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(5))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(98), dec!(3))).unwrap();
        assert_eq!(b.best_bid_price(), Some(dec!(99)));
    }

    #[test]
    fn test_best_ask_price_returns_price() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(5))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(102), dec!(3))).unwrap();
        assert_eq!(b.best_ask_price(), Some(dec!(101)));
    }

    #[test]
    fn test_best_bid_price_none_when_empty() {
        assert_eq!(book("X").best_bid_price(), None);
    }

    #[test]
    fn test_best_ask_price_none_when_empty() {
        assert_eq!(book("X").best_ask_price(), None);
    }

    // ── OrderBook::is_crossed ─────────────────────────────────────────────────

    #[test]
    fn test_is_crossed_false_when_normal() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(!b.is_crossed());
    }

    #[test]
    fn test_is_crossed_false_when_empty() {
        assert!(!book("X").is_crossed());
    }

    // ── OrderBook::has_bids / has_asks ────────────────────────────────────────

    #[test]
    fn test_has_bids_true_when_bid_present() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        assert!(b.has_bids());
    }

    #[test]
    fn test_has_bids_false_when_empty() {
        assert!(!book("X").has_bids());
    }

    #[test]
    fn test_has_asks_true_when_ask_present() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        assert!(b.has_asks());
    }

    #[test]
    fn test_has_asks_false_when_empty() {
        assert!(!book("X").has_asks());
    }

    // ── OrderBook::ask_price_range / bid_price_range ──────────────────────────

    #[test]
    fn test_ask_price_range_none_when_empty() {
        assert_eq!(book("X").ask_price_range(), None);
    }

    #[test]
    fn test_ask_price_range_zero_when_single_level() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(1))).unwrap();
        assert_eq!(b.ask_price_range(), Some(dec!(0)));
    }

    #[test]
    fn test_ask_price_range_correct_with_multiple_levels() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(102), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(105), dec!(1))).unwrap();
        assert_eq!(b.ask_price_range(), Some(dec!(5)));
    }

    #[test]
    fn test_bid_price_range_none_when_empty() {
        assert_eq!(book("X").bid_price_range(), None);
    }

    #[test]
    fn test_bid_price_range_correct_with_multiple_levels() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(98), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(96), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(94), dec!(1))).unwrap();
        // best = 98, worst = 94, range = 4
        assert_eq!(b.bid_price_range(), Some(dec!(4)));
    }

    // ── OrderBook::mid_spread_ratio ───────────────────────────────────────────

    #[test]
    fn test_mid_spread_ratio_none_when_empty() {
        assert_eq!(book("X").mid_spread_ratio(), None);
    }

    #[test]
    fn test_mid_spread_ratio_correct() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // spread = 2, mid = 100, ratio = 2/100 = 0.02
        let ratio = b.mid_spread_ratio().unwrap();
        assert!((ratio - 0.02).abs() < 1e-9);
    }

    // ── OrderBook::volume_imbalance ────────────────────────────────────────────

    #[test]
    fn test_volume_imbalance_none_when_empty() {
        assert_eq!(book("X").volume_imbalance(), None);
    }

    #[test]
    fn test_volume_imbalance_positive_when_more_bids() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(3))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(1))).unwrap();
        // (3 - 1) / (3 + 1) = 0.5
        let imb = b.volume_imbalance().unwrap();
        assert!((imb - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_volume_imbalance_negative_when_more_asks() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(3))).unwrap();
        // (1 - 3) / (1 + 3) = -0.5
        let imb = b.volume_imbalance().unwrap();
        assert!((imb - (-0.5)).abs() < 1e-9);
    }

    #[test]
    fn test_volume_imbalance_zero_when_equal() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(2))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        let imb = b.volume_imbalance().unwrap();
        assert!(imb.abs() < 1e-9);
    }

    // ── OrderBook::ask_volume_within / bid_volume_within ─────────────────────

    #[test]
    fn test_ask_volume_within_sums_levels_in_range() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(2))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(3))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(105), dec!(10))).unwrap();
        // best ask = 100, range = 2 → include 100 and 101 (102 is limit, 105 outside)
        let vol = b.ask_volume_within(dec!(2));
        assert_eq!(vol, dec!(5)); // 2 + 3
    }

    #[test]
    fn test_ask_volume_within_zero_when_empty() {
        assert_eq!(book("X").ask_volume_within(dec!(10)), dec!(0));
    }

    #[test]
    fn test_bid_volume_within_sums_levels_in_range() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(100), dec!(5))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(3))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(95), dec!(10))).unwrap();
        // best bid = 100, range = 2 → include 100 and 99 (floor=98, 95 outside)
        let vol = b.bid_volume_within(dec!(2));
        assert_eq!(vol, dec!(8)); // 5 + 3
    }

    #[test]
    fn test_bid_volume_within_zero_when_empty() {
        assert_eq!(book("X").bid_volume_within(dec!(10)), dec!(0));
    }

    // ── OrderBook::ask_level_count / bid_level_count ─────────────────────────

    #[test]
    fn test_ask_level_count_zero_when_empty() {
        assert_eq!(book("X").ask_level_count(), 0);
    }

    #[test]
    fn test_ask_level_count_correct() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(2))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(102), dec!(3))).unwrap();
        assert_eq!(b.ask_level_count(), 3);
    }

    #[test]
    fn test_bid_level_count_correct() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(1))).unwrap();
        b.apply(delta("X", BookSide::Bid, dec!(98), dec!(2))).unwrap();
        assert_eq!(b.bid_level_count(), 2);
    }

    // ── OrderBook::price_impact_buy / price_impact_sell ──────────────────────

    #[test]
    fn test_price_impact_buy_correct_single_level() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(10))).unwrap();
        // buy 5 units all at 100 → avg = 100
        assert_eq!(b.price_impact_buy(dec!(5)), Some(dec!(100)));
    }

    #[test]
    fn test_price_impact_buy_spans_levels() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(5))).unwrap();
        b.apply(delta("X", BookSide::Ask, dec!(101), dec!(5))).unwrap();
        // buy 10: 5@100 + 5@101 → avg = 100.5
        assert_eq!(b.price_impact_buy(dec!(10)), Some(dec!(100.5)));
    }

    #[test]
    fn test_price_impact_buy_none_when_insufficient_liquidity() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Ask, dec!(100), dec!(3))).unwrap();
        assert!(b.price_impact_buy(dec!(5)).is_none());
    }

    #[test]
    fn test_price_impact_sell_correct_single_level() {
        let mut b = book("X");
        b.apply(delta("X", BookSide::Bid, dec!(99), dec!(10))).unwrap();
        assert_eq!(b.price_impact_sell(dec!(5)), Some(dec!(99)));
    }
}
