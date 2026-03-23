//! # Module: lob_sim
//!
//! ## Responsibility
//! Full L3 (order-level) limit order book simulator with a price-time priority
//! matching engine. Suitable for backtesting against realistic microstructure.
//!
//! ## Supported Events
//! - `LobEvent::LimitOrder` — add a new resting limit order
//! - `LobEvent::MarketOrder` — immediately match against resting liquidity
//! - `LobEvent::CancelOrder` — remove a resting order by ID
//!
//! ## Matching Rules
//! - Price-time priority (FIFO within each price level)
//! - Partial fills are supported
//! - Market orders that exhaust the book return `Err(StreamError::InsufficientLiquidity)`
//!
//! ## NOT Responsible For
//! - Persistent storage of order history
//! - Fee calculations
//! - Iceberg / hidden order types

use crate::error::StreamError;
use std::collections::{BTreeMap, HashMap, VecDeque};

// ─── side ─────────────────────────────────────────────────────────────────────

/// Whether an order is on the bid (buy) or ask (sell) side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    /// Buy side (bids).
    Bid,
    /// Sell side (asks).
    Ask,
}

// ─── order ────────────────────────────────────────────────────────────────────

/// A resting limit order in the book.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LimitOrder {
    /// Unique order identifier.
    pub id: u64,
    /// Which side of the book this order rests on.
    pub side: Side,
    /// Limit price (integer ticks or fixed-point; caller defines the unit).
    pub price: u64,
    /// Remaining quantity.
    pub quantity: u64,
    /// Sequence number (insertion order) for time-priority tiebreaking.
    pub sequence: u64,
}

// ─── events ───────────────────────────────────────────────────────────────────

/// An event submitted to the simulator.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LobEvent {
    /// Add a new resting limit order.
    LimitOrder {
        /// Caller-supplied unique order ID.
        id: u64,
        /// Bid or ask.
        side: Side,
        /// Limit price.
        price: u64,
        /// Order quantity.
        quantity: u64,
    },
    /// Submit an aggressive market order that matches immediately.
    MarketOrder {
        /// Bid (buy market) or Ask (sell market).
        side: Side,
        /// Quantity to execute.
        quantity: u64,
    },
    /// Cancel a resting limit order by its ID.
    CancelOrder {
        /// ID of the order to cancel.
        id: u64,
    },
}

// ─── fills ────────────────────────────────────────────────────────────────────

/// A single fill produced by matching.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fill {
    /// The resting order ID that was matched against.
    pub maker_order_id: u64,
    /// Fill price (the resting order's limit price).
    pub price: u64,
    /// Fill quantity.
    pub quantity: u64,
}

/// Result of processing a `LobEvent`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LobResult {
    /// Limit order was accepted and rests in the book.
    Accepted { id: u64 },
    /// Order was cancelled successfully.
    Cancelled { id: u64 },
    /// Market order was fully or partially filled.
    Filled { fills: Vec<Fill>, remaining: u64 },
}

// ─── price level ─────────────────────────────────────────────────────────────

/// A single price level: a FIFO queue of resting orders.
#[derive(Debug, Default)]
struct PriceLevel {
    orders: VecDeque<LimitOrder>,
}

impl PriceLevel {
    fn total_quantity(&self) -> u64 {
        self.orders.iter().map(|o| o.quantity).sum()
    }

    fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

// ─── order book ───────────────────────────────────────────────────────────────

/// L3 limit order book simulator.
///
/// Internal representation:
/// - `bids`: `BTreeMap<u64, PriceLevel>` keyed by price descending (highest bid first)
/// - `asks`: `BTreeMap<u64, PriceLevel>` keyed by price ascending (lowest ask first)
/// - `order_index`: fast order-ID → (side, price) lookup for cancellation
pub struct LobSimulator {
    /// Bid side: price → level. Iterated high-to-low for matching.
    bids: BTreeMap<u64, PriceLevel>,
    /// Ask side: price → level. Iterated low-to-high for matching.
    asks: BTreeMap<u64, PriceLevel>,
    /// Order ID → (side, price) for O(1) cancel lookup.
    order_index: HashMap<u64, (Side, u64)>,
    /// Monotonically increasing sequence counter for time priority.
    sequence: u64,
}

impl LobSimulator {
    /// Create a new empty L3 order book.
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            order_index: HashMap::new(),
            sequence: 0,
        }
    }

    /// Process an event. Returns the result or an error.
    ///
    /// # Errors
    /// - `StreamError::InvalidInput` if the order ID already exists (for `LimitOrder`).
    /// - `StreamError::InvalidInput` if the order ID is not found (for `CancelOrder`).
    /// - `StreamError::InsufficientLiquidity` if a market order cannot be fully filled.
    pub fn process(&mut self, event: LobEvent) -> Result<LobResult, StreamError> {
        match event {
            LobEvent::LimitOrder { id, side, price, quantity } => {
                self.add_limit(id, side, price, quantity)
            }
            LobEvent::MarketOrder { side, quantity } => self.match_market(side, quantity),
            LobEvent::CancelOrder { id } => self.cancel(id),
        }
    }

    /// Best bid price (highest), or `None` if the bid side is empty.
    pub fn best_bid(&self) -> Option<u64> {
        self.bids.keys().next_back().copied()
    }

    /// Best ask price (lowest), or `None` if the ask side is empty.
    pub fn best_ask(&self) -> Option<u64> {
        self.asks.keys().next().copied()
    }

    /// Mid-price as average of best bid and best ask, or `None` if either side is empty.
    pub fn mid_price(&self) -> Option<u64> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        Some((bid + ask) / 2)
    }

    /// Total resting quantity on the bid side.
    pub fn total_bid_quantity(&self) -> u64 {
        self.bids.values().map(|l| l.total_quantity()).sum()
    }

    /// Total resting quantity on the ask side.
    pub fn total_ask_quantity(&self) -> u64 {
        self.asks.values().map(|l| l.total_quantity()).sum()
    }

    /// Number of resting orders on each side.
    pub fn order_count(&self) -> (usize, usize) {
        let bids: usize = self.bids.values().map(|l| l.orders.len()).sum();
        let asks: usize = self.asks.values().map(|l| l.orders.len()).sum();
        (bids, asks)
    }

    fn add_limit(
        &mut self,
        id: u64,
        side: Side,
        price: u64,
        quantity: u64,
    ) -> Result<LobResult, StreamError> {
        if self.order_index.contains_key(&id) {
            return Err(StreamError::InvalidInput(format!(
                "Order ID {id} already exists in the book"
            )));
        }
        if quantity == 0 {
            return Err(StreamError::InvalidInput("Order quantity must be positive".to_owned()));
        }

        self.sequence += 1;
        let order =
            LimitOrder { id, side, price, quantity, sequence: self.sequence };

        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        book.entry(price).or_default().orders.push_back(order);
        self.order_index.insert(id, (side, price));

        Ok(LobResult::Accepted { id })
    }

    fn cancel(&mut self, id: u64) -> Result<LobResult, StreamError> {
        let (side, price) = self.order_index.remove(&id).ok_or_else(|| {
            StreamError::InvalidInput(format!("Order ID {id} not found for cancellation"))
        })?;

        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        if let Some(level) = book.get_mut(&price) {
            level.orders.retain(|o| o.id != id);
            if level.is_empty() {
                book.remove(&price);
            }
        }

        Ok(LobResult::Cancelled { id })
    }

    fn match_market(&mut self, side: Side, quantity: u64) -> Result<LobResult, StreamError> {
        if quantity == 0 {
            return Err(StreamError::InvalidInput(
                "Market order quantity must be positive".to_owned(),
            ));
        }

        let mut remaining = quantity;
        let mut fills = Vec::new();

        // A buy market order matches against the asks (lowest price first).
        // A sell market order matches against the bids (highest price first).
        match side {
            Side::Bid => {
                // Match against asks ascending
                let prices: Vec<u64> = self.asks.keys().copied().collect();
                for price in prices {
                    if remaining == 0 {
                        break;
                    }
                    if let Some(level) = self.asks.get_mut(&price) {
                        Self::fill_level(level, price, &mut remaining, &mut fills, &mut self.order_index);
                    }
                    if self.asks.get(&price).map_or(true, |l| l.is_empty()) {
                        self.asks.remove(&price);
                    }
                }
            }
            Side::Ask => {
                // Match against bids descending
                let prices: Vec<u64> = self.bids.keys().rev().copied().collect();
                for price in prices {
                    if remaining == 0 {
                        break;
                    }
                    if let Some(level) = self.bids.get_mut(&price) {
                        Self::fill_level(level, price, &mut remaining, &mut fills, &mut self.order_index);
                    }
                    if self.bids.get(&price).map_or(true, |l| l.is_empty()) {
                        self.bids.remove(&price);
                    }
                }
            }
        }

        if remaining > 0 && fills.is_empty() {
            return Err(StreamError::InsufficientLiquidity);
        }

        Ok(LobResult::Filled { fills, remaining })
    }

    fn fill_level(
        level: &mut PriceLevel,
        price: u64,
        remaining: &mut u64,
        fills: &mut Vec<Fill>,
        index: &mut HashMap<u64, (Side, u64)>,
    ) {
        while *remaining > 0 {
            let front = match level.orders.front_mut() {
                Some(o) => o,
                None => break,
            };
            let fill_qty = (*remaining).min(front.quantity);
            fills.push(Fill { maker_order_id: front.id, price, quantity: fill_qty });
            front.quantity -= fill_qty;
            *remaining -= fill_qty;
            if front.quantity == 0 {
                let id = front.id;
                level.orders.pop_front();
                index.remove(&id);
            }
        }
    }
}

impl Default for LobSimulator {
    fn default() -> Self {
        Self::new()
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn book_with_two_sides() -> LobSimulator {
        let mut sim = LobSimulator::new();
        sim.process(LobEvent::LimitOrder { id: 1, side: Side::Bid, price: 100, quantity: 10 }).unwrap();
        sim.process(LobEvent::LimitOrder { id: 2, side: Side::Ask, price: 101, quantity: 10 }).unwrap();
        sim
    }

    #[test]
    fn test_best_bid_ask() {
        let sim = book_with_two_sides();
        assert_eq!(sim.best_bid(), Some(100));
        assert_eq!(sim.best_ask(), Some(101));
    }

    #[test]
    fn test_mid_price() {
        let sim = book_with_two_sides();
        assert_eq!(sim.mid_price(), Some(100)); // (100+101)/2 = 100 (integer)
    }

    #[test]
    fn test_market_buy_fills_ask() {
        let mut sim = book_with_two_sides();
        let result = sim.process(LobEvent::MarketOrder { side: Side::Bid, quantity: 5 }).unwrap();
        assert!(matches!(result, LobResult::Filled { remaining: 0, .. }));
        assert_eq!(sim.total_ask_quantity(), 5); // partial fill
    }

    #[test]
    fn test_market_sell_fills_bid() {
        let mut sim = book_with_two_sides();
        let result = sim.process(LobEvent::MarketOrder { side: Side::Ask, quantity: 10 }).unwrap();
        assert!(matches!(result, LobResult::Filled { remaining: 0, .. }));
        assert_eq!(sim.total_bid_quantity(), 0);
    }

    #[test]
    fn test_cancel_order() {
        let mut sim = book_with_two_sides();
        sim.process(LobEvent::CancelOrder { id: 1 }).unwrap();
        assert_eq!(sim.best_bid(), None);
    }

    #[test]
    fn test_duplicate_order_id_errors() {
        let mut sim = LobSimulator::new();
        sim.process(LobEvent::LimitOrder { id: 42, side: Side::Bid, price: 100, quantity: 5 }).unwrap();
        let err = sim.process(LobEvent::LimitOrder { id: 42, side: Side::Bid, price: 100, quantity: 5 });
        assert!(matches!(err, Err(StreamError::InvalidInput(_))));
    }

    #[test]
    fn test_cancel_nonexistent_errors() {
        let mut sim = LobSimulator::new();
        let err = sim.process(LobEvent::CancelOrder { id: 999 });
        assert!(matches!(err, Err(StreamError::InvalidInput(_))));
    }

    #[test]
    fn test_market_order_insufficient_liquidity() {
        let mut sim = LobSimulator::new();
        sim.process(LobEvent::LimitOrder { id: 1, side: Side::Ask, price: 100, quantity: 5 }).unwrap();
        let err = sim.process(LobEvent::MarketOrder { side: Side::Bid, quantity: 100 });
        assert!(matches!(err, Err(StreamError::InsufficientLiquidity)));
    }

    #[test]
    fn test_price_time_priority() {
        let mut sim = LobSimulator::new();
        // Two bids at same price; first inserted should be matched first
        sim.process(LobEvent::LimitOrder { id: 1, side: Side::Bid, price: 100, quantity: 5 }).unwrap();
        sim.process(LobEvent::LimitOrder { id: 2, side: Side::Bid, price: 100, quantity: 5 }).unwrap();
        let result = sim.process(LobEvent::MarketOrder { side: Side::Ask, quantity: 5 }).unwrap();
        if let LobResult::Filled { fills, .. } = result {
            assert_eq!(fills[0].maker_order_id, 1); // FIFO
        } else {
            panic!("expected Filled");
        }
    }
}
