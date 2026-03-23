//! Market making quote engine.
//!
//! Provides inventory-aware bid/ask spread computation, fill processing,
//! mark-to-market accounting, and aggregate statistics.

use std::collections::HashMap;

// ─── Core types ───────────────────────────────────────────────────────────────

/// A two-sided market quote.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Spread {
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Mid-market price.
    pub mid: f64,
    /// Half the bid-ask spread.
    pub half_spread: f64,
}

/// Current inventory for one symbol.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InventoryPosition {
    /// Symbol / asset identifier.
    pub symbol: String,
    /// Net quantity held (positive = long, negative = short).
    pub quantity: f64,
    /// Volume-weighted average cost per unit.
    pub avg_cost: f64,
    /// Current mark-to-market value (quantity × current_price).
    pub market_value: f64,
    /// Unrealized P&L = market_value − quantity × avg_cost.
    pub unrealized_pnl: f64,
}

/// Parameters governing quote generation.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct QuoteParams {
    /// Target bid-ask spread in basis points.
    pub target_spread_bps: f64,
    /// Maximum absolute inventory the maker is willing to hold.
    pub max_inventory: f64,
    /// Fraction of the half-spread used to skew quotes by inventory ratio.
    pub inventory_skew_factor: f64,
    /// Minimum order size.
    pub min_size: f64,
    /// Maximum order size.
    pub max_size: f64,
}

// ─── MarketMaker ──────────────────────────────────────────────────────────────

/// An inventory-aware market making engine.
#[derive(Debug, Clone)]
pub struct MarketMaker {
    /// Configuration.
    pub params: QuoteParams,
    /// Per-symbol inventory.
    pub inventory: HashMap<String, InventoryPosition>,
    /// Cumulative realized P&L.
    pub realized_pnl: f64,
    /// Total number of fills processed.
    pub total_trades: u64,
}

impl MarketMaker {
    /// Create a new market maker with the given parameters and empty inventory.
    pub fn new(params: QuoteParams) -> Self {
        Self {
            params,
            inventory: HashMap::new(),
            realized_pnl: 0.0,
            total_trades: 0,
        }
    }

    /// Compute an inventory-skewed spread for a given mid-price and inventory quantity.
    ///
    /// - `base_half` = mid × target_spread_bps / 20 000
    /// - `skew`      = (inventory / max_inventory) × inventory_skew_factor × base_half
    /// - `bid`       = mid − base_half − skew
    /// - `ask`       = mid + base_half − skew
    pub fn compute_spread(mid: f64, params: &QuoteParams, inventory_qty: f64) -> Spread {
        let base_half = mid * params.target_spread_bps / 20_000.0;
        let skew = if params.max_inventory == 0.0 {
            0.0
        } else {
            inventory_qty / params.max_inventory * params.inventory_skew_factor * base_half
        };
        let bid = mid - base_half - skew;
        let ask = mid + base_half - skew;
        Spread { bid, ask, mid, half_spread: base_half }
    }

    /// Generate a quote for `symbol` at the given mid-price.
    pub fn quote(&self, symbol: &str, mid: f64) -> Spread {
        let qty = self.inventory.get(symbol).map(|p| p.quantity).unwrap_or(0.0);
        Self::compute_spread(mid, &self.params, qty)
    }

    /// Process a bid fill: we bought `qty` at `price` (inventory increases).
    pub fn fill_bid(&mut self, symbol: &str, price: f64, qty: f64) {
        self.total_trades += 1;
        let pos = self.inventory.entry(symbol.to_string()).or_insert_with(|| InventoryPosition {
            symbol: symbol.to_string(),
            quantity: 0.0,
            avg_cost: 0.0,
            market_value: 0.0,
            unrealized_pnl: 0.0,
        });
        // Weighted average cost for long side
        let new_qty = pos.quantity + qty;
        if new_qty.abs() < f64::EPSILON {
            pos.avg_cost = 0.0;
        } else if pos.quantity >= 0.0 {
            // Adding to existing long or opening from zero
            pos.avg_cost = (pos.avg_cost * pos.quantity + price * qty) / new_qty;
        } else {
            // Covering a short: realize P&L on the closed portion
            let closed = qty.min(-pos.quantity);
            if closed > 0.0 {
                self.realized_pnl += closed * (pos.avg_cost - price);
            }
            let remaining = qty - closed;
            if remaining > 0.0 {
                pos.avg_cost = price; // flip to long
            }
        }
        pos.quantity = new_qty;
        pos.market_value = pos.quantity * price;
        pos.unrealized_pnl = pos.market_value - pos.quantity * pos.avg_cost;
    }

    /// Process an ask fill: we sold `qty` at `price` (inventory decreases).
    pub fn fill_ask(&mut self, symbol: &str, price: f64, qty: f64) {
        self.total_trades += 1;
        let pos = self.inventory.entry(symbol.to_string()).or_insert_with(|| InventoryPosition {
            symbol: symbol.to_string(),
            quantity: 0.0,
            avg_cost: 0.0,
            market_value: 0.0,
            unrealized_pnl: 0.0,
        });
        let new_qty = pos.quantity - qty;
        if new_qty.abs() < f64::EPSILON {
            pos.avg_cost = 0.0;
        } else if pos.quantity <= 0.0 {
            // Adding to existing short or opening from zero
            let total_short = pos.quantity.abs() + qty;
            pos.avg_cost =
                (pos.avg_cost * pos.quantity.abs() + price * qty) / total_short;
        } else {
            // Reducing a long: realize P&L on the closed portion
            let closed = qty.min(pos.quantity);
            if closed > 0.0 {
                self.realized_pnl += closed * (price - pos.avg_cost);
            }
            let remaining = qty - closed;
            if remaining > 0.0 {
                pos.avg_cost = price; // flip to short
            }
        }
        pos.quantity = new_qty;
        pos.market_value = pos.quantity * price;
        pos.unrealized_pnl = pos.market_value - pos.quantity * pos.avg_cost;
    }

    /// Update market value and unrealized P&L for `symbol` at `current_price`.
    pub fn mark_to_market(&mut self, symbol: &str, current_price: f64) {
        if let Some(pos) = self.inventory.get_mut(symbol) {
            pos.market_value = pos.quantity * current_price;
            pos.unrealized_pnl = pos.market_value - pos.quantity * pos.avg_cost;
        }
    }

    /// Total P&L = realized + sum of all unrealized P&Ls.
    pub fn total_pnl(&self) -> f64 {
        let unrealized: f64 = self.inventory.values().map(|p| p.unrealized_pnl).sum();
        self.realized_pnl + unrealized
    }

    /// Inventory risk = Σ |quantity × avg_cost| across all positions.
    pub fn inventory_risk(&self) -> f64 {
        self.inventory.values().map(|p| (p.quantity * p.avg_cost).abs()).sum()
    }

    /// Aggregate statistics snapshot.
    pub fn stats(&self, current_mid: &HashMap<String, f64>) -> MarketMakerStats {
        let unrealized_pnl: f64 = self.inventory.values().map(|p| p.unrealized_pnl).sum();
        let inventory_risk = self.inventory_risk();

        // Average spread in bps across quoted symbols
        let avg_spread_bps = if current_mid.is_empty() {
            0.0
        } else {
            let total_bps: f64 = current_mid
                .iter()
                .map(|(sym, &mid)| {
                    let qty = self.inventory.get(sym).map(|p| p.quantity).unwrap_or(0.0);
                    let s = Self::compute_spread(mid, &self.params, qty);
                    // Full spread in bps = (ask - bid) / mid * 10_000
                    if mid == 0.0 { 0.0 } else { (s.ask - s.bid) / mid * 10_000.0 }
                })
                .sum();
            total_bps / current_mid.len() as f64
        };

        MarketMakerStats {
            total_trades: self.total_trades,
            realized_pnl: self.realized_pnl,
            unrealized_pnl,
            inventory_risk,
            avg_spread_bps,
        }
    }
}

/// Aggregate statistics for a `MarketMaker` instance.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct MarketMakerStats {
    /// Total fills processed.
    pub total_trades: u64,
    /// Cumulative realized P&L.
    pub realized_pnl: f64,
    /// Sum of unrealized P&Ls across all positions.
    pub unrealized_pnl: f64,
    /// Total inventory risk (Σ |qty × avg_cost|).
    pub inventory_risk: f64,
    /// Average quoted spread in basis points.
    pub avg_spread_bps: f64,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_params() -> QuoteParams {
        QuoteParams {
            target_spread_bps: 10.0,
            max_inventory: 100.0,
            inventory_skew_factor: 1.0,
            min_size: 1.0,
            max_size: 100.0,
        }
    }

    #[test]
    fn spread_symmetric_at_zero_inventory() {
        let params = default_params();
        let s = MarketMaker::compute_spread(100.0, &params, 0.0);
        // half_spread = 100 * 10 / 20000 = 0.05
        assert!((s.half_spread - 0.05).abs() < 1e-10);
        assert!((s.bid - 99.95).abs() < 1e-10);
        assert!((s.ask - 100.05).abs() < 1e-10);
        assert!((s.mid - 100.0).abs() < 1e-10);
    }

    #[test]
    fn spread_widens_bid_with_long_inventory() {
        let params = default_params();
        // Long 50 units with max_inventory=100 => skew = 0.5 * 1.0 * 0.05 = 0.025
        let s = MarketMaker::compute_spread(100.0, &params, 50.0);
        // bid = 100 - 0.05 - 0.025 = 99.925
        // ask = 100 + 0.05 - 0.025 = 100.025
        assert!((s.bid - 99.925).abs() < 1e-10, "bid={}", s.bid);
        assert!((s.ask - 100.025).abs() < 1e-10, "ask={}", s.ask);
    }

    #[test]
    fn bid_ask_asymmetry_with_skew() {
        let params = default_params();
        let s_long = MarketMaker::compute_spread(100.0, &params, 50.0);
        let s_short = MarketMaker::compute_spread(100.0, &params, -50.0);
        // Long inventory pushes both quotes down; short inventory pushes both up
        assert!(s_long.bid < s_short.bid, "long inventory => lower bid");
        assert!(s_long.ask < s_short.ask, "long inventory => lower ask");
    }

    #[test]
    fn pnl_on_round_trip() {
        let mut mm = MarketMaker::new(default_params());
        // Buy at 99.95 (our bid), sell at 100.05 (our ask)
        mm.fill_bid("AAPL", 99.95, 10.0);
        mm.fill_ask("AAPL", 100.05, 10.0);
        // Realized PnL = 10 * (100.05 - 99.95) = 1.0
        assert!((mm.realized_pnl - 1.0).abs() < 1e-8, "realized={}", mm.realized_pnl);
        // After selling all, unrealized should be ~0
        let pos = &mm.inventory["AAPL"];
        assert!(pos.quantity.abs() < 1e-10, "qty={}", pos.quantity);
    }

    #[test]
    fn mark_to_market_updates_unrealized() {
        let mut mm = MarketMaker::new(default_params());
        mm.fill_bid("BTC", 50_000.0, 1.0);
        mm.mark_to_market("BTC", 51_000.0);
        let pos = &mm.inventory["BTC"];
        assert!((pos.market_value - 51_000.0).abs() < 1e-6);
        assert!((pos.unrealized_pnl - 1_000.0).abs() < 1e-6);
        assert!((mm.total_pnl() - 1_000.0).abs() < 1e-6);
    }

    #[test]
    fn stats_returns_correct_avg_spread() {
        let mm = MarketMaker::new(default_params());
        let mut mids = HashMap::new();
        mids.insert("AAPL".to_string(), 100.0);
        let stats = mm.stats(&mids);
        // At 0 inventory: spread = 10 bps
        assert!((stats.avg_spread_bps - 10.0).abs() < 1e-8, "bps={}", stats.avg_spread_bps);
    }
}
