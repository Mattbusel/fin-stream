//! Limit order book simulation with price-time priority matching.
//!
//! Provides a full limit order book simulation engine with:
//! - Price-time priority matching
//! - BTreeMap-based bid/ask book with FIFO queues per price level
//! - Market and limit order submission
//! - Order cancellation
//! - Book snapshot and spread computation

use std::collections::{BTreeMap, VecDeque};

/// Unique order identifier.
pub type OrderId = u64;

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Buy side.
    Bid,
    /// Sell side.
    Ask,
}

/// Order lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    /// Order is resting in the book.
    Open,
    /// Order is partially filled.
    PartiallyFilled,
    /// Order is fully filled.
    Filled,
    /// Order was cancelled.
    Cancelled,
}

/// A limit order resting in the book.
#[derive(Debug, Clone)]
pub struct LimitOrder {
    /// Unique order ID.
    pub id: OrderId,
    /// Order side (bid or ask).
    pub side: Side,
    /// Limit price.
    pub price: f64,
    /// Original order quantity.
    pub qty: f64,
    /// Remaining unfilled quantity.
    pub remaining_qty: f64,
    /// Current order status.
    pub status: OrderStatus,
    /// Submission timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Trader identifier.
    pub trader_id: String,
}

/// A single trade execution record.
#[derive(Debug, Clone)]
pub struct Trade {
    /// ID of the aggressor (incoming) order.
    pub aggressor_id: OrderId,
    /// ID of the resting (passive) order.
    pub resting_id: OrderId,
    /// Execution price.
    pub price: f64,
    /// Executed quantity.
    pub qty: f64,
    /// Execution timestamp in milliseconds.
    pub timestamp_ms: u64,
}

/// Convert a price to a BTreeMap key.
///
/// Bids use `u64::MAX - (price * 10000) as u64` so that higher prices sort first.
/// Asks use `(price * 10000) as u64` so that lower prices sort first.
pub fn price_to_key(price: f64, side: Side) -> u64 {
    let scaled = (price * 10_000.0).round() as u64;
    match side {
        Side::Bid => u64::MAX - scaled,
        Side::Ask => scaled,
    }
}

/// Convert a BTreeMap key back to price.
fn key_to_price(key: u64, side: Side) -> f64 {
    match side {
        Side::Bid => (u64::MAX - key) as f64 / 10_000.0,
        Side::Ask => key as f64 / 10_000.0,
    }
}

/// Limit order book simulator with price-time priority matching.
pub struct OrderBookSim {
    /// Bid side: price_key → FIFO queue of resting orders.
    pub bids: BTreeMap<u64, VecDeque<LimitOrder>>,
    /// Ask side: price_key → FIFO queue of resting orders.
    pub asks: BTreeMap<u64, VecDeque<LimitOrder>>,
    /// Next order ID to assign.
    pub next_id: u64,
    /// Symbol this book tracks.
    pub symbol: String,
    /// All trades executed so far.
    pub trades: Vec<Trade>,
}

impl OrderBookSim {
    /// Create a new empty order book for the given symbol.
    pub fn new(symbol: &str) -> Self {
        OrderBookSim {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            next_id: 1,
            symbol: symbol.to_string(),
            trades: Vec::new(),
        }
    }

    /// Submit a limit order. Attempts to match immediately; remainder rests in book.
    ///
    /// Returns the assigned order ID.
    pub fn submit_limit(
        &mut self,
        side: Side,
        price: f64,
        qty: f64,
        trader_id: &str,
        timestamp_ms: u64,
    ) -> OrderId {
        let id = self.next_id;
        self.next_id += 1;

        let mut order = LimitOrder {
            id,
            side,
            price,
            qty,
            remaining_qty: qty,
            status: OrderStatus::Open,
            timestamp_ms,
            trader_id: trader_id.to_string(),
        };

        // Try to match the incoming order against the opposite side
        let new_trades = Self::match_incoming(
            &mut order,
            &mut self.bids,
            &mut self.asks,
            timestamp_ms,
        );
        self.trades.extend(new_trades);

        // If not fully filled, rest in book
        if order.remaining_qty > 0.0 && order.status != OrderStatus::Filled {
            let key = price_to_key(price, side);
            match side {
                Side::Bid => self.bids.entry(key).or_default().push_back(order),
                Side::Ask => self.asks.entry(key).or_default().push_back(order),
            }
        }
        id
    }

    /// Submit a market order. Matches against the best available prices.
    ///
    /// Returns all trades generated.
    pub fn submit_market(
        &mut self,
        side: Side,
        qty: f64,
        trader_id: &str,
        timestamp_ms: u64,
    ) -> Vec<Trade> {
        let id = self.next_id;
        self.next_id += 1;

        let mut order = LimitOrder {
            id,
            side,
            price: match side {
                Side::Bid => f64::INFINITY,
                Side::Ask => 0.0,
            },
            qty,
            remaining_qty: qty,
            status: OrderStatus::Open,
            timestamp_ms,
            trader_id: trader_id.to_string(),
        };

        let new_trades = Self::match_incoming(
            &mut order,
            &mut self.bids,
            &mut self.asks,
            timestamp_ms,
        );
        self.trades.extend(new_trades.clone());
        new_trades
    }

    /// Attempt to match `order` against the opposite side of the book.
    fn match_incoming(
        order: &mut LimitOrder,
        bids: &mut BTreeMap<u64, VecDeque<LimitOrder>>,
        asks: &mut BTreeMap<u64, VecDeque<LimitOrder>>,
        now_ms: u64,
    ) -> Vec<Trade> {
        let mut trades = Vec::new();

        match order.side {
            Side::Bid => {
                // Match against asks (lowest ask first = smallest key)
                let mut keys_to_remove = Vec::new();
                for (&key, queue) in asks.iter_mut() {
                    let ask_price = key_to_price(key, Side::Ask);
                    if ask_price > order.price {
                        break; // No more matchable levels
                    }
                    if order.remaining_qty <= 0.0 {
                        break;
                    }
                    let mut empties = Vec::new();
                    for (idx, resting) in queue.iter_mut().enumerate() {
                        if order.remaining_qty <= 0.0 {
                            break;
                        }
                        let fill_qty = order.remaining_qty.min(resting.remaining_qty);
                        let trade = Trade {
                            aggressor_id: order.id,
                            resting_id: resting.id,
                            price: resting.price,
                            qty: fill_qty,
                            timestamp_ms: now_ms,
                        };
                        trades.push(trade);
                        order.remaining_qty -= fill_qty;
                        resting.remaining_qty -= fill_qty;
                        if resting.remaining_qty <= 0.0 {
                            resting.status = OrderStatus::Filled;
                            empties.push(idx);
                        } else {
                            resting.status = OrderStatus::PartiallyFilled;
                        }
                    }
                    // Remove filled resting orders (in reverse to preserve indices)
                    for &idx in empties.iter().rev() {
                        queue.remove(idx);
                    }
                    if queue.is_empty() {
                        keys_to_remove.push(key);
                    }
                }
                for key in keys_to_remove {
                    asks.remove(&key);
                }
            }
            Side::Ask => {
                // Match against bids (highest bid first = smallest key in bid map)
                let mut keys_to_remove = Vec::new();
                for (&key, queue) in bids.iter_mut() {
                    let bid_price = key_to_price(key, Side::Bid);
                    if bid_price < order.price {
                        break;
                    }
                    if order.remaining_qty <= 0.0 {
                        break;
                    }
                    let mut empties = Vec::new();
                    for (idx, resting) in queue.iter_mut().enumerate() {
                        if order.remaining_qty <= 0.0 {
                            break;
                        }
                        let fill_qty = order.remaining_qty.min(resting.remaining_qty);
                        let trade = Trade {
                            aggressor_id: order.id,
                            resting_id: resting.id,
                            price: resting.price,
                            qty: fill_qty,
                            timestamp_ms: now_ms,
                        };
                        trades.push(trade);
                        order.remaining_qty -= fill_qty;
                        resting.remaining_qty -= fill_qty;
                        if resting.remaining_qty <= 0.0 {
                            resting.status = OrderStatus::Filled;
                            empties.push(idx);
                        } else {
                            resting.status = OrderStatus::PartiallyFilled;
                        }
                    }
                    for &idx in empties.iter().rev() {
                        queue.remove(idx);
                    }
                    if queue.is_empty() {
                        keys_to_remove.push(key);
                    }
                }
                for key in keys_to_remove {
                    bids.remove(&key);
                }
            }
        }

        if order.remaining_qty <= 0.0 {
            order.status = OrderStatus::Filled;
        } else if order.remaining_qty < order.qty {
            order.status = OrderStatus::PartiallyFilled;
        }

        trades
    }

    /// Run the matching engine: check if best bid >= best ask and match.
    ///
    /// Returns all trades generated in this matching cycle.
    pub fn match_orders(&mut self, now_ms: u64) -> Vec<Trade> {
        let mut trades = Vec::new();

        loop {
            // Check if there is a crossing
            let best_bid_key = self.bids.keys().next().copied();
            let best_ask_key = self.asks.keys().next().copied();

            let (bid_key, ask_key) = match (best_bid_key, best_ask_key) {
                (Some(b), Some(a)) => (b, a),
                _ => break,
            };

            let bid_price = key_to_price(bid_key, Side::Bid);
            let ask_price = key_to_price(ask_key, Side::Ask);

            if bid_price < ask_price {
                break; // No crossing
            }

            // Match best bid against best ask
            let bid_queue = self.bids.get_mut(&bid_key).unwrap();
            let mut bid_order = bid_queue.pop_front().unwrap();
            if bid_queue.is_empty() {
                self.bids.remove(&bid_key);
            }

            let ask_queue = self.asks.get_mut(&ask_key).unwrap();
            let ask_order = ask_queue.front_mut().unwrap();

            let fill_qty = bid_order.remaining_qty.min(ask_order.remaining_qty);
            let fill_price = ask_order.price; // resting order price

            let trade = Trade {
                aggressor_id: bid_order.id,
                resting_id: ask_order.id,
                price: fill_price,
                qty: fill_qty,
                timestamp_ms: now_ms,
            };
            trades.push(trade.clone());
            self.trades.push(trade);

            bid_order.remaining_qty -= fill_qty;
            ask_order.remaining_qty -= fill_qty;

            if ask_order.remaining_qty <= 0.0 {
                ask_order.status = OrderStatus::Filled;
                ask_queue.pop_front();
                if ask_queue.is_empty() {
                    self.asks.remove(&ask_key);
                }
            } else {
                ask_order.status = OrderStatus::PartiallyFilled;
            }

            if bid_order.remaining_qty > 0.0 {
                bid_order.status = OrderStatus::PartiallyFilled;
                self.bids.entry(bid_key).or_default().push_front(bid_order);
            }
        }

        trades
    }

    /// Cancel an order by ID and side. Returns true if the order was found and removed.
    pub fn cancel_order(&mut self, id: OrderId, side: Side) -> bool {
        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let mut found = false;
        let mut keys_to_remove = Vec::new();
        for (&key, queue) in book.iter_mut() {
            if let Some(pos) = queue.iter().position(|o| o.id == id) {
                queue.remove(pos);
                found = true;
                if queue.is_empty() {
                    keys_to_remove.push(key);
                }
                break;
            }
        }
        for key in keys_to_remove {
            book.remove(&key);
        }
        found
    }

    /// Best bid price, if any orders exist on the bid side.
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.keys().next().map(|&k| key_to_price(k, Side::Bid))
    }

    /// Best ask price, if any orders exist on the ask side.
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.keys().next().map(|&k| key_to_price(k, Side::Ask))
    }

    /// Bid-ask spread, if both sides have at least one order.
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some(a - b),
            _ => None,
        }
    }

    /// Snapshot of the order book at a given depth.
    ///
    /// Returns `(bids, asks)` where each is a vec of `(price, total_qty)` at that level.
    pub fn order_book_snapshot(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let bids: Vec<(f64, f64)> = self
            .bids
            .iter()
            .take(depth)
            .map(|(&key, queue)| {
                let price = key_to_price(key, Side::Bid);
                let total_qty: f64 = queue.iter().map(|o| o.remaining_qty).sum();
                (price, total_qty)
            })
            .collect();

        let asks: Vec<(f64, f64)> = self
            .asks
            .iter()
            .take(depth)
            .map(|(&key, queue)| {
                let price = key_to_price(key, Side::Ask);
                let total_qty: f64 = queue.iter().map(|o| o.remaining_qty).sum();
                (price, total_qty)
            })
            .collect();

        (bids, asks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_limit_order_rests_when_no_match() {
        let mut book = OrderBookSim::new("AAPL");
        let id = book.submit_limit(Side::Bid, 100.0, 10.0, "trader1", 1000);
        assert_eq!(book.trades.len(), 0);
        assert!(book.best_bid().is_some());
        assert_eq!(book.best_bid().unwrap(), 100.0);
        assert_eq!(id, 1);
    }

    #[test]
    fn test_market_order_fills_against_limit() {
        let mut book = OrderBookSim::new("AAPL");
        book.submit_limit(Side::Ask, 101.0, 5.0, "seller", 1000);
        let trades = book.submit_market(Side::Bid, 3.0, "buyer", 1001);
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].qty, 3.0);
        assert_eq!(trades[0].price, 101.0);
    }

    #[test]
    fn test_two_limits_cross_and_trade() {
        let mut book = OrderBookSim::new("MSFT");
        book.submit_limit(Side::Ask, 100.0, 10.0, "seller", 1000);
        // Buyer posts limit above ask — should cross immediately
        book.submit_limit(Side::Bid, 101.0, 5.0, "buyer", 1001);
        assert_eq!(book.trades.len(), 1);
        assert_eq!(book.trades[0].qty, 5.0);
        assert_eq!(book.trades[0].price, 100.0); // resting order's price
    }

    #[test]
    fn test_cancel_removes_order() {
        let mut book = OrderBookSim::new("TSLA");
        let id = book.submit_limit(Side::Bid, 200.0, 8.0, "trader", 1000);
        assert!(book.best_bid().is_some());
        let cancelled = book.cancel_order(id, Side::Bid);
        assert!(cancelled);
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn test_spread_calculation() {
        let mut book = OrderBookSim::new("SPY");
        book.submit_limit(Side::Bid, 449.50, 10.0, "mm", 1000);
        book.submit_limit(Side::Ask, 449.55, 10.0, "mm", 1000);
        let spread = book.spread().unwrap();
        assert!((spread - 0.05).abs() < 0.001, "Spread should be 0.05, got {}", spread);
    }

    #[test]
    fn test_price_time_priority() {
        let mut book = OrderBookSim::new("ETH");
        // Two asks at same price, submit order — first ask should fill first
        book.submit_limit(Side::Ask, 2000.0, 5.0, "seller1", 1000);
        book.submit_limit(Side::Ask, 2000.0, 5.0, "seller2", 1001);
        let trades = book.submit_market(Side::Bid, 5.0, "buyer", 1002);
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].resting_id, 1); // first ask should fill first
    }

    #[test]
    fn test_snapshot_depth() {
        let mut book = OrderBookSim::new("BTC");
        book.submit_limit(Side::Bid, 50000.0, 1.0, "mm", 1000);
        book.submit_limit(Side::Bid, 49900.0, 2.0, "mm", 1001);
        book.submit_limit(Side::Ask, 50100.0, 1.5, "mm", 1002);
        let (bids, asks) = book.order_book_snapshot(5);
        assert_eq!(bids.len(), 2);
        assert_eq!(asks.len(), 1);
        assert_eq!(bids[0].0, 50000.0); // best bid first
    }
}
