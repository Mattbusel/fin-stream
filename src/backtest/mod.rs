//! Tick-level backtesting engine.
//!
//! Provides order submission, tick-level order matching (Market, Limit, StopLoss),
//! fill processing, mark-to-market equity tracking, and performance metrics.

use std::collections::HashMap;

// ─────────────────────────────────────────
//  Configuration
// ─────────────────────────────────────────

/// Backtesting engine configuration.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Starting capital in USD (or base currency).
    pub initial_capital: f64,
    /// Commission rate in basis points applied to each fill.
    pub commission_bps: f64,
    /// Slippage in basis points added (buys) or subtracted (sells) from the fill price.
    pub slippage_bps: f64,
    /// Maximum single position size as a fraction of total equity (0.0–1.0).
    pub max_position_pct: f64,
}

// ─────────────────────────────────────────
//  Order types
// ─────────────────────────────────────────

/// Buy or sell direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    /// Buy (long) order.
    Buy,
    /// Sell (short/exit) order.
    Sell,
}

/// Order execution type.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderType {
    /// Fills immediately at the next market price (plus slippage).
    Market,
    /// Fills when the market price crosses the limit price.
    Limit,
    /// Fills when the market price crosses the stop trigger price.
    StopLoss(f64),
}

/// A pending or submitted order.
#[derive(Debug, Clone)]
pub struct Order {
    /// Unique order identifier.
    pub id: u64,
    /// Ticker symbol.
    pub symbol: String,
    /// Buy or sell.
    pub side: OrderSide,
    /// Quantity in units of the asset.
    pub qty: f64,
    /// Execution type.
    pub order_type: OrderType,
    /// Limit price, if applicable.
    pub limit_price: Option<f64>,
    /// Submission timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

// ─────────────────────────────────────────
//  Fill
// ─────────────────────────────────────────

/// A completed order fill.
#[derive(Debug, Clone)]
pub struct Fill {
    /// ID of the order that generated this fill.
    pub order_id: u64,
    /// Ticker symbol.
    pub symbol: String,
    /// Buy or sell side of the fill.
    pub side: OrderSide,
    /// Quantity filled.
    pub qty: f64,
    /// Execution price (after slippage).
    pub fill_price: f64,
    /// Commission charged for this fill.
    pub commission: f64,
    /// Fill timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

// ─────────────────────────────────────────
//  Position
// ─────────────────────────────────────────

/// A current open position in a single symbol.
#[derive(Debug, Clone)]
pub struct Position {
    /// Ticker symbol.
    pub symbol: String,
    /// Quantity held (positive = long).
    pub qty: f64,
    /// Volume-weighted average cost of the current position.
    pub avg_cost: f64,
    /// Cumulative realized P&L for this symbol.
    pub realized_pnl: f64,
}

// ─────────────────────────────────────────
//  BacktestState
// ─────────────────────────────────────────

/// Mutable state of the backtest engine.
#[derive(Debug, Clone)]
pub struct BacktestState {
    /// Available cash.
    pub capital: f64,
    /// Open positions keyed by symbol.
    pub positions: HashMap<String, Position>,
    /// All completed fills.
    pub fills: Vec<Fill>,
    /// All submitted (pending and filled) orders.
    pub orders: Vec<Order>,
    /// Equity curve: `(timestamp_ms, total_equity)`.
    pub equity_curve: Vec<(u64, f64)>,
}

// ─────────────────────────────────────────
//  BacktestEngine
// ─────────────────────────────────────────

/// Tick-level backtesting engine.
pub struct BacktestEngine {
    /// Engine configuration (immutable after construction).
    pub config: BacktestConfig,
    /// Mutable engine state.
    pub state: BacktestState,
    /// Monotonically increasing order ID counter.
    next_order_id: u64,
}

impl BacktestEngine {
    /// Create a new engine with the given configuration.
    #[must_use]
    pub fn new(config: BacktestConfig) -> Self {
        let capital = config.initial_capital;
        Self {
            config,
            state: BacktestState {
                capital,
                positions: HashMap::new(),
                fills: Vec::new(),
                orders: Vec::new(),
                equity_curve: Vec::new(),
            },
            next_order_id: 1,
        }
    }

    /// Submit a new order and return its assigned ID.
    ///
    /// The order is recorded but not yet matched. Call [`process_tick`] to match orders.
    pub fn submit_order(
        &mut self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        order_type: OrderType,
    ) -> u64 {
        let id = self.next_order_id;
        self.next_order_id += 1;
        let limit_price = match &order_type {
            OrderType::Limit => None, // caller embeds price in order_type context; use None here
            _ => None,
        };
        let order = Order {
            id,
            symbol: symbol.to_owned(),
            side,
            qty,
            order_type,
            limit_price,
            timestamp_ms: 0,
        };
        self.state.orders.push(order);
        id
    }

    /// Submit a limit order with an explicit limit price.
    pub fn submit_limit_order(
        &mut self,
        symbol: &str,
        side: OrderSide,
        qty: f64,
        limit_price: f64,
    ) -> u64 {
        let id = self.next_order_id;
        self.next_order_id += 1;
        let order = Order {
            id,
            symbol: symbol.to_owned(),
            side,
            qty,
            order_type: OrderType::Limit,
            limit_price: Some(limit_price),
            timestamp_ms: 0,
        };
        self.state.orders.push(order);
        id
    }

    /// Process a single market tick: match all pending orders for `symbol` at `price`.
    ///
    /// Returns the list of fills generated in this tick.
    ///
    /// Matching rules:
    /// - **Market**: always fills at `price` ± slippage.
    /// - **Limit Buy**: fills if `price <= limit_price`.
    /// - **Limit Sell**: fills if `price >= limit_price`.
    /// - **StopLoss Buy**: fires if `price >= trigger`.
    /// - **StopLoss Sell**: fires if `price <= trigger`.
    pub fn process_tick(
        &mut self,
        symbol: &str,
        price: f64,
        timestamp_ms: u64,
    ) -> Vec<Fill> {
        let slippage_factor = self.config.slippage_bps / 10_000.0;
        let commission_rate = self.config.commission_bps / 10_000.0;

        let mut new_fills: Vec<Fill> = Vec::new();
        let mut remaining_orders: Vec<Order> = Vec::new();

        // Drain orders, matching those for this symbol.
        let orders = std::mem::take(&mut self.state.orders);
        for order in orders {
            if order.symbol != symbol {
                remaining_orders.push(order);
                continue;
            }

            let fill_price_opt = match &order.order_type {
                OrderType::Market => {
                    // Market buy: add slippage; market sell: subtract slippage
                    let fp = match order.side {
                        OrderSide::Buy => price * (1.0 + slippage_factor),
                        OrderSide::Sell => price * (1.0 - slippage_factor),
                    };
                    Some(fp)
                }
                OrderType::Limit => {
                    match (order.side, order.limit_price) {
                        (OrderSide::Buy, Some(lp)) if price <= lp => {
                            // Buy limit: fill at market price (no worse than limit)
                            let fp = price * (1.0 + slippage_factor);
                            Some(fp.min(lp))
                        }
                        (OrderSide::Sell, Some(lp)) if price >= lp => {
                            let fp = price * (1.0 - slippage_factor);
                            Some(fp.max(lp))
                        }
                        _ => None,
                    }
                }
                OrderType::StopLoss(trigger) => {
                    let trigger = *trigger;
                    match order.side {
                        OrderSide::Buy if price >= trigger => {
                            Some(price * (1.0 + slippage_factor))
                        }
                        OrderSide::Sell if price <= trigger => {
                            Some(price * (1.0 - slippage_factor))
                        }
                        _ => None,
                    }
                }
            };

            if let Some(fp) = fill_price_opt {
                let commission = fp * order.qty * commission_rate;
                let fill = Fill {
                    order_id: order.id,
                    symbol: order.symbol.clone(),
                    side: order.side,
                    qty: order.qty,
                    fill_price: fp,
                    commission,
                    timestamp_ms,
                };
                new_fills.push(fill);
                // Order is consumed; don't re-add to remaining
            } else {
                remaining_orders.push(order);
            }
        }

        self.state.orders = remaining_orders;

        // Apply each fill.
        for fill in &new_fills {
            let fill_clone = fill.clone();
            self.apply_fill(&fill_clone);
            self.state.fills.push(fill.clone());
        }

        new_fills
    }

    /// Apply a fill to update capital, positions, and realized P&L.
    pub fn apply_fill(&mut self, fill: &Fill) {
        let position = self
            .state
            .positions
            .entry(fill.symbol.clone())
            .or_insert(Position {
                symbol: fill.symbol.clone(),
                qty: 0.0,
                avg_cost: 0.0,
                realized_pnl: 0.0,
            });

        match fill.side {
            OrderSide::Buy => {
                // Increase position; update avg_cost via VWAP formula.
                let old_value = position.qty * position.avg_cost;
                let new_value = fill.qty * fill.fill_price;
                position.qty += fill.qty;
                if position.qty > 0.0 {
                    position.avg_cost = (old_value + new_value) / position.qty;
                }
                // Deduct cash
                self.state.capital -= fill.qty * fill.fill_price + fill.commission;
            }
            OrderSide::Sell => {
                // Realize P&L for sold quantity.
                let sell_qty = fill.qty.min(position.qty);
                if sell_qty > 0.0 && position.qty > 0.0 {
                    let realized = sell_qty * (fill.fill_price - position.avg_cost);
                    position.realized_pnl += realized;
                }
                position.qty -= fill.qty;
                if position.qty < 0.0 {
                    position.qty = 0.0; // no short selling in this simple engine
                }
                // Add cash proceeds
                self.state.capital += fill.qty * fill.fill_price - fill.commission;
            }
        }
    }

    /// Update the equity curve with current mark-to-market value.
    ///
    /// `total_equity = capital + Σ unrealized_pnl(position)`.
    pub fn mark_to_market(&mut self, prices: &HashMap<String, f64>, timestamp_ms: u64) {
        let unrealized: f64 = self
            .state
            .positions
            .values()
            .map(|pos| {
                let market_price = prices.get(&pos.symbol).copied().unwrap_or(pos.avg_cost);
                pos.qty * (market_price - pos.avg_cost)
            })
            .sum();
        let total_equity = self.state.capital + unrealized;
        self.state.equity_curve.push((timestamp_ms, total_equity));
    }

    /// Compute backtest performance metrics from the equity curve and fills.
    #[must_use]
    pub fn compute_metrics(&self) -> BacktestMetrics {
        let equity = &self.state.equity_curve;
        let fills = &self.state.fills;

        let initial = self
            .state
            .equity_curve
            .first()
            .map(|e| e.1)
            .unwrap_or(self.config.initial_capital);
        let final_eq = equity.last().map(|e| e.1).unwrap_or(initial);
        let total_return = if initial != 0.0 {
            (final_eq - initial) / initial
        } else {
            0.0
        };

        let max_dd = max_drawdown(equity);

        // Sharpe ratio: mean / std of period returns, annualized assuming daily bars.
        let sharpe = {
            if equity.len() < 2 {
                0.0
            } else {
                let returns: Vec<f64> = equity
                    .windows(2)
                    .map(|w| {
                        let prev = w[0].1;
                        let curr = w[1].1;
                        if prev != 0.0 { (curr - prev) / prev } else { 0.0 }
                    })
                    .collect();
                let n = returns.len() as f64;
                let mean = returns.iter().sum::<f64>() / n;
                let var = returns.iter().map(|&r| (r - mean).powi(2)).sum::<f64>() / n.max(1.0);
                let std = var.sqrt();
                if std > 0.0 { mean / std * (252.0_f64).sqrt() } else { 0.0 }
            }
        };

        // Win rate and profit factor from fills (pair buy/sell as round trips).
        let (win_rate, profit_factor, total_trades) = compute_trade_stats(fills);

        BacktestMetrics {
            total_return,
            sharpe,
            max_drawdown: max_dd,
            win_rate,
            total_trades,
            profit_factor,
        }
    }
}

// ─────────────────────────────────────────
//  Metrics helpers
// ─────────────────────────────────────────

/// Compute win rate, profit factor, and total trade count from fills.
fn compute_trade_stats(fills: &[Fill]) -> (f64, f64, usize) {
    // Group fills by symbol; pair buys and sells in order.
    let mut buy_fills: HashMap<String, Vec<&Fill>> = HashMap::new();
    let mut sell_fills: HashMap<String, Vec<&Fill>> = HashMap::new();

    for fill in fills {
        match fill.side {
            OrderSide::Buy => buy_fills.entry(fill.symbol.clone()).or_default().push(fill),
            OrderSide::Sell => sell_fills.entry(fill.symbol.clone()).or_default().push(fill),
        }
    }

    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut gross_profit = 0.0_f64;
    let mut gross_loss = 0.0_f64;

    for (symbol, sells) in &sell_fills {
        if let Some(buys) = buy_fills.get(symbol) {
            for (buy, sell) in buys.iter().zip(sells.iter()) {
                let pnl = sell.fill_price - buy.fill_price;
                if pnl > 0.0 {
                    wins += 1;
                    gross_profit += pnl;
                } else {
                    losses += 1;
                    gross_loss += pnl.abs();
                }
            }
        }
    }

    let total_trades = wins + losses;
    let win_rate = if total_trades > 0 {
        wins as f64 / total_trades as f64
    } else {
        0.0
    };
    let profit_factor = if gross_loss > 0.0 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    (win_rate, profit_factor, total_trades)
}

// ─────────────────────────────────────────
//  BacktestMetrics
// ─────────────────────────────────────────

/// Summary performance metrics for a completed backtest.
#[derive(Debug, Clone)]
pub struct BacktestMetrics {
    /// Total return as a fraction (e.g. 0.15 = 15%).
    pub total_return: f64,
    /// Annualized Sharpe ratio (assuming daily observations, 252 trading days).
    pub sharpe: f64,
    /// Maximum peak-to-trough drawdown as a positive fraction (e.g. 0.10 = 10%).
    pub max_drawdown: f64,
    /// Fraction of round-trip trades that were profitable.
    pub win_rate: f64,
    /// Total number of completed round-trip trades.
    pub total_trades: usize,
    /// Gross profit / gross loss ratio.
    pub profit_factor: f64,
}

/// Compute the maximum peak-to-trough drawdown from an equity curve.
///
/// Returns a positive fraction representing the largest percentage drop.
#[must_use]
pub fn max_drawdown(equity: &[(u64, f64)]) -> f64 {
    if equity.is_empty() {
        return 0.0;
    }
    let mut peak = equity[0].1;
    let mut max_dd = 0.0_f64;

    for &(_, eq) in equity {
        if eq > peak {
            peak = eq;
        }
        if peak > 0.0 {
            let dd = (peak - eq) / peak;
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }

    max_dd
}

// ─────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine() -> BacktestEngine {
        BacktestEngine::new(BacktestConfig {
            initial_capital: 100_000.0,
            commission_bps: 5.0,
            slippage_bps: 2.0,
            max_position_pct: 0.25,
        })
    }

    #[test]
    fn market_order_fills_immediately() {
        let mut engine = make_engine();
        let _id = engine.submit_order("AAPL", OrderSide::Buy, 10.0, OrderType::Market);
        let fills = engine.process_tick("AAPL", 150.0, 1_000);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].symbol, "AAPL");
        assert_eq!(fills[0].qty, 10.0);
        // Fill price should be 150 + slippage (2 bps = 0.03)
        assert!(fills[0].fill_price > 150.0);
    }

    #[test]
    fn limit_order_fills_on_cross() {
        let mut engine = make_engine();
        // Limit buy at 100 — won't fill at 105
        engine.submit_limit_order("BTC", OrderSide::Buy, 1.0, 100.0);
        let fills = engine.process_tick("BTC", 105.0, 1_000);
        assert_eq!(fills.len(), 0, "limit buy should not fill above limit price");

        // Now price drops to 98 — should fill
        let fills = engine.process_tick("BTC", 98.0, 2_000);
        assert_eq!(fills.len(), 1);
        assert!(fills[0].fill_price <= 100.0);
    }

    #[test]
    fn stop_loss_triggers() {
        let mut engine = make_engine();
        // StopLoss sell at 90 (trigger: price <= 90)
        engine.submit_order("ETH", OrderSide::Sell, 2.0, OrderType::StopLoss(90.0));
        let fills = engine.process_tick("ETH", 95.0, 1_000);
        assert_eq!(fills.len(), 0, "stop not triggered yet");

        let fills = engine.process_tick("ETH", 89.0, 2_000);
        assert_eq!(fills.len(), 1, "stop should trigger at 89 <= 90");
    }

    #[test]
    fn pnl_after_round_trip() {
        let mut engine = make_engine();
        // Buy 1 unit at 100
        engine.submit_order("X", OrderSide::Buy, 1.0, OrderType::Market);
        engine.process_tick("X", 100.0, 1_000);

        let pos = engine.state.positions.get("X").unwrap();
        assert!(pos.qty > 0.0);

        // Sell 1 unit at 110
        engine.submit_order("X", OrderSide::Sell, 1.0, OrderType::Market);
        engine.process_tick("X", 110.0, 2_000);

        // Realized PnL should be positive (~10 - commissions)
        let pos = engine.state.positions.get("X").unwrap();
        assert!(pos.realized_pnl > 0.0, "realized PnL should be positive");
    }

    #[test]
    fn max_drawdown_calculation() {
        // Equity: 100 → 120 → 90 → 110 → drawdown = (120-90)/120 = 25%
        let equity = vec![(0, 100.0), (1, 120.0), (2, 90.0), (3, 110.0)];
        let dd = max_drawdown(&equity);
        assert!((dd - 0.25).abs() < 1e-9, "max drawdown should be 25%, got {dd}");
    }

    #[test]
    fn max_drawdown_monotone_increase() {
        let equity: Vec<(u64, f64)> = (0..10).map(|i| (i, 100.0 + i as f64)).collect();
        assert_eq!(max_drawdown(&equity), 0.0);
    }

    #[test]
    fn mark_to_market_updates_equity_curve() {
        let mut engine = make_engine();
        engine.submit_order("SPY", OrderSide::Buy, 10.0, OrderType::Market);
        engine.process_tick("SPY", 400.0, 1_000);

        let prices: HashMap<String, f64> = [("SPY".to_owned(), 420.0)].into();
        engine.mark_to_market(&prices, 2_000);
        assert_eq!(engine.state.equity_curve.len(), 1);
        let (ts, eq) = engine.state.equity_curve[0];
        assert_eq!(ts, 2_000);
        assert!(eq > 0.0);
    }
}
