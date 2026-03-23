//! Real-time portfolio risk tracking with concurrent position management.
//!
//! Provides [`RiskTracker`] for managing positions, recording daily returns,
//! and computing risk metrics such as VaR, max drawdown, Sharpe ratio, and volatility.

use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// A single position in the portfolio.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Position {
    /// Ticker symbol or instrument identifier.
    pub symbol: String,
    /// Number of units held (positive = long).
    pub quantity: f64,
    /// Average cost basis per unit.
    pub avg_cost: f64,
    /// Current market price per unit.
    pub current_price: f64,
}

impl Position {
    /// Current market value of the position.
    pub fn market_value(&self) -> f64 {
        self.quantity * self.current_price
    }

    /// Unrealized profit or loss.
    pub fn unrealized_pnl(&self) -> f64 {
        self.quantity * (self.current_price - self.avg_cost)
    }

    /// Position weight relative to the total portfolio value.
    pub fn weight(&self, total_portfolio_value: f64) -> f64 {
        if total_portfolio_value.abs() < 1e-12 {
            return 0.0;
        }
        self.market_value() / total_portfolio_value
    }
}

/// A point-in-time snapshot of the entire portfolio.
#[derive(Debug, Clone)]
pub struct PortfolioSnapshot {
    /// All positions at snapshot time.
    pub positions: Vec<Position>,
    /// Unix timestamp in milliseconds when snapshot was taken.
    pub timestamp: u64,
}

impl PortfolioSnapshot {
    /// Total market value of all positions.
    pub fn total_value(&self) -> f64 {
        self.positions.iter().map(|p| p.market_value()).sum()
    }

    /// Top `n` positions by absolute market value (descending).
    pub fn top_holdings(&self, n: usize) -> Vec<&Position> {
        let mut sorted: Vec<&Position> = self.positions.iter().collect();
        sorted.sort_by(|a, b| {
            b.market_value()
                .abs()
                .partial_cmp(&a.market_value().abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted.into_iter().take(n).collect()
    }

    /// Herfindahl-Hirschman Index of portfolio concentration.
    ///
    /// HHI = sum(w_i^2) where w_i is the weight of position i.
    /// Ranges from 1/N (perfectly diversified) to 1.0 (fully concentrated).
    pub fn concentration_hhi(&self) -> f64 {
        let total = self.total_value();
        if total.abs() < 1e-12 {
            return 0.0;
        }
        self.positions
            .iter()
            .map(|p| {
                let w = p.weight(total);
                w * w
            })
            .sum()
    }
}

/// Concurrent real-time portfolio risk tracker.
///
/// Thread-safe: positions are stored in a [`DashMap`] and daily returns
/// in a [`Mutex`]-protected [`VecDeque`] capped at 252 observations.
pub struct RiskTracker {
    positions: DashMap<String, Position>,
    daily_returns: Mutex<VecDeque<f64>>,
}

impl RiskTracker {
    /// Construct a new empty `RiskTracker`.
    pub fn new() -> Self {
        Self {
            positions: DashMap::new(),
            daily_returns: Mutex::new(VecDeque::with_capacity(252)),
        }
    }

    /// Update the current price for an existing position.
    ///
    /// If the symbol is not tracked, this is a no-op.
    pub fn update_price(&self, symbol: &str, price: f64) {
        if let Some(mut pos) = self.positions.get_mut(symbol) {
            pos.current_price = price;
        }
    }

    /// Insert or replace a position.
    pub fn add_position(&self, position: Position) {
        self.positions.insert(position.symbol.clone(), position);
    }

    /// Remove a position by symbol.
    pub fn remove_position(&self, symbol: &str) {
        self.positions.remove(symbol);
    }

    /// Record a daily portfolio return. Keeps at most 252 observations (rolling).
    pub fn record_daily_return(&self, portfolio_return: f64) {
        if let Ok(mut dq) = self.daily_returns.lock() {
            if dq.len() >= 252 {
                dq.pop_front();
            }
            dq.push_back(portfolio_return);
        }
    }

    /// Take a point-in-time snapshot of all positions.
    pub fn snapshot(&self) -> PortfolioSnapshot {
        let positions: Vec<Position> = self
            .positions
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        PortfolioSnapshot { positions, timestamp }
    }

    /// Historical VaR at the 95% confidence level from stored daily returns.
    pub fn var_95(&self) -> f64 {
        self.historical_var(0.95)
    }

    /// Historical VaR at the 99% confidence level from stored daily returns.
    pub fn var_99(&self) -> f64 {
        self.historical_var(0.99)
    }

    /// Peak-to-trough maximum drawdown from the stored daily return series.
    ///
    /// Returns a positive number representing the fractional loss from peak equity.
    pub fn max_drawdown(&self) -> f64 {
        let returns = self.returns_vec();
        if returns.is_empty() {
            return 0.0;
        }

        let mut peak = 1.0_f64;
        let mut equity = 1.0_f64;
        let mut max_dd = 0.0_f64;

        for r in &returns {
            equity *= 1.0 + r;
            if equity > peak {
                peak = equity;
            }
            let dd = (peak - equity) / peak;
            if dd > max_dd {
                max_dd = dd;
            }
        }
        max_dd
    }

    /// Annualized Sharpe ratio: (mean_return - risk_free) / std_dev * sqrt(252).
    pub fn sharpe_ratio(&self, risk_free_rate: f64) -> f64 {
        let returns = self.returns_vec();
        let n = returns.len();
        if n < 2 {
            return 0.0;
        }
        let mean = returns.iter().sum::<f64>() / n as f64;
        let daily_rf = risk_free_rate / 252.0;
        let excess = mean - daily_rf;
        let std_dev = self.std_dev_of(&returns);
        if std_dev < 1e-12 {
            return 0.0;
        }
        excess / std_dev * 252_f64.sqrt()
    }

    /// Annualized portfolio volatility from daily returns: std_dev * sqrt(252).
    pub fn volatility_annualized(&self) -> f64 {
        let returns = self.returns_vec();
        self.std_dev_of(&returns) * 252_f64.sqrt()
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    fn returns_vec(&self) -> Vec<f64> {
        self.daily_returns
            .lock()
            .map(|dq| dq.iter().copied().collect())
            .unwrap_or_default()
    }

    fn historical_var(&self, confidence: f64) -> f64 {
        let returns = self.returns_vec();
        if returns.is_empty() {
            return 0.0;
        }
        let mut sorted = returns.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let idx = ((1.0 - confidence) * n as f64).floor() as usize;
        let idx = idx.min(n - 1);
        -sorted[idx].min(0.0)
    }

    fn std_dev_of(&self, returns: &[f64]) -> f64 {
        let n = returns.len();
        if n < 2 {
            return 0.0;
        }
        let mean = returns.iter().sum::<f64>() / n as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
        variance.sqrt()
    }
}

impl Default for RiskTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker() -> RiskTracker {
        let t = RiskTracker::new();
        t.add_position(Position {
            symbol: "AAPL".into(),
            quantity: 100.0,
            avg_cost: 150.0,
            current_price: 160.0,
        });
        t.add_position(Position {
            symbol: "MSFT".into(),
            quantity: 50.0,
            avg_cost: 300.0,
            current_price: 320.0,
        });
        t
    }

    #[test]
    fn market_value_and_pnl() {
        let p = Position {
            symbol: "AAPL".into(),
            quantity: 100.0,
            avg_cost: 150.0,
            current_price: 160.0,
        };
        assert!((p.market_value() - 16_000.0).abs() < 1e-8);
        assert!((p.unrealized_pnl() - 1_000.0).abs() < 1e-8);
    }

    #[test]
    fn snapshot_total_value() {
        let t = make_tracker();
        let s = t.snapshot();
        // AAPL: 100*160=16000, MSFT: 50*320=16000 => 32000
        assert!((s.total_value() - 32_000.0).abs() < 1e-6);
    }

    #[test]
    fn hhi_two_equal_positions() {
        let t = make_tracker();
        let s = t.snapshot();
        let hhi = s.concentration_hhi();
        // Two equal positions: HHI = 0.25 + 0.25 = 0.5
        assert!((hhi - 0.5).abs() < 1e-6);
    }

    #[test]
    fn var_from_returns() {
        let t = RiskTracker::new();
        for i in -50i32..=50 {
            t.record_daily_return(i as f64 / 1000.0);
        }
        let var95 = t.var_95();
        let var99 = t.var_99();
        assert!(var99 >= var95);
        assert!(var95 > 0.0);
    }

    #[test]
    fn sharpe_ratio_positive_drift() {
        let t = RiskTracker::new();
        for _ in 0..100 {
            t.record_daily_return(0.001); // constant positive return
        }
        // With constant returns, std_dev is 0 => Sharpe = 0
        let sharpe = t.sharpe_ratio(0.02);
        assert!(sharpe.is_finite());
    }

    #[test]
    fn max_drawdown_monotone_decline() {
        let t = RiskTracker::new();
        for _ in 0..10 {
            t.record_daily_return(-0.01);
        }
        let dd = t.max_drawdown();
        assert!(dd > 0.0 && dd < 1.0);
    }

    #[test]
    fn update_price_changes_position() {
        let t = make_tracker();
        t.update_price("AAPL", 200.0);
        let s = t.snapshot();
        let aapl = s.positions.iter().find(|p| p.symbol == "AAPL").unwrap();
        assert!((aapl.current_price - 200.0).abs() < 1e-10);
    }
}
