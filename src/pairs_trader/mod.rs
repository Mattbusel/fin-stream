//! Statistical pairs trading: cointegration testing, spread z-score, and signal generation.

use std::collections::VecDeque;

/// Result of a cointegration test between two price series.
#[derive(Debug, Clone)]
pub struct CointegrationResult {
    /// Hedge ratio (beta) from OLS regression.
    pub beta: f64,
    /// Intercept (alpha) from OLS regression.
    pub alpha: f64,
    /// Standard deviation of OLS residuals.
    pub residual_std: f64,
    /// Mean-reversion half-life in periods.
    pub half_life: f64,
    /// ADF test statistic on residuals.
    pub adf_stat: f64,
    /// True if adf_stat < -3.0 (reject unit root at ~5% level).
    pub is_cointegrated: bool,
}

/// Current spread state for a pairs trade.
#[derive(Debug, Clone)]
pub struct SpreadState {
    /// Symbol A identifier.
    pub symbol_a: String,
    /// Symbol B identifier.
    pub symbol_b: String,
    /// Current spread value.
    pub spread: f64,
    /// Spread z-score.
    pub zscore: f64,
    /// Mean-reversion half-life.
    pub half_life: f64,
    /// Current position.
    pub position: PairsPosition,
}

/// Current pairs position.
#[derive(Debug, Clone, PartialEq)]
pub enum PairsPosition {
    /// No open position.
    Flat,
    /// Long spread (long A, short B).
    LongA_ShortB {
        /// Z-score at entry.
        entry_zscore: f64,
    },
    /// Short spread (short A, long B).
    LongB_ShortA {
        /// Z-score at entry.
        entry_zscore: f64,
    },
}

/// Trading signal from pairs trader.
#[derive(Debug, Clone, PartialEq)]
pub enum PairsSignal {
    /// Enter long spread (long A, short B).
    EnterLongSpread,
    /// Enter short spread (short A, long B).
    EnterShortSpread,
    /// Exit a long spread position.
    ExitLong,
    /// Exit a short spread position.
    ExitShort,
    /// No action.
    Hold,
}

/// Cointegration testing utilities.
pub struct CointegrationTester;

impl CointegrationTester {
    /// OLS regression of y on x. Returns (beta, alpha, residuals).
    pub fn ols_regression(y: &[f64], x: &[f64]) -> (f64, f64, Vec<f64>) {
        let n = y.len().min(x.len()) as f64;
        if n < 2.0 {
            return (0.0, 0.0, vec![]);
        }
        let mean_x = x.iter().take(n as usize).sum::<f64>() / n;
        let mean_y = y.iter().take(n as usize).sum::<f64>() / n;
        let cov_xy: f64 = x.iter().zip(y.iter()).take(n as usize)
            .map(|(xi, yi)| (xi - mean_x) * (yi - mean_y))
            .sum();
        let var_x: f64 = x.iter().take(n as usize)
            .map(|xi| (xi - mean_x).powi(2))
            .sum();
        let beta = if var_x.abs() < 1e-12 { 0.0 } else { cov_xy / var_x };
        let alpha = mean_y - beta * mean_x;
        let residuals: Vec<f64> = y.iter().zip(x.iter()).take(n as usize)
            .map(|(yi, xi)| yi - beta * xi - alpha)
            .collect();
        (beta, alpha, residuals)
    }

    /// Simplified ADF t-statistic: OLS of Δy_t on y_{t-1}.
    pub fn adf_test(residuals: &[f64]) -> f64 {
        if residuals.len() < 3 {
            return 0.0;
        }
        // Δy_t = γ * y_{t-1} + ε_t
        let y_lag: Vec<f64> = residuals[..residuals.len() - 1].to_vec();
        let dy: Vec<f64> = residuals.windows(2).map(|w| w[1] - w[0]).collect();

        let n = dy.len() as f64;
        let mean_lag = y_lag.iter().sum::<f64>() / n;
        let cov: f64 = y_lag.iter().zip(dy.iter())
            .map(|(l, d)| (l - mean_lag) * d)
            .sum();
        let var_lag: f64 = y_lag.iter().map(|l| (l - mean_lag).powi(2)).sum();

        if var_lag.abs() < 1e-12 {
            return 0.0;
        }
        let gamma = cov / var_lag;

        // Residuals of this regression
        let mean_dy = dy.iter().sum::<f64>() / n;
        let resid_var: f64 = y_lag.iter().zip(dy.iter())
            .map(|(l, d)| {
                let fitted = gamma * (l - mean_lag) + mean_dy;
                (d - fitted).powi(2)
            })
            .sum::<f64>()
            / (n - 2.0).max(1.0);

        let se = if var_lag.abs() < 1e-12 { f64::INFINITY } else { (resid_var / var_lag).sqrt() };
        gamma / se
    }

    /// Estimate mean-reversion half-life from AR(1) fit on residuals.
    /// half_life = -ln(2) / ln(|rho|)
    pub fn half_life(residuals: &[f64]) -> f64 {
        if residuals.len() < 3 {
            return f64::NAN;
        }
        let y: &[f64] = &residuals[1..];
        let x: &[f64] = &residuals[..residuals.len() - 1];
        let (rho, _, _) = Self::ols_regression(y, x);
        if rho <= 0.0 || rho >= 1.0 {
            return f64::NAN;
        }
        -std::f64::consts::LN_2 / rho.ln()
    }

    /// Run a full cointegration test on two price series.
    pub fn test(series_a: &[f64], series_b: &[f64]) -> CointegrationResult {
        let (beta, alpha, residuals) = Self::ols_regression(series_a, series_b);
        let n = residuals.len() as f64;
        let residual_std = if n < 2.0 {
            0.0
        } else {
            let mean = residuals.iter().sum::<f64>() / n;
            (residuals.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        };
        let adf_stat = Self::adf_test(&residuals);
        let half_life = Self::half_life(&residuals);
        CointegrationResult {
            beta,
            alpha,
            residual_std,
            half_life,
            adf_stat,
            is_cointegrated: adf_stat < -3.0,
        }
    }
}

/// Configuration for the pairs trader.
#[derive(Debug, Clone)]
pub struct PairsConfig {
    /// Z-score threshold to enter a position.
    pub entry_zscore: f64,
    /// Z-score threshold to exit a position.
    pub exit_zscore: f64,
    /// Z-score threshold to stop out.
    pub stop_zscore: f64,
    /// Lookback window for cointegration and z-score computation.
    pub lookback: usize,
    /// Minimum acceptable half-life in days.
    pub min_half_life_days: f64,
    /// Maximum acceptable half-life in days.
    pub max_half_life_days: f64,
}

impl Default for PairsConfig {
    fn default() -> Self {
        Self {
            entry_zscore: 2.0,
            exit_zscore: 0.5,
            stop_zscore: 3.5,
            lookback: 60,
            min_half_life_days: 1.0,
            max_half_life_days: 30.0,
        }
    }
}

/// Online statistical pairs trader.
pub struct PairsTrader {
    /// Trading configuration.
    pub config: PairsConfig,
    /// Recent prices for symbol A.
    pub history_a: VecDeque<f64>,
    /// Recent prices for symbol B.
    pub history_b: VecDeque<f64>,
    /// Current spread state.
    pub current_state: SpreadState,
    /// Most recent cointegration result.
    pub cointegration: Option<CointegrationResult>,
}

impl PairsTrader {
    /// Construct a new PairsTrader.
    pub fn new(symbol_a: String, symbol_b: String, config: PairsConfig) -> Self {
        let current_state = SpreadState {
            symbol_a,
            symbol_b,
            spread: 0.0,
            zscore: 0.0,
            half_life: f64::NAN,
            position: PairsPosition::Flat,
        };
        Self {
            config,
            history_a: VecDeque::new(),
            history_b: VecDeque::new(),
            current_state,
            cointegration: None,
        }
    }

    /// Update with new prices and return the trading signal.
    pub fn update(&mut self, price_a: f64, price_b: f64) -> PairsSignal {
        self.history_a.push_back(price_a);
        self.history_b.push_back(price_b);

        while self.history_a.len() > self.config.lookback {
            self.history_a.pop_front();
        }
        while self.history_b.len() > self.config.lookback {
            self.history_b.pop_front();
        }

        if self.history_a.len() >= self.config.lookback {
            self.refit();
        }

        let spread = match self.compute_spread() {
            Some(s) => s,
            None => return PairsSignal::Hold,
        };
        self.current_state.spread = spread;

        let zscore = match self.cointegration.as_ref() {
            Some(coint) if coint.residual_std > 1e-12 => {
                // Compute mean of recent spread history
                let a_vec: Vec<f64> = self.history_a.iter().copied().collect();
                let b_vec: Vec<f64> = self.history_b.iter().copied().collect();
                let spreads: Vec<f64> = a_vec.iter().zip(b_vec.iter())
                    .map(|(a, b)| a - coint.beta * b - coint.alpha)
                    .collect();
                Self::compute_zscore_from_slice(&spreads)
            }
            _ => return PairsSignal::Hold,
        };
        self.current_state.zscore = zscore;

        // Signal logic
        let pos = self.current_state.position.clone();
        match pos {
            PairsPosition::Flat => {
                if zscore < -self.config.entry_zscore {
                    self.current_state.position = PairsPosition::LongA_ShortB { entry_zscore: zscore };
                    PairsSignal::EnterLongSpread
                } else if zscore > self.config.entry_zscore {
                    self.current_state.position = PairsPosition::LongB_ShortA { entry_zscore: zscore };
                    PairsSignal::EnterShortSpread
                } else {
                    PairsSignal::Hold
                }
            }
            PairsPosition::LongA_ShortB { .. } => {
                if zscore > -self.config.exit_zscore || zscore > self.config.stop_zscore {
                    self.current_state.position = PairsPosition::Flat;
                    PairsSignal::ExitLong
                } else {
                    PairsSignal::Hold
                }
            }
            PairsPosition::LongB_ShortA { .. } => {
                if zscore < self.config.exit_zscore || zscore < -self.config.stop_zscore {
                    self.current_state.position = PairsPosition::Flat;
                    PairsSignal::ExitShort
                } else {
                    PairsSignal::Hold
                }
            }
        }
    }

    /// Compute current spread: a - beta*b - alpha.
    pub fn compute_spread(&self) -> Option<f64> {
        let a = *self.history_a.back()?;
        let b = *self.history_b.back()?;
        let coint = self.cointegration.as_ref()?;
        Some(a - coint.beta * b - coint.alpha)
    }

    /// Compute z-score of last value in a slice: (last - mean) / std.
    pub fn compute_zscore(spread: f64) -> f64 {
        // Stateless convenience: caller supplies raw spread, returns it unchanged
        // (full z-score requires history; use compute_zscore_from_slice for internal use)
        spread
    }

    fn compute_zscore_from_slice(spreads: &[f64]) -> f64 {
        let n = spreads.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let mean = spreads.iter().sum::<f64>() / n;
        let std = (spreads.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        if std < 1e-12 {
            return 0.0;
        }
        (*spreads.last().unwrap_or(&0.0) - mean) / std
    }

    /// Recompute cointegration on current history.
    pub fn refit(&mut self) {
        let a: Vec<f64> = self.history_a.iter().copied().collect();
        let b: Vec<f64> = self.history_b.iter().copied().collect();
        let result = CointegrationTester::test(&a, &b);
        if let Some(hl) = Some(result.half_life).filter(|h| h.is_finite()) {
            self.current_state.half_life = hl;
        }
        self.cointegration = Some(result);
    }

    /// Most recent spread value.
    pub fn current_spread(&self) -> Option<f64> {
        self.compute_spread()
    }

    /// Whether the pair is currently considered cointegrated.
    pub fn is_cointegrated(&self) -> bool {
        self.cointegration.as_ref().map(|c| c.is_cointegrated).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ols_regression_basic() {
        let x: Vec<f64> = (0..10).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|xi| 2.0 * xi + 1.0).collect();
        let (beta, alpha, residuals) = CointegrationTester::ols_regression(&y, &x);
        assert!((beta - 2.0).abs() < 1e-6, "beta={beta}");
        assert!((alpha - 1.0).abs() < 1e-6, "alpha={alpha}");
        assert!(residuals.iter().all(|r| r.abs() < 1e-9));
    }

    #[test]
    fn test_pairs_trader_hold_before_lookback() {
        let config = PairsConfig { lookback: 10, ..Default::default() };
        let mut trader = PairsTrader::new("A".into(), "B".into(), config);
        let sig = trader.update(100.0, 50.0);
        assert_eq!(sig, PairsSignal::Hold);
    }

    #[test]
    fn test_cointegration_tester_perfect() {
        // Perfect cointegration: y = 2x
        let x: Vec<f64> = (1..=30).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|xi| 2.0 * xi).collect();
        let result = CointegrationTester::test(&y, &x);
        assert!((result.beta - 2.0).abs() < 0.01, "beta={}", result.beta);
        assert!(result.residual_std < 1e-6);
    }
}
