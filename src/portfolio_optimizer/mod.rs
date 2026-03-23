//! Portfolio optimization with market impact

/// Compute the sample covariance matrix from a matrix of return series.
///
/// `returns[i]` is the time series for asset i.
pub fn cov_matrix(returns: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = returns.len();
    let t = returns[0].len();
    let means: Vec<f64> = returns.iter().map(|r| r.iter().sum::<f64>() / t as f64).collect();
    let mut cov = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..n {
            let c: f64 = (0..t).map(|k| (returns[i][k] - means[i]) * (returns[j][k] - means[j])).sum::<f64>() / (t - 1).max(1) as f64;
            cov[i][j] = c;
        }
    }
    cov
}

/// Compute portfolio variance: w^T * Sigma * w.
pub fn portfolio_variance(weights: &[f64], cov: &[Vec<f64>]) -> f64 {
    let n = weights.len();
    let mut var = 0.0;
    for i in 0..n {
        for j in 0..n {
            var += weights[i] * cov[i][j] * weights[j];
        }
    }
    var
}

/// Compute marginal risk contributions: RC_i = w_i * (Sigma*w)_i / sigma_p.
pub fn risk_contributions(weights: &[f64], cov: &[Vec<f64>]) -> Vec<f64> {
    let n = weights.len();
    let sigma_p = portfolio_variance(weights, cov).max(1e-20).sqrt();
    let mut rc = vec![0.0; n];
    for i in 0..n {
        let mut sigma_w_i = 0.0;
        for j in 0..n { sigma_w_i += cov[i][j] * weights[j]; }
        rc[i] = weights[i] * sigma_w_i / sigma_p;
    }
    rc
}

/// Project a weight vector onto the probability simplex (sum = 1, w_i >= 0).
pub fn project_simplex(w: &[f64]) -> Vec<f64> {
    let n = w.len();
    let mut u: Vec<f64> = w.to_vec();
    u.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let cssv: Vec<f64> = u.iter().scan(0.0, |s, &x| { *s += x; Some(*s) }).collect();
    let rho = (0..n).filter(|&j| u[j] - (cssv[j] - 1.0) / (j + 1) as f64 > 0.0).last().unwrap_or(0);
    let theta = (cssv[rho] - 1.0) / (rho + 1) as f64;
    w.iter().map(|&x| (x - theta).max(0.0)).collect()
}

/// Mean-variance portfolio optimizer using projected gradient ascent on the Sharpe objective.
pub struct MeanVarianceOptimizer {
    /// Quadratic risk-aversion parameter (lambda in: max E[r] - lambda*Var)
    pub risk_aversion: f64,
    /// Gradient step size
    pub lr: f64,
    /// Number of gradient iterations
    pub n_iters: u32,
}

impl MeanVarianceOptimizer {
    /// Optimize portfolio weights given expected returns and a covariance matrix.
    pub fn optimize(&self, expected_returns: &[f64], cov: &[Vec<f64>], _seed: u64) -> Vec<f64> {
        let n = expected_returns.len();
        let mut w = vec![1.0 / n as f64; n];

        for _ in 0..self.n_iters {
            let mut grad = vec![0.0; n];
            for i in 0..n {
                let mut sigma_w_i = 0.0;
                for j in 0..n { sigma_w_i += cov[i][j] * w[j]; }
                grad[i] = expected_returns[i] - self.risk_aversion * 2.0 * sigma_w_i;
            }

            let new_w: Vec<f64> = w.iter().zip(grad.iter()).map(|(&wi, &gi)| wi + self.lr * gi).collect();
            w = project_simplex(&new_w);
        }
        w
    }
}

/// Risk-parity portfolio optimizer: iteratively rescales weights to equalize risk contributions.
pub struct RiskParityOptimizer {
    /// Maximum number of rescaling iterations
    pub n_iters: u32,
    /// Convergence tolerance on weight change
    pub tol: f64,
}

impl RiskParityOptimizer {
    /// Compute risk-parity weights for the given covariance matrix.
    pub fn optimize(&self, cov: &[Vec<f64>]) -> Vec<f64> {
        let n = cov.len();
        let mut w = vec![1.0 / n as f64; n];
        let target = 1.0 / n as f64;

        for _ in 0..self.n_iters {
            let rc = risk_contributions(&w, cov);
            let total_rc: f64 = rc.iter().sum();
            let mut max_diff = 0.0f64;
            let mut new_w = w.clone();
            for i in 0..n {
                if rc[i] > 1e-10 {
                    let scale = target * total_rc / rc[i];
                    new_w[i] = (w[i] * scale).max(1e-6);
                    max_diff = max_diff.max((new_w[i] - w[i]).abs());
                }
            }
            let sum: f64 = new_w.iter().sum();
            w = new_w.iter().map(|&x| x / sum).collect();
            if max_diff < self.tol { break; }
        }
        w
    }
}

/// Market impact-aware execution optimizer using the Almgren-Chriss (2001) framework.
pub struct ExecutionOptimizer {
    /// Trader risk-aversion coefficient
    pub risk_aversion: f64,
    /// Per-period price volatility
    pub volatility: f64,
    /// Temporary (instantaneous) market-impact coefficient
    pub temp_impact: f64,
    /// Permanent market-impact coefficient
    pub perm_impact: f64,
    /// Total number of shares to liquidate
    pub total_shares: f64,
    /// Number of discrete trading periods
    pub trading_periods: u32,
}

impl ExecutionOptimizer {
    /// Compute the optimal trade schedule (shares per period) under Almgren-Chriss.
    pub fn optimal_schedule(&self) -> Vec<f64> {
        let n = self.trading_periods as usize;
        let eta = self.temp_impact;
        let sigma = self.volatility;
        let lambda = self.risk_aversion;

        let kappa = (lambda * sigma * sigma / eta).sqrt().max(0.001);

        let mut trades = Vec::with_capacity(n);
        let mut remaining = self.total_shares;

        for j in 0..n {
            let t = j as f64 / n as f64;
            let trade = remaining * (kappa * (1.0 - t) / (kappa * (1.0 - t) + 1.0)).min(1.0);
            let trade = trade.max(0.0);
            trades.push(trade);
            remaining -= trade;
        }
        if !trades.is_empty() { *trades.last_mut().unwrap() += remaining; }
        trades
    }

    /// Estimate implementation shortfall for a given trade schedule and price path.
    pub fn implementation_shortfall(&self, schedule: &[f64], price_path: &[f64]) -> f64 {
        let arrival_price = price_path[0];
        let mut is_cost = 0.0;
        for (_i, (&trade, &price)) in schedule.iter().zip(price_path.iter()).enumerate() {
            let impact = self.temp_impact * trade + self.perm_impact * trade;
            is_cost += trade * (price + impact - arrival_price);
        }
        is_cost
    }
}
