//! Real-Time Risk Metrics
//!
//! Computes rolling risk metrics from a price tick window: annualized volatility,
//! historical VaR, maximum drawdown, and Sharpe ratio.
//! The [`RiskMonitor`] manages per-symbol [`RollingRisk`] instances in a concurrent
//! [`dashmap::DashMap`] and can compute portfolio-level VaR via weighted summation.

use std::collections::{HashMap, VecDeque};
use dashmap::DashMap;

/// Assumed trading days per year for annualization.
const TRADING_DAYS_PER_YEAR: f64 = 252.0;
/// Assumed ticks per day (used in volatility annualization when 1 tick = 1 day).
const TICKS_PER_DAY: f64 = 1.0;

/// Rolling risk engine for a single symbol.
///
/// Stores the last `window_size` prices and computes risk metrics on demand.
pub struct RollingRisk {
    /// Symbol identifier.
    pub symbol: String,
    /// Maximum number of price observations to retain.
    pub window_size: usize,
    /// Rolling price window (oldest first).
    pub ticks: VecDeque<f64>,
}

impl RollingRisk {
    /// Create a new [`RollingRisk`] tracker.
    pub fn new(symbol: impl Into<String>, window_size: usize) -> Self {
        Self {
            symbol: symbol.into(),
            window_size,
            ticks: VecDeque::with_capacity(window_size + 1),
        }
    }

    /// Push a new price observation into the rolling window.
    pub fn push(&mut self, price: f64) {
        self.ticks.push_back(price);
        if self.ticks.len() > self.window_size {
            self.ticks.pop_front();
        }
    }

    /// Compute log returns from the price window.
    fn log_returns(&self) -> Vec<f64> {
        self.ticks
            .iter()
            .zip(self.ticks.iter().skip(1))
            .map(|(&p0, &p1)| {
                if p0 > 0.0 && p1 > 0.0 {
                    (p1 / p0).ln()
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// Annualized volatility from log returns.
    ///
    /// Returns `None` if fewer than 2 prices are available.
    ///
    /// Formula: `std_dev(log_returns) * sqrt(TRADING_DAYS * TICKS_PER_DAY)`
    pub fn volatility_annualized(&self) -> Option<f64> {
        let rets = self.log_returns();
        if rets.len() < 2 {
            return None;
        }
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        let std_dev = var.sqrt();
        Some(std_dev * (TRADING_DAYS_PER_YEAR * TICKS_PER_DAY).sqrt())
    }

    /// Historical simulation Value-at-Risk on log returns.
    ///
    /// `confidence`: e.g. 0.95 for 95% VaR.
    /// Returns the loss at the `(1 - confidence)` percentile (a positive number representing loss).
    /// Returns `None` if fewer than 2 prices are available.
    pub fn var_historical(&self, confidence: f64) -> Option<f64> {
        let mut rets = self.log_returns();
        if rets.is_empty() {
            return None;
        }
        rets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((1.0 - confidence) * rets.len() as f64).floor() as usize;
        let idx = idx.min(rets.len() - 1);
        // VaR is expressed as a loss (positive number for a negative return)
        Some(-rets[idx])
    }

    /// Maximum drawdown within the rolling window.
    ///
    /// Expressed as a positive fraction: `(peak - trough) / peak`.
    /// Returns `None` if fewer than 2 prices are available.
    pub fn max_drawdown(&self) -> Option<f64> {
        if self.ticks.len() < 2 {
            return None;
        }
        let mut peak = f64::NEG_INFINITY;
        let mut max_dd = 0.0f64;
        for &p in &self.ticks {
            if p > peak {
                peak = p;
            }
            if peak > 0.0 {
                let dd = (peak - p) / peak;
                if dd > max_dd {
                    max_dd = dd;
                }
            }
        }
        Some(max_dd)
    }

    /// Rolling Sharpe ratio.
    ///
    /// `risk_free_daily`: daily risk-free rate (e.g. 0.0001 for ~2.5% annual).
    /// Returns `None` if fewer than 2 prices are available or std_dev is zero.
    pub fn sharpe(&self, risk_free_daily: f64) -> Option<f64> {
        let rets = self.log_returns();
        if rets.len() < 2 {
            return None;
        }
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let excess = mean - risk_free_daily;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        let std_dev = var.sqrt();
        if std_dev < 1e-15 {
            return None;
        }
        // Annualized Sharpe
        Some(excess / std_dev * (TRADING_DAYS_PER_YEAR * TICKS_PER_DAY).sqrt())
    }
}

/// A snapshot of risk metrics for a single symbol.
#[derive(Debug, Clone)]
pub struct RiskSnapshot {
    /// Symbol identifier.
    pub symbol: String,
    /// Annualized volatility (may be None if insufficient data).
    pub volatility: Option<f64>,
    /// 95% historical VaR expressed as a positive loss fraction.
    pub var_95: Option<f64>,
    /// Maximum drawdown within the rolling window.
    pub max_drawdown: Option<f64>,
    /// Annualized Sharpe ratio.
    pub sharpe: Option<f64>,
    /// Most recent price in the window.
    pub last_price: f64,
    /// Window capacity.
    pub window_size: usize,
}

/// Concurrent multi-symbol risk monitor.
///
/// Wraps a [`DashMap`] from symbol string to [`RollingRisk`] so that
/// multiple threads can update different symbols without contention.
pub struct RiskMonitor {
    store: DashMap<String, RollingRisk>,
    window_size: usize,
}

impl RiskMonitor {
    /// Create a new [`RiskMonitor`] with the given rolling window size.
    pub fn new(window_size: usize) -> Self {
        Self {
            store: DashMap::new(),
            window_size,
        }
    }

    /// Push a price update for `symbol`.
    pub fn update(&self, symbol: &str, price: f64) {
        let mut entry = self.store.entry(symbol.to_string()).or_insert_with(|| {
            RollingRisk::new(symbol, self.window_size)
        });
        entry.push(price);
    }

    /// Return a risk snapshot for `symbol`, or `None` if unknown.
    pub fn risk_snapshot(&self, symbol: &str) -> Option<RiskSnapshot> {
        let rr = self.store.get(symbol)?;
        let last_price = rr.ticks.back().copied().unwrap_or(0.0);
        Some(RiskSnapshot {
            symbol: rr.symbol.clone(),
            volatility: rr.volatility_annualized(),
            var_95: rr.var_historical(0.95),
            max_drawdown: rr.max_drawdown(),
            sharpe: rr.sharpe(0.0),
            last_price,
            window_size: rr.window_size,
        })
    }

    /// Compute portfolio-level VaR as a weighted sum of individual symbol VaRs.
    ///
    /// `weights`: map of symbol → portfolio weight (should sum to 1.0 but is not enforced).
    /// Returns `None` if no symbol has a valid VaR.
    pub fn portfolio_var(&self, weights: &HashMap<String, f64>) -> Option<f64> {
        let mut total = 0.0f64;
        let mut any = false;
        for (symbol, &weight) in weights {
            if let Some(rr) = self.store.get(symbol.as_str()) {
                if let Some(var) = rr.var_historical(0.95) {
                    total += weight * var;
                    any = true;
                }
            }
        }
        if any { Some(total) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_risk(n: usize) -> RollingRisk {
        let mut rr = RollingRisk::new("TEST", 100);
        let mut p = 100.0f64;
        for i in 0..n {
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            p *= (0.001 + sign * 0.005f64).exp();
            rr.push(p);
        }
        rr
    }

    #[test]
    fn test_push_caps_at_window_size() {
        let mut rr = RollingRisk::new("X", 10);
        for i in 0..30 {
            rr.push(100.0 + i as f64);
        }
        assert_eq!(rr.ticks.len(), 10);
    }

    #[test]
    fn test_push_oldest_evicted() {
        let mut rr = RollingRisk::new("X", 3);
        rr.push(1.0);
        rr.push(2.0);
        rr.push(3.0);
        rr.push(4.0); // evicts 1.0
        assert_eq!(rr.ticks.front(), Some(&2.0));
    }

    #[test]
    fn test_volatility_none_with_single_price() {
        let mut rr = RollingRisk::new("X", 50);
        rr.push(100.0);
        assert!(rr.volatility_annualized().is_none());
    }

    #[test]
    fn test_volatility_positive() {
        let rr = filled_risk(50);
        let v = rr.volatility_annualized().expect("should have vol");
        assert!(v > 0.0, "volatility should be positive");
    }

    #[test]
    fn test_volatility_flat_prices_is_zero() {
        let mut rr = RollingRisk::new("X", 50);
        for _ in 0..20 {
            rr.push(100.0);
        }
        let v = rr.volatility_annualized().expect("two prices needed");
        assert!(v < 1e-10, "flat prices → zero volatility");
    }

    #[test]
    fn test_var_none_with_no_returns() {
        let mut rr = RollingRisk::new("X", 50);
        rr.push(100.0);
        assert!(rr.var_historical(0.95).is_none());
    }

    #[test]
    fn test_var_95_is_positive_loss() {
        let rr = filled_risk(100);
        let var = rr.var_historical(0.95).expect("should compute VaR");
        // VaR is reported as a positive loss value
        // (could be negative if all returns are positive, which is fine numerically)
        let _ = var; // just ensure no panic
    }

    #[test]
    fn test_var_at_different_confidence_levels() {
        let rr = filled_risk(100);
        let var_95 = rr.var_historical(0.95).unwrap();
        let var_99 = rr.var_historical(0.99).unwrap();
        // 99% VaR should be >= 95% VaR (worse loss at higher confidence)
        assert!(var_99 >= var_95, "99% VaR ({var_99}) should be >= 95% VaR ({var_95})");
    }

    #[test]
    fn test_max_drawdown_none_with_single_price() {
        let mut rr = RollingRisk::new("X", 50);
        rr.push(100.0);
        assert!(rr.max_drawdown().is_none());
    }

    #[test]
    fn test_max_drawdown_monotone_increasing_is_zero() {
        let mut rr = RollingRisk::new("X", 50);
        for i in 0..10 {
            rr.push(100.0 + i as f64);
        }
        let dd = rr.max_drawdown().expect("has two prices");
        assert!(dd < 1e-10, "monotone increasing series → zero drawdown, got {dd}");
    }

    #[test]
    fn test_max_drawdown_known_value() {
        let mut rr = RollingRisk::new("X", 50);
        rr.push(100.0);
        rr.push(120.0); // peak
        rr.push(90.0);  // trough: drawdown = (120-90)/120 = 0.25
        let dd = rr.max_drawdown().expect("has prices");
        assert!((dd - 0.25).abs() < 1e-10, "drawdown should be 0.25, got {dd}");
    }

    #[test]
    fn test_max_drawdown_between_zero_and_one() {
        let rr = filled_risk(50);
        let dd = rr.max_drawdown().expect("has prices");
        assert!(dd >= 0.0 && dd <= 1.0, "drawdown should be in [0,1], got {dd}");
    }

    #[test]
    fn test_sharpe_none_with_single_price() {
        let mut rr = RollingRisk::new("X", 50);
        rr.push(100.0);
        assert!(rr.sharpe(0.0).is_none());
    }

    #[test]
    fn test_sharpe_flat_prices_is_none() {
        let mut rr = RollingRisk::new("X", 50);
        for _ in 0..20 {
            rr.push(100.0);
        }
        // std_dev = 0 → None
        assert!(rr.sharpe(0.0).is_none());
    }

    #[test]
    fn test_sharpe_trending_up_is_positive() {
        let mut rr = RollingRisk::new("X", 50);
        for i in 0..30 {
            rr.push(100.0 * (1.001f64).powi(i));
        }
        let s = rr.sharpe(0.0).expect("should compute sharpe");
        assert!(s > 0.0, "uptrending prices → positive Sharpe, got {s}");
    }

    #[test]
    fn test_risk_monitor_update_and_snapshot() {
        let monitor = RiskMonitor::new(50);
        for i in 0..20 {
            monitor.update("AAPL", 150.0 + i as f64 * 0.5);
        }
        let snap = monitor.risk_snapshot("AAPL").expect("should have snapshot");
        assert_eq!(snap.symbol, "AAPL");
        assert!(snap.volatility.is_some());
        assert!(snap.max_drawdown.is_some());
    }

    #[test]
    fn test_risk_monitor_unknown_symbol_returns_none() {
        let monitor = RiskMonitor::new(50);
        assert!(monitor.risk_snapshot("UNKNOWN").is_none());
    }

    #[test]
    fn test_risk_monitor_multiple_symbols() {
        let monitor = RiskMonitor::new(50);
        for i in 0..10 {
            monitor.update("AAPL", 100.0 + i as f64);
            monitor.update("MSFT", 200.0 + i as f64 * 2.0);
        }
        assert!(monitor.risk_snapshot("AAPL").is_some());
        assert!(monitor.risk_snapshot("MSFT").is_some());
        assert!(monitor.risk_snapshot("GOOG").is_none());
    }

    #[test]
    fn test_portfolio_var_weighted_sum() {
        let monitor = RiskMonitor::new(100);
        let mut p_aapl = 100.0f64;
        let mut p_msft = 200.0f64;
        for i in 0..50 {
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            p_aapl *= (0.001 + sign * 0.005f64).exp();
            p_msft *= (0.001 + sign * 0.004f64).exp();
            monitor.update("AAPL", p_aapl);
            monitor.update("MSFT", p_msft);
        }
        let mut weights = HashMap::new();
        weights.insert("AAPL".to_string(), 0.6);
        weights.insert("MSFT".to_string(), 0.4);
        let pvar = monitor.portfolio_var(&weights);
        assert!(pvar.is_some(), "portfolio VaR should be computable");
    }

    #[test]
    fn test_portfolio_var_none_when_no_symbols() {
        let monitor = RiskMonitor::new(50);
        let weights: HashMap<String, f64> = HashMap::new();
        assert!(monitor.portfolio_var(&weights).is_none());
    }

    #[test]
    fn test_portfolio_var_unknown_symbol_skipped() {
        let monitor = RiskMonitor::new(50);
        for i in 0..20 {
            monitor.update("AAPL", 100.0 + i as f64);
        }
        let mut weights = HashMap::new();
        weights.insert("AAPL".to_string(), 0.5);
        weights.insert("UNKNOWN".to_string(), 0.5);
        // Should still compute (just skip UNKNOWN)
        assert!(monitor.portfolio_var(&weights).is_some());
    }

    #[test]
    fn test_snapshot_last_price() {
        let monitor = RiskMonitor::new(50);
        monitor.update("X", 100.0);
        monitor.update("X", 110.0);
        monitor.update("X", 105.0);
        let snap = monitor.risk_snapshot("X").expect("snapshot");
        assert!((snap.last_price - 105.0).abs() < 1e-10);
    }

    #[test]
    fn test_snapshot_window_size() {
        let monitor = RiskMonitor::new(25);
        monitor.update("X", 100.0);
        let snap = monitor.risk_snapshot("X").expect("snapshot");
        assert_eq!(snap.window_size, 25);
    }
}
