//! Real-time portfolio risk calculation engine.
//!
//! Provides Greek aggregation, P&L attribution, stress testing,
//! and a concurrent DashMap-backed [`RiskEngine`].

use std::collections::{HashMap, VecDeque};
use dashmap::DashMap;

// ---------------------------------------------------------------------------
// Greeks
// ---------------------------------------------------------------------------

/// Option Greeks for a single position.
#[derive(Debug, Clone, Default)]
pub struct Greeks {
    /// Price sensitivity to spot (dV/dS).
    pub delta: f64,
    /// Second-order spot sensitivity (d²V/dS²).
    pub gamma: f64,
    /// Time decay (dV/dt, typically negative).
    pub theta: f64,
    /// Sensitivity to volatility (dV/dσ).
    pub vega: f64,
    /// Sensitivity to interest rate (dV/dr).
    pub rho: f64,
}

impl Greeks {
    /// Dollar delta: delta * spot * notional.
    pub fn dollar_delta(&self, spot: f64, notional: f64) -> f64 {
        self.delta * spot * notional
    }

    /// Scale all Greeks by a scalar factor.
    pub fn scale(&self, factor: f64) -> Greeks {
        Greeks {
            delta: self.delta * factor,
            gamma: self.gamma * factor,
            theta: self.theta * factor,
            vega: self.vega * factor,
            rho: self.rho * factor,
        }
    }

    /// Add two Greeks together (portfolio aggregation).
    pub fn add(&self, other: &Greeks) -> Greeks {
        Greeks {
            delta: self.delta + other.delta,
            gamma: self.gamma + other.gamma,
            theta: self.theta + other.theta,
            vega: self.vega + other.vega,
            rho: self.rho + other.rho,
        }
    }
}

// ---------------------------------------------------------------------------
// PnlAttribution
// ---------------------------------------------------------------------------

/// Decomposition of P&L into Greek-explained and unexplained components.
#[derive(Debug, Clone, Default)]
pub struct PnlAttribution {
    /// Total observed P&L.
    pub total_pnl: f64,
    /// Delta-explained P&L.
    pub delta_pnl: f64,
    /// Gamma-explained P&L.
    pub gamma_pnl: f64,
    /// Theta-explained P&L (time decay).
    pub theta_pnl: f64,
    /// Vega-explained P&L.
    pub vega_pnl: f64,
    /// Residual unexplained P&L.
    pub unexplained: f64,
}

// ---------------------------------------------------------------------------
// RiskFactor
// ---------------------------------------------------------------------------

/// Market risk factor driving portfolio risk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RiskFactor {
    /// Equity or commodity spot price.
    Spot {
        /// Ticker symbol.
        symbol: String,
    },
    /// Implied volatility surface.
    Vol {
        /// Ticker symbol.
        symbol: String,
    },
    /// Interest rate at a given tenor.
    Rate {
        /// Tenor (e.g. "2Y", "10Y").
        tenor: String,
    },
    /// Credit spread for an issuer.
    Credit {
        /// Issuer name.
        issuer: String,
    },
    /// Foreign exchange pair.
    Fx {
        /// Currency pair (e.g. "EURUSD").
        pair: String,
    },
}

// ---------------------------------------------------------------------------
// RiskSensitivity
// ---------------------------------------------------------------------------

/// Sensitivity of a position to a specific risk factor.
#[derive(Debug, Clone)]
pub struct RiskSensitivity {
    /// The market risk factor.
    pub factor: RiskFactor,
    /// Dollar value of a 1 basis-point move in rates.
    pub dv01: f64,
    /// Dollar value of a 1 bps credit spread move.
    pub cs01: f64,
    /// P&L for a 1% move in the underlying factor.
    pub sensitivity_1pct: f64,
}

// ---------------------------------------------------------------------------
// PortfolioRisk
// ---------------------------------------------------------------------------

/// Aggregated portfolio risk summary.
#[derive(Debug, Clone, Default)]
pub struct PortfolioRisk {
    /// Net portfolio delta.
    pub total_delta: f64,
    /// Net portfolio gamma.
    pub total_gamma: f64,
    /// Net portfolio vega.
    pub total_vega: f64,
    /// 95% historical Value at Risk.
    pub var_95: f64,
    /// 99% historical Value at Risk.
    pub var_99: f64,
    /// 99% Expected Shortfall (CVaR).
    pub expected_shortfall: f64,
    /// Portfolio beta to market.
    pub beta_to_market: f64,
    /// Concentration: top-5 positions as % of gross exposure.
    pub concentration_top5_pct: f64,
}

// ---------------------------------------------------------------------------
// RiskEngine
// ---------------------------------------------------------------------------

/// Concurrent real-time risk engine.
///
/// Stores per-symbol Greeks and prices in [`DashMap`]s and maintains a
/// rolling window of portfolio returns for VaR computation.
pub struct RiskEngine {
    greeks: DashMap<String, Greeks>,
    prices: DashMap<String, f64>,
    /// Rolling portfolio returns (max 252 observations).
    returns: std::sync::Mutex<VecDeque<f64>>,
}

impl RiskEngine {
    /// Create a new empty [`RiskEngine`].
    pub fn new() -> Self {
        Self {
            greeks: DashMap::new(),
            prices: DashMap::new(),
            returns: std::sync::Mutex::new(VecDeque::with_capacity(252)),
        }
    }

    /// Update the Greeks for a symbol.
    pub fn update_greeks(&self, symbol: &str, greeks: Greeks) {
        self.greeks.insert(symbol.to_string(), greeks);
    }

    /// Update the market price for a symbol.
    pub fn update_price(&self, symbol: &str, price: f64) {
        self.prices.insert(symbol.to_string(), price);
    }

    /// Record a portfolio-level return observation.
    ///
    /// The window is capped at 252 observations (one trading year).
    pub fn record_return(&self, portfolio_return: f64) {
        if let Ok(mut q) = self.returns.lock() {
            if q.len() >= 252 {
                q.pop_front();
            }
            q.push_back(portfolio_return);
        }
    }

    /// Aggregate all position Greeks into a single portfolio-level Greeks.
    pub fn aggregate_greeks(&self) -> Greeks {
        self.greeks.iter().fold(Greeks::default(), |acc, entry| acc.add(entry.value()))
    }

    /// Compute full portfolio risk metrics.
    pub fn compute_portfolio_risk(&self) -> PortfolioRisk {
        let agg = self.aggregate_greeks();
        let returns = self.returns.lock().map(|q| q.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();

        let var_95 = Self::historical_var(&returns, 0.95);
        let var_99 = Self::historical_var(&returns, 0.99);
        let es = Self::expected_shortfall_from_returns(&returns, 0.99);

        // Simple beta: correlation with a proxy market return (identity here).
        let beta = Self::compute_beta(&returns);

        PortfolioRisk {
            total_delta: agg.delta,
            total_gamma: agg.gamma,
            total_vega: agg.vega,
            var_95,
            var_99,
            expected_shortfall: es,
            beta_to_market: beta,
            concentration_top5_pct: self.concentration_top5(),
        }
    }

    /// Attribute P&L between curr and prev prices using the given Greeks.
    ///
    /// Uses first-order (delta, theta) and second-order (gamma) Taylor expansion.
    pub fn pnl_attribution(
        prev_prices: &HashMap<String, f64>,
        curr_prices: &HashMap<String, f64>,
        greeks: &Greeks,
    ) -> PnlAttribution {
        let mut total_pnl = 0.0;
        let mut ds = 0.0;
        let mut n = 0;

        for (sym, &curr) in curr_prices {
            if let Some(&prev) = prev_prices.get(sym) {
                ds += curr - prev;
                total_pnl += curr - prev;
                n += 1;
            }
        }

        // Average spot move across all symbols.
        let avg_ds = if n > 0 { ds / n as f64 } else { 0.0 };
        let dt = 1.0 / 252.0; // one trading day

        let delta_pnl = greeks.delta * avg_ds;
        let gamma_pnl = 0.5 * greeks.gamma * avg_ds * avg_ds;
        let theta_pnl = greeks.theta * dt;
        // Vega P&L requires vol change — approximate as zero here.
        let vega_pnl = 0.0;

        let explained = delta_pnl + gamma_pnl + theta_pnl + vega_pnl;
        let unexplained = total_pnl - explained;

        PnlAttribution {
            total_pnl,
            delta_pnl,
            gamma_pnl,
            theta_pnl,
            vega_pnl,
            unexplained,
        }
    }

    /// Estimate portfolio P&L under a uniform price shock of `shock_pct` (e.g. -0.10 for -10%).
    ///
    /// Approximation: delta * S * shock + 0.5 * gamma * S^2 * shock^2 averaged over symbols.
    pub fn stress_test(&self, shock_pct: f64) -> f64 {
        let greeks = self.aggregate_greeks();
        let avg_price: f64 = if self.prices.is_empty() {
            1.0
        } else {
            let sum: f64 = self.prices.iter().map(|e| *e.value()).sum();
            sum / self.prices.len() as f64
        };

        let ds = avg_price * shock_pct;
        greeks.delta * ds + 0.5 * greeks.gamma * ds * ds + greeks.theta / 252.0
    }

    /// Generate a human-readable risk report string.
    pub fn risk_report(&self) -> String {
        let risk = self.compute_portfolio_risk();
        format!(
            "=== Portfolio Risk Report ===\n\
             Delta:          {:.4}\n\
             Gamma:          {:.4}\n\
             Vega:           {:.4}\n\
             VaR 95%:        {:.4}\n\
             VaR 99%:        {:.4}\n\
             Expected Shortfall: {:.4}\n\
             Beta to Market: {:.4}\n\
             Top-5 Concentration: {:.2}%\n\
             ============================",
            risk.total_delta,
            risk.total_gamma,
            risk.total_vega,
            risk.var_95,
            risk.var_99,
            risk.expected_shortfall,
            risk.beta_to_market,
            risk.concentration_top5_pct,
        )
    }

    // --- Private helpers ---

    fn historical_var(returns: &[f64], confidence: f64) -> f64 {
        if returns.is_empty() {
            return 0.0;
        }
        let mut sorted = returns.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((1.0 - confidence) * sorted.len() as f64) as usize;
        let idx = idx.min(sorted.len().saturating_sub(1));
        -sorted[idx]
    }

    fn expected_shortfall_from_returns(returns: &[f64], confidence: f64) -> f64 {
        if returns.is_empty() {
            return 0.0;
        }
        let mut sorted = returns.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let cutoff = ((1.0 - confidence) * sorted.len() as f64) as usize;
        let cutoff = cutoff.min(sorted.len().saturating_sub(1));
        let tail = &sorted[..=cutoff];
        if tail.is_empty() {
            return 0.0;
        }
        -tail.iter().sum::<f64>() / tail.len() as f64
    }

    fn compute_beta(returns: &[f64]) -> f64 {
        // With no market returns available, return 1.0 as neutral assumption.
        if returns.is_empty() {
            return 1.0;
        }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
            / returns.len() as f64;
        // Beta = cov(port, market) / var(market). Assume perfect correlation with market.
        if variance < 1e-15 { 1.0 } else { variance.sqrt() * 10.0 }  // crude approximation
    }

    fn concentration_top5(&self) -> f64 {
        let mut prices: Vec<f64> = self.prices.iter().map(|e| e.value().abs()).collect();
        if prices.is_empty() {
            return 0.0;
        }
        prices.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let total: f64 = prices.iter().sum();
        if total <= 0.0 {
            return 0.0;
        }
        let top5: f64 = prices.iter().take(5).sum();
        (top5 / total) * 100.0
    }
}

impl Default for RiskEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeks_scale_add() {
        let g = Greeks { delta: 1.0, gamma: 2.0, theta: -0.5, vega: 3.0, rho: 0.1 };
        let scaled = g.scale(2.0);
        assert!((scaled.delta - 2.0).abs() < 1e-9);
        let added = g.add(&scaled);
        assert!((added.delta - 3.0).abs() < 1e-9);
    }

    #[test]
    fn dollar_delta() {
        let g = Greeks { delta: 0.5, ..Greeks::default() };
        assert!((g.dollar_delta(100.0, 1000.0) - 50_000.0).abs() < 1e-6);
    }

    #[test]
    fn risk_engine_aggregate() {
        let engine = RiskEngine::new();
        engine.update_greeks("AAPL", Greeks { delta: 0.5, gamma: 0.01, ..Greeks::default() });
        engine.update_greeks("GOOG", Greeks { delta: 0.3, gamma: 0.02, ..Greeks::default() });
        let agg = engine.aggregate_greeks();
        assert!((agg.delta - 0.8).abs() < 1e-9);
        assert!((agg.gamma - 0.03).abs() < 1e-9);
    }

    #[test]
    fn risk_engine_var() {
        let engine = RiskEngine::new();
        for i in 0..100 {
            engine.record_return((i as f64 - 50.0) * 0.001);
        }
        let risk = engine.compute_portfolio_risk();
        assert!(risk.var_95 >= 0.0);
        assert!(risk.var_99 >= 0.0);
        assert!(risk.expected_shortfall >= risk.var_99);
    }

    #[test]
    fn pnl_attribution_basic() {
        let greeks = Greeks { delta: 1.0, gamma: 0.0, theta: -10.0, vega: 0.0, rho: 0.0 };
        let mut prev = HashMap::new();
        prev.insert("SPY".to_string(), 400.0_f64);
        let mut curr = HashMap::new();
        curr.insert("SPY".to_string(), 405.0_f64);
        let attr = RiskEngine::pnl_attribution(&prev, &curr, &greeks);
        assert!((attr.delta_pnl - 5.0).abs() < 1e-9);
    }

    #[test]
    fn stress_test_sign() {
        let engine = RiskEngine::new();
        engine.update_greeks("X", Greeks { delta: 1.0, ..Greeks::default() });
        engine.update_price("X", 100.0);
        let pnl = engine.stress_test(-0.10);
        assert!(pnl < 0.0); // negative delta => loss on down shock
    }

    #[test]
    fn risk_report_non_empty() {
        let engine = RiskEngine::new();
        let report = engine.risk_report();
        assert!(report.contains("Delta"));
        assert!(report.contains("VaR"));
    }
}
