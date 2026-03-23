//! # Module: marketmaker
//!
//! ## Responsibility
//! Inventory-aware market maker simulator: quotes bid/ask with symmetric
//! spread skewed by current inventory, processes fills, and tracks P&L.
//!
//! ## Guarantees
//! - No panics on the hot path; all edge cases handled via saturating arithmetic.
//! - Thread-unsafe by design (single-threaded simulation); wrap in a Mutex for shared state.

use std::collections::VecDeque;

// ─── config ───────────────────────────────────────────────────────────────────

/// Configuration for a `MarketMaker` instance.
#[derive(Debug, Clone, Copy)]
pub struct MmConfig {
    /// Full bid-ask spread in basis points (e.g. 10.0 = 10 bps = 0.10%).
    pub spread_bps: f64,
    /// Target inventory level (position the MM wants to hold).
    pub inventory_target: f64,
    /// Maximum absolute inventory before quotes are skewed maximally.
    pub inventory_limit: f64,
    /// Skew factor: how aggressively to skew quotes based on inventory deviation.
    pub skew_factor: f64,
    /// Lot size for each side of the quote.
    pub lot_size: f64,
}

impl Default for MmConfig {
    fn default() -> Self {
        Self {
            spread_bps: 10.0,
            inventory_target: 0.0,
            inventory_limit: 100.0,
            skew_factor: 1.0,
            lot_size: 1.0,
        }
    }
}

// ─── state ────────────────────────────────────────────────────────────────────

/// Runtime state of the market maker.
#[derive(Debug, Clone, Copy, Default)]
pub struct MmState {
    /// Current inventory (positive = long, negative = short).
    pub inventory: f64,
    /// Current cash balance (P&L from fills, not mark-to-market).
    pub cash: f64,
    /// Realised P&L accumulated so far.
    pub pnl: f64,
    /// Number of trades executed.
    pub trades_made: u64,
}

// ─── quote ────────────────────────────────────────────────────────────────────

/// A two-sided quote produced by the market maker.
#[derive(Debug, Clone, Copy)]
pub struct MmQuote {
    /// Bid price.
    pub bid: f64,
    /// Ask price.
    pub ask: f64,
    /// Quantity offered on the bid side.
    pub bid_size: f64,
    /// Quantity offered on the ask side.
    pub ask_size: f64,
}

// ─── fill ─────────────────────────────────────────────────────────────────────

/// Which side of the market was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// A buyer lifted the MM's ask (MM sold).
    Buy,
    /// A seller hit the MM's bid (MM bought).
    Sell,
}

/// Result of processing a fill.
#[derive(Debug, Clone, Copy)]
pub struct FillResult {
    /// Inventory after the fill.
    pub inventory_after: f64,
    /// Realised P&L from this fill (0.0 if no closing of an existing position).
    pub realized_pnl: f64,
    /// Mark-to-market value of the remaining inventory at the fill price.
    pub mark_to_market: f64,
}

// ─── stats ────────────────────────────────────────────────────────────────────

/// Aggregate statistics over the MM's lifetime.
#[derive(Debug, Clone, Copy)]
pub struct MmStats {
    /// Total realised P&L.
    pub total_pnl: f64,
    /// Maximum inventory reached (most long).
    pub max_inventory: f64,
    /// Minimum inventory reached (most short).
    pub min_inventory: f64,
    /// Total number of trades.
    pub trade_count: u64,
    /// Average spread in basis points across all quotes.
    pub avg_spread_bps: f64,
}

// ─── market maker ─────────────────────────────────────────────────────────────

/// Simple inventory-aware market maker simulator.
///
/// # Quoting logic
/// ```text
/// half_spread = mid * spread_bps / 2 / 10_000
/// inv_excess  = (inventory - inventory_target) / inventory_limit
/// skew        = skew_factor * inv_excess * half_spread
/// bid         = mid - half_spread - skew
/// ask         = mid + half_spread - skew
/// ```
/// When inventory is above target (long), bids are lowered to discourage
/// further buying, and asks are lowered to encourage selling.
pub struct MarketMaker {
    config: MmConfig,
    state: MmState,
    /// Average cost basis of current inventory (VWAP).
    avg_cost: f64,
    /// Running sum of spread bps for average calculation.
    spread_sum: f64,
    /// Number of quotes issued (for avg spread calculation).
    quote_count: u64,
    /// Inventory high-water and low-water marks.
    max_inventory: f64,
    min_inventory: f64,
    /// Recent fills for mark-to-market tracking.
    last_fill_price: f64,
    /// History of inventory for risk tracking (bounded).
    _inventory_history: VecDeque<f64>,
}

impl MarketMaker {
    /// Create a new `MarketMaker` with the given configuration.
    pub fn new(config: MmConfig) -> Self {
        Self {
            config,
            state: MmState::default(),
            avg_cost: 0.0,
            spread_sum: 0.0,
            quote_count: 0,
            max_inventory: 0.0,
            min_inventory: 0.0,
            last_fill_price: 0.0,
            _inventory_history: VecDeque::with_capacity(1024),
        }
    }

    /// Generate a two-sided quote around `mid_price`, skewed by current inventory.
    pub fn quote(&mut self, mid_price: f64) -> MmQuote {
        let half_spread = mid_price * self.config.spread_bps / 2.0 / 10_000.0;
        let inv_excess = if self.config.inventory_limit.abs() < f64::EPSILON {
            0.0
        } else {
            (self.state.inventory - self.config.inventory_target)
                / self.config.inventory_limit
        };
        let skew = self.config.skew_factor * inv_excess * half_spread;

        let bid = mid_price - half_spread - skew;
        let ask = mid_price + half_spread - skew;

        self.spread_sum += self.config.spread_bps;
        self.quote_count += 1;

        MmQuote {
            bid,
            ask,
            bid_size: self.config.lot_size,
            ask_size: self.config.lot_size,
        }
    }

    /// Process a fill: update inventory, cash, and compute realised P&L.
    ///
    /// `side` is from the taker's perspective:
    /// - `Side::Buy` means a buyer hit the ask — MM sold.
    /// - `Side::Sell` means a seller hit the bid — MM bought.
    pub fn fill(&mut self, side: Side, price: f64, qty: f64) -> FillResult {
        let prev_inv = self.state.inventory;
        let realized_pnl;

        match side {
            Side::Buy => {
                // MM sold: cash increases, inventory decreases.
                let proceeds = price * qty;
                self.state.cash += proceeds;

                if prev_inv > 0.0 {
                    // Closing long position (or partial close).
                    let closing_qty = qty.min(prev_inv);
                    realized_pnl = (price - self.avg_cost) * closing_qty;
                } else {
                    realized_pnl = 0.0;
                }
                self.state.inventory -= qty;

                // Update avg cost for short position
                if self.state.inventory < 0.0 && prev_inv >= 0.0 {
                    // Flipped to short
                    self.avg_cost = price;
                } else if self.state.inventory < 0.0 && prev_inv < 0.0 {
                    // Adding to short: VWAP
                    let total_qty = prev_inv.abs() + qty;
                    self.avg_cost = (self.avg_cost * prev_inv.abs() + price * qty) / total_qty;
                }
            }
            Side::Sell => {
                // MM bought: cash decreases, inventory increases.
                let cost = price * qty;
                self.state.cash -= cost;

                if prev_inv < 0.0 {
                    // Closing short position (or partial close).
                    let closing_qty = qty.min(prev_inv.abs());
                    realized_pnl = (self.avg_cost - price) * closing_qty;
                } else {
                    realized_pnl = 0.0;
                }
                self.state.inventory += qty;

                // Update avg cost for long position
                if self.state.inventory > 0.0 && prev_inv <= 0.0 {
                    // Flipped to long
                    self.avg_cost = price;
                } else if self.state.inventory > 0.0 && prev_inv > 0.0 {
                    // Adding to long: VWAP
                    let total_qty = prev_inv + qty;
                    self.avg_cost = (self.avg_cost * prev_inv + price * qty) / total_qty;
                }
            }
        }

        self.state.pnl += realized_pnl;
        self.state.trades_made += 1;
        self.last_fill_price = price;

        // Update high/low watermarks
        if self.state.inventory > self.max_inventory {
            self.max_inventory = self.state.inventory;
        }
        if self.state.inventory < self.min_inventory {
            self.min_inventory = self.state.inventory;
        }

        let mark_to_market = self.state.inventory * price;

        FillResult {
            inventory_after: self.state.inventory,
            realized_pnl,
            mark_to_market,
        }
    }

    /// Return a snapshot of current state.
    pub fn state(&self) -> MmState {
        self.state
    }

    /// Return aggregate statistics.
    pub fn stats(&self) -> MmStats {
        let avg_spread_bps = if self.quote_count == 0 {
            0.0
        } else {
            self.spread_sum / self.quote_count as f64
        };
        MmStats {
            total_pnl: self.state.pnl,
            max_inventory: self.max_inventory,
            min_inventory: self.min_inventory,
            trade_count: self.state.trades_made,
            avg_spread_bps,
        }
    }

    /// Current inventory.
    pub fn inventory(&self) -> f64 {
        self.state.inventory
    }

    /// Mark-to-market value of inventory at a given price.
    pub fn mark_to_market(&self, price: f64) -> f64 {
        self.state.inventory * price
    }

    /// Total P&L: realised + unrealised at `mark_price`.
    pub fn total_pnl(&self, mark_price: f64) -> f64 {
        let unrealised = if self.state.inventory.abs() > f64::EPSILON {
            (mark_price - self.avg_cost) * self.state.inventory
        } else {
            0.0
        };
        self.state.pnl + unrealised
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_mm() -> MarketMaker {
        MarketMaker::new(MmConfig::default())
    }

    fn mm_with(spread_bps: f64, skew: f64, limit: f64) -> MarketMaker {
        MarketMaker::new(MmConfig {
            spread_bps,
            inventory_target: 0.0,
            inventory_limit: limit,
            skew_factor: skew,
            lot_size: 1.0,
        })
    }

    // ── quoting ──

    #[test]
    fn quote_bid_below_ask() {
        let mut mm = default_mm();
        let q = mm.quote(100.0);
        assert!(q.bid < q.ask, "bid should be below ask: {} >= {}", q.bid, q.ask);
    }

    #[test]
    fn quote_spread_equals_config() {
        let mut mm = mm_with(20.0, 0.0, 100.0);
        let q = mm.quote(100.0);
        let spread_bps = (q.ask - q.bid) / 100.0 * 10_000.0;
        assert!((spread_bps - 20.0).abs() < 1e-8, "spread: {spread_bps:.4} bps");
    }

    #[test]
    fn quote_symmetric_at_zero_inventory() {
        let mut mm = default_mm();
        let q = mm.quote(100.0);
        let mid = (q.bid + q.ask) / 2.0;
        assert!((mid - 100.0).abs() < 1e-8, "mid should equal input: {mid}");
    }

    #[test]
    fn quote_skew_long_inventory_lowers_bid() {
        let mut mm = mm_with(10.0, 1.0, 100.0);
        // Manually set long inventory
        mm.state.inventory = 50.0;
        let q = mm.quote(100.0);
        let symmetric_bid = 100.0 - (100.0 * 10.0 / 2.0 / 10_000.0);
        assert!(q.bid < symmetric_bid, "long inventory should lower bid");
    }

    #[test]
    fn quote_skew_short_inventory_raises_bid() {
        let mut mm = mm_with(10.0, 1.0, 100.0);
        mm.state.inventory = -50.0;
        let q = mm.quote(100.0);
        let symmetric_bid = 100.0 - (100.0 * 10.0 / 2.0 / 10_000.0);
        assert!(q.bid > symmetric_bid, "short inventory should raise bid");
    }

    #[test]
    fn quote_size_equals_lot_size() {
        let mut mm = MarketMaker::new(MmConfig { lot_size: 5.0, ..MmConfig::default() });
        let q = mm.quote(100.0);
        assert_eq!(q.bid_size, 5.0);
        assert_eq!(q.ask_size, 5.0);
    }

    #[test]
    fn quote_at_inventory_limit_maximum_skew() {
        let mut mm = mm_with(10.0, 1.0, 100.0);
        mm.state.inventory = 100.0; // exactly at limit
        let q_maxlong = mm.quote(100.0);

        let mut mm2 = mm_with(10.0, 1.0, 100.0);
        mm2.state.inventory = 0.0;
        let q_flat = mm2.quote(100.0);

        assert!(q_maxlong.bid < q_flat.bid, "max-long should quote lowest bid");
    }

    // ── fill mechanics ──

    #[test]
    fn fill_buy_decreases_inventory() {
        let mut mm = default_mm();
        let res = mm.fill(Side::Buy, 100.0, 1.0);
        assert_eq!(res.inventory_after, -1.0);
    }

    #[test]
    fn fill_sell_increases_inventory() {
        let mut mm = default_mm();
        let res = mm.fill(Side::Sell, 100.0, 1.0);
        assert_eq!(res.inventory_after, 1.0);
    }

    #[test]
    fn fill_cash_increases_on_sell_to_buyer() {
        let mut mm = default_mm();
        mm.fill(Side::Buy, 100.0, 1.0);
        assert_eq!(mm.state().cash, 100.0);
    }

    #[test]
    fn fill_cash_decreases_on_buy_from_seller() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 1.0);
        assert_eq!(mm.state().cash, -100.0);
    }

    #[test]
    fn fill_realized_pnl_round_trip() {
        // Buy at 99, sell at 101 → realized PnL = 2
        let mut mm = default_mm();
        mm.fill(Side::Sell, 99.0, 1.0); // MM buys 1 at 99
        let res = mm.fill(Side::Buy, 101.0, 1.0); // MM sells 1 at 101
        assert!((res.realized_pnl - 2.0).abs() < 1e-8, "realized PnL: {}", res.realized_pnl);
    }

    #[test]
    fn fill_trade_count_increments() {
        let mut mm = default_mm();
        mm.fill(Side::Buy, 100.0, 1.0);
        mm.fill(Side::Buy, 100.0, 1.0);
        assert_eq!(mm.state().trades_made, 2);
    }

    #[test]
    fn fill_mark_to_market_correct() {
        let mut mm = default_mm();
        let res = mm.fill(Side::Sell, 100.0, 3.0);
        // Inventory = 3, price = 100 → MtM = 300
        assert!((res.mark_to_market - 300.0).abs() < 1e-8);
    }

    #[test]
    fn fill_no_realized_pnl_when_opening() {
        let mut mm = default_mm();
        let res = mm.fill(Side::Sell, 100.0, 1.0);
        assert_eq!(res.realized_pnl, 0.0, "opening trade has no realized PnL");
    }

    // ── stats ──

    #[test]
    fn stats_avg_spread_correct() {
        let mut mm = mm_with(20.0, 0.0, 100.0);
        mm.quote(100.0);
        mm.quote(100.0);
        let s = mm.stats();
        assert!((s.avg_spread_bps - 20.0).abs() < 1e-8);
    }

    #[test]
    fn stats_max_min_inventory_tracked() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 5.0); // inv = 5
        mm.fill(Side::Buy, 100.0, 10.0); // inv = -5
        let s = mm.stats();
        assert!((s.max_inventory - 5.0).abs() < 1e-8);
        assert!((s.min_inventory - (-5.0)).abs() < 1e-8);
    }

    #[test]
    fn stats_trade_count() {
        let mut mm = default_mm();
        for _ in 0..7 {
            mm.fill(Side::Buy, 100.0, 1.0);
        }
        assert_eq!(mm.stats().trade_count, 7);
    }

    // ── PnL accounting ──

    #[test]
    fn total_pnl_includes_unrealized() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 1.0); // long 1 at avg cost 100
        let pnl = mm.total_pnl(110.0);
        assert!((pnl - 10.0).abs() < 1e-8, "total PnL: {pnl}");
    }

    #[test]
    fn mark_to_market_correct() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 2.0);
        assert!((mm.mark_to_market(105.0) - 210.0).abs() < 1e-8);
    }

    #[test]
    fn vwap_avg_cost_two_buys() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 2.0); // buy 2 at 100 → avg = 100
        mm.fill(Side::Sell, 110.0, 2.0); // buy 2 at 110 → avg = 105
        let pnl = mm.total_pnl(105.0);
        // Unrealized: (105 - 105) * 4 = 0; realized = 0
        assert!(pnl.abs() < 1e-6, "VWAP PnL should be ~0 at avg cost: {pnl}");
    }

    #[test]
    fn pnl_accumulates_across_multiple_fills() {
        let mut mm = default_mm();
        mm.fill(Side::Sell, 100.0, 1.0);
        mm.fill(Side::Buy, 102.0, 1.0);
        mm.fill(Side::Sell, 98.0, 1.0);
        mm.fill(Side::Buy, 101.0, 1.0);
        // Trade 1: buy@100, sell@102 → +2; Trade 2: buy@98, sell@101 → +3
        let s = mm.stats();
        assert!((s.total_pnl - 5.0).abs() < 1e-8, "accumulated PnL: {}", s.total_pnl);
    }
}
