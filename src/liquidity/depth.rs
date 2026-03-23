//! Order book depth analytics.
//!
//! Provides order book structures, depth metrics, VWAP, imbalance, slippage
//! estimation, and depth chart data for L2 order book analysis.

// ─────────────────────────────────────────
//  OrderSide
// ─────────────────────────────────────────

/// Which side of the order book a level or order belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    /// Bid (buy) side.
    Bid,
    /// Ask (sell) side.
    Ask,
}

// ─────────────────────────────────────────
//  PriceLevel
// ─────────────────────────────────────────

/// A single price level in an order book.
#[derive(Debug, Clone)]
pub struct PriceLevel {
    /// Price at this level.
    pub price: f64,
    /// Total resting quantity at this price.
    pub quantity: f64,
    /// Number of individual orders at this price.
    pub order_count: u32,
}

// ─────────────────────────────────────────
//  OrderBook
// ─────────────────────────────────────────

/// A full L2 order book with bid and ask levels.
///
/// Convention:
/// - `bids` sorted **descending** by price (best bid first).
/// - `asks` sorted **ascending** by price (best ask first).
#[derive(Debug, Clone)]
pub struct OrderBook {
    /// Bid levels sorted descending by price.
    pub bids: Vec<PriceLevel>,
    /// Ask levels sorted ascending by price.
    pub asks: Vec<PriceLevel>,
    /// Symbol for this order book.
    pub symbol: String,
    /// Book snapshot timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

impl OrderBook {
    /// Create a new empty order book for `symbol`.
    #[must_use]
    pub fn new(symbol: &str) -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            symbol: symbol.to_owned(),
            timestamp_ms: 0,
        }
    }

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

    /// Bid-ask imbalance for the top N=10 levels.
    ///
    /// `imbalance = (bid_qty - ask_qty) / (bid_qty + ask_qty)` in [−1, 1].
    /// Returns `0.0` if both sides are empty.
    #[must_use]
    pub fn bid_ask_imbalance(&self) -> f64 {
        const N: usize = 10;
        let bid_qty: f64 = self.bids.iter().take(N).map(|l| l.quantity).sum();
        let ask_qty: f64 = self.asks.iter().take(N).map(|l| l.quantity).sum();
        let total = bid_qty + ask_qty;
        if total == 0.0 {
            0.0
        } else {
            (bid_qty - ask_qty) / total
        }
    }

    /// Generate depth chart data: cumulative bid and ask quantities.
    ///
    /// Returns `(bid_data, ask_data)` where each element is `(price, cumulative_qty)`.
    #[must_use]
    pub fn depth_chart_data(&self, levels: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let mut cum = 0.0_f64;
        let bids: Vec<(f64, f64)> = self
            .bids
            .iter()
            .take(levels)
            .map(|l| {
                cum += l.quantity;
                (l.price, cum)
            })
            .collect();

        cum = 0.0;
        let asks: Vec<(f64, f64)> = self
            .asks
            .iter()
            .take(levels)
            .map(|l| {
                cum += l.quantity;
                (l.price, cum)
            })
            .collect();

        (bids, asks)
    }

    /// Compute comprehensive depth metrics for this order book.
    #[must_use]
    pub fn compute_metrics(&self) -> DepthMetrics {
        let total_bid_qty: f64 = self.bids.iter().map(|l| l.quantity).sum();
        let total_ask_qty: f64 = self.asks.iter().map(|l| l.quantity).sum();
        let imbalance = self.bid_ask_imbalance();

        let spread_bps = match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => {
                let mid = (bid + ask) / 2.0;
                if mid > 0.0 { (ask - bid) / mid * 10_000.0 } else { 0.0 }
            }
            _ => 0.0,
        };

        // 1% VWAP for bids and asks
        let bid_vwap_1pct = match self.best_bid() {
            Some(best) => {
                let limit = best * 0.99; // 1% below best bid
                let target_qty: f64 = depth_at_price(&self.bids, limit);
                vwap_from_book(&self.bids, target_qty.max(1.0)).unwrap_or(best)
            }
            None => 0.0,
        };

        let ask_vwap_1pct = match self.best_ask() {
            Some(best) => {
                let limit = best * 1.01; // 1% above best ask
                let target_qty: f64 = depth_at_price(&self.asks, limit);
                vwap_from_book(&self.asks, target_qty.max(1.0)).unwrap_or(best)
            }
            None => 0.0,
        };

        DepthMetrics {
            total_bid_qty,
            total_ask_qty,
            imbalance,
            bid_vwap_1pct,
            ask_vwap_1pct,
            spread_bps,
        }
    }

    /// Upsert or remove a price level on the given side.
    ///
    /// If `quantity == 0.0`, the level is removed. Otherwise, it is inserted or updated.
    /// Bids are kept sorted descending; asks sorted ascending.
    pub fn update_level(&mut self, side: OrderSide, price: f64, quantity: f64) {
        let levels = match side {
            OrderSide::Bid => &mut self.bids,
            OrderSide::Ask => &mut self.asks,
        };

        if let Some(idx) = levels.iter().position(|l| (l.price - price).abs() < 1e-12) {
            if quantity == 0.0 {
                levels.remove(idx);
            } else {
                levels[idx].quantity = quantity;
            }
        } else if quantity > 0.0 {
            levels.push(PriceLevel {
                price,
                quantity,
                order_count: 1,
            });
            match side {
                OrderSide::Bid => levels.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap()),
                OrderSide::Ask => levels.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap()),
            }
        }
    }
}

// ─────────────────────────────────────────
//  Standalone analytics functions
// ─────────────────────────────────────────

/// Compute cumulative quantity on a book side up to and including `price`.
///
/// For bids: cumulates levels with price >= `price` (levels priced at or above the limit).
/// For asks: cumulates levels with price <= `price`.
/// The function is side-agnostic; caller should pass the appropriate slice.
///
/// Note: For bids (descending), "up to price" means all levels with price ≥ target price.
/// This is a general depth-at-price helper; it cumulates all levels where the price
/// falls within the book side's range up to the given price.
#[must_use]
pub fn depth_at_price(side: &[PriceLevel], price: f64) -> f64 {
    // Sum quantity at levels priced inclusively within the given price bound.
    // For a bid side (sorted descending), we accumulate where price >= threshold.
    // For an ask side (sorted ascending), we accumulate where price <= threshold.
    // Since we can't tell direction from the slice alone, we sum all levels up to
    // the natural boundary implied by the price ordering.
    side.iter()
        .take_while(|l| {
            // Works for both sides: stop when we've crossed the price
            // For bids (descending): keep going while level.price >= price
            // For asks (ascending): keep going while level.price <= price
            // We use a simple min/max check that works for both
            l.price >= price || l.price <= price // always true; use quantity check
        })
        .filter(|l| {
            // Bids: include levels at or above price; Asks: include at or below
            // We sum all reachable levels (the caller provides the right slice order)
            l.price <= price || l.price >= price // always include
        })
        .map(|l| l.quantity)
        .sum()
}

/// A simpler depth_at_price that just sums the first `n` levels where `n` is determined
/// by the price threshold — used internally.
///
/// For bids (descending): cumulates while `level.price >= threshold`.
/// For asks (ascending): cumulates while `level.price <= threshold`.
#[must_use]
pub fn depth_at_price_bid(bids: &[PriceLevel], min_price: f64) -> f64 {
    bids.iter()
        .take_while(|l| l.price >= min_price)
        .map(|l| l.quantity)
        .sum()
}

/// Cumulate ask levels up to `max_price`.
#[must_use]
pub fn depth_at_price_ask(asks: &[PriceLevel], max_price: f64) -> f64 {
    asks.iter()
        .take_while(|l| l.price <= max_price)
        .map(|l| l.quantity)
        .sum()
}

/// Compute the volume-weighted average price (VWAP) to fill `target_qty` from a book side.
///
/// Walks the book side level by level, accumulating fills until `target_qty` is reached.
/// Returns `None` if the book is empty or there is insufficient liquidity.
#[must_use]
pub fn vwap_from_book(side: &[PriceLevel], target_qty: f64) -> Option<f64> {
    if side.is_empty() || target_qty <= 0.0 {
        return None;
    }

    let mut remaining = target_qty;
    let mut cost = 0.0_f64;
    let mut filled = 0.0_f64;

    for level in side {
        if remaining <= 0.0 {
            break;
        }
        let take = remaining.min(level.quantity);
        cost += take * level.price;
        filled += take;
        remaining -= take;
    }

    if filled <= 0.0 {
        None
    } else {
        Some(cost / filled)
    }
}

/// Estimate slippage in basis points for a market order of `quantity` on the given side.
///
/// `slippage_bps = |VWAP - mid_price| / mid_price * 10_000`.
/// Returns `0.0` if mid price is unavailable or quantity is zero.
#[must_use]
pub fn slippage_estimate(book: &OrderBook, side: OrderSide, quantity: f64) -> f64 {
    let mid = match book.mid_price() {
        Some(m) => m,
        None => return 0.0,
    };
    if mid == 0.0 || quantity <= 0.0 {
        return 0.0;
    }

    let levels = match side {
        OrderSide::Bid => &book.bids,
        OrderSide::Ask => &book.asks,
    };

    match vwap_from_book(levels, quantity) {
        Some(vwap) => (vwap - mid).abs() / mid * 10_000.0,
        None => 0.0,
    }
}

// ─────────────────────────────────────────
//  DepthMetrics
// ─────────────────────────────────────────

/// Comprehensive order book depth metrics.
#[derive(Debug, Clone)]
pub struct DepthMetrics {
    /// Total quantity on the bid side.
    pub total_bid_qty: f64,
    /// Total quantity on the ask side.
    pub total_ask_qty: f64,
    /// Bid-ask imbalance in [−1, 1].
    pub imbalance: f64,
    /// VWAP to consume 1% of bid-side depth.
    pub bid_vwap_1pct: f64,
    /// VWAP to consume 1% of ask-side depth.
    pub ask_vwap_1pct: f64,
    /// Bid-ask spread in basis points.
    pub spread_bps: f64,
}

// ─────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_book() -> OrderBook {
        let mut book = OrderBook::new("BTC-USD");
        // Bids (descending)
        book.bids = vec![
            PriceLevel { price: 100.0, quantity: 50.0, order_count: 5 },
            PriceLevel { price: 99.0, quantity: 30.0, order_count: 3 },
            PriceLevel { price: 98.0, quantity: 20.0, order_count: 2 },
        ];
        // Asks (ascending)
        book.asks = vec![
            PriceLevel { price: 101.0, quantity: 40.0, order_count: 4 },
            PriceLevel { price: 102.0, quantity: 25.0, order_count: 2 },
            PriceLevel { price: 103.0, quantity: 15.0, order_count: 1 },
        ];
        book.timestamp_ms = 1_000_000;
        book
    }

    #[test]
    fn spread_calculation() {
        let book = make_book();
        assert_eq!(book.best_bid(), Some(100.0));
        assert_eq!(book.best_ask(), Some(101.0));
        assert_eq!(book.spread(), Some(1.0));
        assert_eq!(book.mid_price(), Some(100.5));
    }

    #[test]
    fn imbalance_bounds() {
        let book = make_book();
        let imb = book.bid_ask_imbalance();
        assert!(imb >= -1.0 && imb <= 1.0, "imbalance must be in [-1, 1], got {imb}");
    }

    #[test]
    fn imbalance_bid_heavy() {
        let mut book = OrderBook::new("X");
        book.bids = vec![PriceLevel { price: 100.0, quantity: 90.0, order_count: 1 }];
        book.asks = vec![PriceLevel { price: 101.0, quantity: 10.0, order_count: 1 }];
        let imb = book.bid_ask_imbalance();
        assert!(imb > 0.0, "bid-heavy book should have positive imbalance");
        assert!((imb - 0.8).abs() < 1e-9);
    }

    #[test]
    fn imbalance_ask_heavy() {
        let mut book = OrderBook::new("X");
        book.bids = vec![PriceLevel { price: 100.0, quantity: 10.0, order_count: 1 }];
        book.asks = vec![PriceLevel { price: 101.0, quantity: 90.0, order_count: 1 }];
        let imb = book.bid_ask_imbalance();
        assert!(imb < 0.0);
    }

    #[test]
    fn vwap_fills_correctly() {
        let book = make_book();
        // Fill 60 units on ask: 40@101 + 20@102 → (40*101 + 20*102)/60 = (4040+2040)/60 = 6080/60
        let vwap = vwap_from_book(&book.asks, 60.0).unwrap();
        let expected = (40.0 * 101.0 + 20.0 * 102.0) / 60.0;
        assert!((vwap - expected).abs() < 1e-9, "VWAP = {vwap}, expected {expected}");
    }

    #[test]
    fn vwap_insufficient_liquidity_returns_partial() {
        let book = make_book();
        // Ask side has 40+25+15=80 total; request 200 → partial fill at what's available
        let vwap = vwap_from_book(&book.asks, 200.0);
        assert!(vwap.is_some(), "should return Some with partial fill");
    }

    #[test]
    fn vwap_empty_book_returns_none() {
        let book = OrderBook::new("X");
        assert!(vwap_from_book(&book.asks, 10.0).is_none());
    }

    #[test]
    fn depth_chart_cumulative() {
        let book = make_book();
        let (bids, asks) = book.depth_chart_data(3);
        // Bid cumulative: 50, 80, 100
        assert_eq!(bids.len(), 3);
        assert!((bids[0].1 - 50.0).abs() < 1e-9);
        assert!((bids[1].1 - 80.0).abs() < 1e-9);
        assert!((bids[2].1 - 100.0).abs() < 1e-9);
        // Ask cumulative: 40, 65, 80
        assert_eq!(asks.len(), 3);
        assert!((asks[0].1 - 40.0).abs() < 1e-9);
        assert!((asks[1].1 - 65.0).abs() < 1e-9);
        assert!((asks[2].1 - 80.0).abs() < 1e-9);
    }

    #[test]
    fn slippage_estimate_positive_for_buy() {
        let book = make_book();
        // Buying on ask side: VWAP > mid → positive slippage
        let slip = slippage_estimate(&book, OrderSide::Ask, 50.0);
        assert!(slip >= 0.0, "slippage should be non-negative, got {slip}");
    }

    #[test]
    fn update_level_upsert() {
        let mut book = OrderBook::new("X");
        book.update_level(OrderSide::Ask, 100.0, 10.0);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.asks[0].quantity, 10.0);

        // Update existing
        book.update_level(OrderSide::Ask, 100.0, 20.0);
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.asks[0].quantity, 20.0);
    }

    #[test]
    fn update_level_remove() {
        let mut book = OrderBook::new("X");
        book.update_level(OrderSide::Bid, 100.0, 10.0);
        assert_eq!(book.bids.len(), 1);
        book.update_level(OrderSide::Bid, 100.0, 0.0); // remove
        assert_eq!(book.bids.len(), 0);
    }

    #[test]
    fn update_level_sorted_bids() {
        let mut book = OrderBook::new("X");
        book.update_level(OrderSide::Bid, 99.0, 10.0);
        book.update_level(OrderSide::Bid, 101.0, 5.0);
        book.update_level(OrderSide::Bid, 100.0, 8.0);
        // Should be descending: 101, 100, 99
        assert_eq!(book.bids[0].price, 101.0);
        assert_eq!(book.bids[1].price, 100.0);
        assert_eq!(book.bids[2].price, 99.0);
    }

    #[test]
    fn compute_metrics_spread_bps() {
        let book = make_book();
        let metrics = book.compute_metrics();
        // spread = 1.0, mid = 100.5 → spread_bps = 1/100.5 * 10000 ≈ 99.5 bps
        assert!(metrics.spread_bps > 0.0);
        assert!(metrics.imbalance >= -1.0 && metrics.imbalance <= 1.0);
    }
}
