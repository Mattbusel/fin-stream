//! Real-time position and P&L tracking.
//!
//! ## Architecture
//!
//! ```text
//! Trade events
//!      │
//!      ▼
//! PositionTracker  (DashMap<symbol, Position>)
//!      ├──► apply_trade      — open / average-up / reduce / close
//!      ├──► update_prices    — mark-to-market all positions
//!      ├──► portfolio_value  — sum of market values
//!      ├──► total_unrealized_pnl
//!      ├──► positions_by_pnl — sorted desc
//!      └──► risk_metrics     — exposure, HHI, largest position %
//! ```
//!
//! ## Guarantees
//! - Zero panics; all divisions guarded.
//! - DashMap for lock-free concurrent access.
//! - Partial close reduces quantity and accumulates realized P&L correctly.

use dashmap::DashMap;
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
//  Side
// ─────────────────────────────────────────────────────────────────────────────

/// Direction of a position or trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Long: profit from rising prices.
    Long,
    /// Short: profit from falling prices.
    Short,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Position
// ─────────────────────────────────────────────────────────────────────────────

/// An open position in a single instrument.
#[derive(Debug, Clone)]
pub struct Position {
    /// Instrument symbol.
    pub symbol: String,
    /// Direction (Long or Short).
    pub side: Side,
    /// Current open quantity (always positive).
    pub quantity: f64,
    /// Volume-weighted average entry price.
    pub avg_entry_price: f64,
    /// Latest mark-to-market price.
    pub current_price: f64,
    /// Cumulative realised P&L for this position (including partial closes).
    pub realized_pnl: f64,
    /// Unix milliseconds when the position was first opened.
    pub open_time_ms: u64,
}

impl Position {
    /// Unrealised P&L at the current market price.
    pub fn unrealized_pnl(&self) -> f64 {
        match self.side {
            Side::Long => (self.current_price - self.avg_entry_price) * self.quantity,
            Side::Short => (self.avg_entry_price - self.current_price) * self.quantity,
        }
    }

    /// Total P&L = realized + unrealized.
    pub fn total_pnl(&self) -> f64 {
        self.realized_pnl + self.unrealized_pnl()
    }

    /// Return on invested capital: `unrealized_pnl / (entry_price * quantity)`.
    /// Returns 0 if entry value is zero.
    pub fn return_pct(&self) -> f64 {
        let invested = self.avg_entry_price * self.quantity;
        if invested.abs() < f64::EPSILON {
            return 0.0;
        }
        self.unrealized_pnl() / invested
    }

    /// Market value of the position: `current_price * quantity`.
    pub fn market_value(&self) -> f64 {
        self.current_price * self.quantity
    }

    /// Update the mark-to-market price.
    pub fn update_price(&mut self, price: f64) {
        self.current_price = price;
    }

    /// Close `qty` shares at `exit_price`, accumulate realized P&L, reduce quantity.
    ///
    /// Returns the realized P&L for the closed portion.
    /// Clamps `qty` to the available quantity.
    pub fn close_partial(&mut self, qty: f64, exit_price: f64) -> f64 {
        let close_qty = qty.min(self.quantity).max(0.0);
        let pnl = match self.side {
            Side::Long => (exit_price - self.avg_entry_price) * close_qty,
            Side::Short => (self.avg_entry_price - exit_price) * close_qty,
        };
        self.realized_pnl += pnl;
        self.quantity -= close_qty;
        pnl
    }

    /// Close the entire position at `exit_price`.
    ///
    /// Returns the total realized P&L for the closed portion.
    pub fn close_full(&mut self, exit_price: f64) -> f64 {
        self.close_partial(self.quantity, exit_price)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Trade
// ─────────────────────────────────────────────────────────────────────────────

/// A single executed trade.
#[derive(Debug, Clone)]
pub struct Trade {
    /// Instrument symbol.
    pub symbol: String,
    /// Direction of this trade leg.
    pub side: Side,
    /// Quantity traded (positive).
    pub qty: f64,
    /// Execution price.
    pub price: f64,
    /// Unix milliseconds of execution.
    pub timestamp_ms: u64,
    /// Fee paid for this trade.
    pub fee: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  PositionRiskMetrics
// ─────────────────────────────────────────────────────────────────────────────

/// Aggregate risk snapshot across all open positions.
#[derive(Debug, Clone)]
pub struct PositionRiskMetrics {
    /// Sum of all long and short market values.
    pub gross_exposure: f64,
    /// Long exposure minus short exposure.
    pub net_exposure: f64,
    /// Sum of long market values.
    pub long_exposure: f64,
    /// Sum of short market values.
    pub short_exposure: f64,
    /// Fraction of gross exposure in the single largest position.
    pub largest_position_pct: f64,
    /// Herfindahl-Hirschman Index across position market values.
    pub concentration_hhi: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  PositionTracker
// ─────────────────────────────────────────────────────────────────────────────

/// Concurrent real-time position and P&L tracker.
pub struct PositionTracker {
    positions: DashMap<String, Position>,
}

impl PositionTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            positions: DashMap::new(),
        }
    }

    /// Apply a trade: open, average-up, or reduce/close an existing position.
    ///
    /// - **Same side as existing position** → average up (VWAP avg entry price).
    /// - **Opposite side** → reduce or close. If `trade.qty > existing.qty`,
    ///   closes the position and opens a new one in the trade direction for the remainder.
    pub fn apply_trade(&self, trade: Trade) {
        use dashmap::mapref::entry::Entry;
        match self.positions.entry(trade.symbol.clone()) {
            Entry::Vacant(e) => {
                // No existing position — open new
                e.insert(Position {
                    symbol: trade.symbol.clone(),
                    side: trade.side,
                    quantity: trade.qty,
                    avg_entry_price: trade.price,
                    current_price: trade.price,
                    realized_pnl: -trade.fee,
                    open_time_ms: trade.timestamp_ms,
                });
            }
            Entry::Occupied(mut e) => {
                let pos = e.get_mut();
                if pos.side == trade.side {
                    // Average up
                    let total_qty = pos.quantity + trade.qty;
                    if total_qty.abs() > f64::EPSILON {
                        pos.avg_entry_price = (pos.avg_entry_price * pos.quantity
                            + trade.price * trade.qty)
                            / total_qty;
                    }
                    pos.quantity = total_qty;
                    pos.current_price = trade.price;
                    pos.realized_pnl -= trade.fee;
                } else {
                    // Opposite side: reduce or close
                    let close_qty = trade.qty.min(pos.quantity);
                    pos.close_partial(close_qty, trade.price);
                    pos.realized_pnl -= trade.fee;
                    pos.current_price = trade.price;

                    let remaining = trade.qty - close_qty;
                    if pos.quantity < f64::EPSILON {
                        if remaining > f64::EPSILON {
                            // Flip the position
                            *pos = Position {
                                symbol: trade.symbol.clone(),
                                side: trade.side,
                                quantity: remaining,
                                avg_entry_price: trade.price,
                                current_price: trade.price,
                                realized_pnl: pos.realized_pnl,
                                open_time_ms: trade.timestamp_ms,
                            };
                        } else {
                            // Position fully closed: keep entry for realized PnL record
                            // but quantity = 0 already via close_partial
                        }
                    }
                }
            }
        }
    }

    /// Update mark-to-market prices for all positions.
    pub fn update_prices(&self, prices: &HashMap<String, f64>) {
        for mut entry in self.positions.iter_mut() {
            if let Some(&price) = prices.get(entry.key()) {
                entry.update_price(price);
            }
        }
    }

    /// Sum of all position market values.
    pub fn portfolio_value(&self) -> f64 {
        self.positions.iter().map(|p| p.market_value()).sum()
    }

    /// Sum of all unrealised P&L.
    pub fn total_unrealized_pnl(&self) -> f64 {
        self.positions.iter().map(|p| p.unrealized_pnl()).sum()
    }

    /// All positions sorted descending by unrealised P&L.
    pub fn positions_by_pnl(&self) -> Vec<Position> {
        let mut positions: Vec<Position> = self.positions.iter().map(|p| p.clone()).collect();
        positions.sort_by(|a, b| {
            b.unrealized_pnl()
                .partial_cmp(&a.unrealized_pnl())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        positions
    }

    /// Compute aggregate risk metrics.
    pub fn risk_metrics(&self) -> PositionRiskMetrics {
        let mut long_exp = 0.0_f64;
        let mut short_exp = 0.0_f64;
        let mut market_values: Vec<f64> = Vec::new();

        for p in self.positions.iter() {
            let mv = p.market_value().abs();
            match p.side {
                Side::Long => long_exp += mv,
                Side::Short => short_exp += mv,
            }
            market_values.push(mv);
        }

        let gross = long_exp + short_exp;
        let net = long_exp - short_exp;

        let largest_pct = if gross < f64::EPSILON {
            0.0
        } else {
            market_values
                .iter()
                .cloned()
                .fold(0.0_f64, f64::max)
                / gross
        };

        let hhi = if gross < f64::EPSILON {
            0.0
        } else {
            market_values
                .iter()
                .map(|mv| {
                    let w = mv / gross;
                    w * w
                })
                .sum()
        };

        PositionRiskMetrics {
            gross_exposure: gross,
            net_exposure: net,
            long_exposure: long_exp,
            short_exposure: short_exp,
            largest_position_pct: largest_pct,
            concentration_hhi: hhi,
        }
    }
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn buy(symbol: &str, qty: f64, price: f64) -> Trade {
        Trade {
            symbol: symbol.to_string(),
            side: Side::Long,
            qty,
            price,
            timestamp_ms: 0,
            fee: 0.0,
        }
    }

    fn sell(symbol: &str, qty: f64, price: f64) -> Trade {
        Trade {
            symbol: symbol.to_string(),
            side: Side::Short,
            qty,
            price,
            timestamp_ms: 0,
            fee: 0.0,
        }
    }

    #[test]
    fn long_position_unrealized_pnl() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("BTC", 2.0, 50_000.0));
        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), 55_000.0);
        tracker.update_prices(&prices);

        let pos = tracker.positions.get("BTC").unwrap();
        assert!((pos.unrealized_pnl() - 10_000.0).abs() < 1e-6,
            "unrealized_pnl = {}", pos.unrealized_pnl());
    }

    #[test]
    fn average_up_increases_position() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("ETH", 1.0, 3_000.0));
        tracker.apply_trade(buy("ETH", 1.0, 3_200.0));

        let pos = tracker.positions.get("ETH").unwrap();
        assert!((pos.quantity - 2.0).abs() < 1e-9);
        // avg entry = (3000 + 3200) / 2 = 3100
        assert!((pos.avg_entry_price - 3_100.0).abs() < 1e-6,
            "avg_entry = {}", pos.avg_entry_price);
    }

    #[test]
    fn partial_close_reduces_quantity() {
        let mut pos = Position {
            symbol: "X".to_string(),
            side: Side::Long,
            quantity: 10.0,
            avg_entry_price: 100.0,
            current_price: 110.0,
            realized_pnl: 0.0,
            open_time_ms: 0,
        };
        let realized = pos.close_partial(4.0, 120.0);
        assert!((pos.quantity - 6.0).abs() < 1e-9);
        // realized PnL = (120 - 100) * 4 = 80
        assert!((realized - 80.0).abs() < 1e-9, "realized = {realized}");
        assert!((pos.realized_pnl - 80.0).abs() < 1e-9);
    }

    #[test]
    fn opposite_trade_reduces_position() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("SOL", 10.0, 100.0));
        tracker.apply_trade(sell("SOL", 6.0, 110.0));

        let pos = tracker.positions.get("SOL").unwrap();
        assert!((pos.quantity - 4.0).abs() < 1e-9, "qty = {}", pos.quantity);
        // realized PnL = (110 - 100) * 6 = 60
        assert!((pos.realized_pnl - 60.0).abs() < 1e-6, "realized = {}", pos.realized_pnl);
    }

    #[test]
    fn opposite_trade_closes_position() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("ADA", 5.0, 1.0));
        tracker.apply_trade(sell("ADA", 5.0, 1.5));

        let pos = tracker.positions.get("ADA").unwrap();
        // All closed: quantity = 0
        assert!(pos.quantity.abs() < 1e-9, "qty after full close = {}", pos.quantity);
        // realized = (1.5 - 1.0) * 5 = 2.5
        assert!((pos.realized_pnl - 2.5).abs() < 1e-9);
    }

    #[test]
    fn flip_creates_new_short_position() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("BNB", 3.0, 300.0));
        // Sell 5 → close 3 long, open 2 short
        tracker.apply_trade(sell("BNB", 5.0, 320.0));

        let pos = tracker.positions.get("BNB").unwrap();
        assert_eq!(pos.side, Side::Short);
        assert!((pos.quantity - 2.0).abs() < 1e-9, "short qty = {}", pos.quantity);
    }

    #[test]
    fn portfolio_value_sums_market_values() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("A", 10.0, 50.0));
        tracker.apply_trade(buy("B", 5.0, 200.0));
        // Both at entry price: 10*50 + 5*200 = 500 + 1000 = 1500
        assert!((tracker.portfolio_value() - 1_500.0).abs() < 1e-6);
    }

    #[test]
    fn positions_by_pnl_sorted() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("A", 1.0, 100.0));
        tracker.apply_trade(buy("B", 1.0, 100.0));
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 150.0); // +50
        prices.insert("B".to_string(), 80.0);  // -20
        tracker.update_prices(&prices);

        let sorted = tracker.positions_by_pnl();
        assert_eq!(sorted[0].symbol, "A");
        assert_eq!(sorted[1].symbol, "B");
    }

    #[test]
    fn risk_metrics_gross_net_exposure() {
        let tracker = PositionTracker::new();
        tracker.apply_trade(buy("X", 10.0, 100.0));  // long 1000
        tracker.apply_trade(sell("Y", 5.0, 200.0));  // short 1000
        let metrics = tracker.risk_metrics();
        assert!((metrics.gross_exposure - 2_000.0).abs() < 1e-6);
        assert!(metrics.net_exposure.abs() < 1e-6, "net = {}", metrics.net_exposure);
    }

    #[test]
    fn short_position_unrealized_pnl() {
        let pos = Position {
            symbol: "Z".to_string(),
            side: Side::Short,
            quantity: 5.0,
            avg_entry_price: 200.0,
            current_price: 180.0,
            realized_pnl: 0.0,
            open_time_ms: 0,
        };
        // profit when price falls: (200-180)*5 = 100
        assert!((pos.unrealized_pnl() - 100.0).abs() < 1e-9);
    }
}
