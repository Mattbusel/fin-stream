//! Volatility forecasting: GARCH(1,1), EGARCH, RealizedVol

use std::collections::VecDeque;

/// GARCH(1,1) model: sigma_t^2 = omega + alpha * eps_{t-1}^2 + beta * sigma_{t-1}^2
pub struct Garch11 {
    /// Constant term (long-run variance contribution)
    pub omega: f64,
    /// ARCH coefficient (shock sensitivity)
    pub alpha: f64,
    /// GARCH coefficient (variance persistence)
    pub beta: f64,
    /// Seed variance for the filter
    pub initial_variance: f64,
}

impl Garch11 {
    /// Construct a new GARCH(1,1) model.
    pub fn new(omega: f64, alpha: f64, beta: f64, initial_variance: f64) -> Self {
        Garch11 { omega, alpha, beta, initial_variance }
    }

    /// Returns true when alpha + beta < 1 (covariance-stationary process).
    pub fn is_stationary(&self) -> bool {
        self.alpha + self.beta < 1.0
    }

    /// Unconditional (long-run) variance: omega / (1 - alpha - beta).
    pub fn long_run_variance(&self) -> f64 {
        self.omega / (1.0 - self.alpha - self.beta)
    }

    /// Filter a return series to produce the conditional variance series.
    pub fn filter(&self, returns: &[f64]) -> Vec<f64> {
        let mut variances = Vec::with_capacity(returns.len());
        let mut var = self.initial_variance;
        variances.push(var);
        for &r in returns {
            var = self.omega + self.alpha * r * r + self.beta * var;
            var = var.max(1e-10);
            variances.push(var);
        }
        variances
    }

    /// Produce an h-step-ahead variance forecast starting from the last observed state.
    pub fn forecast(&self, last_variance: f64, last_return: f64, h: u32) -> Vec<f64> {
        let lr_var = self.long_run_variance();
        let mut forecasts = Vec::new();
        let mut v = self.omega + self.alpha * last_return * last_return + self.beta * last_variance;
        forecasts.push(v);
        let persistence = self.alpha + self.beta;
        for step in 1..h {
            v = lr_var + persistence.powi(step as i32) * (v - lr_var);
            forecasts.push(v);
        }
        forecasts
    }

    /// Estimate GARCH(1,1) parameters by quasi-MLE grid search over a coarse parameter grid.
    pub fn fit(returns: &[f64], initial_variance: f64) -> Self {
        let best = (0..5).flat_map(|ai| (0..5).flat_map(move |bi| (0..5).map(move |oi| (ai, bi, oi))))
            .filter_map(|(ai, bi, oi)| {
                let alpha = 0.05 + ai as f64 * 0.05;
                let beta = 0.70 + bi as f64 * 0.05;
                let omega = 0.000001 + oi as f64 * 0.000001;
                if alpha + beta >= 1.0 { return None; }
                let g = Garch11::new(omega, alpha, beta, initial_variance);
                let vars = g.filter(returns);
                let ll: f64 = returns.iter().zip(vars.iter()).map(|(&r, &v)| {
                    -0.5 * (v.ln() + r * r / v)
                }).sum();
                Some((ll, alpha, beta, omega))
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((_, alpha, beta, omega)) = best {
            Garch11::new(omega, alpha, beta, initial_variance)
        } else {
            Garch11::new(0.000002, 0.10, 0.85, initial_variance)
        }
    }
}

/// EGARCH model: log(sigma_t^2) = omega + alpha*(|z_{t-1}| - E|z|) + gamma*z_{t-1} + beta*log(sigma_{t-1}^2)
///
/// The gamma term captures the leverage effect (negative returns raise volatility more).
pub struct Egarch {
    /// Constant term in the log-variance equation
    pub omega: f64,
    /// Magnitude coefficient
    pub alpha: f64,
    /// Leverage (asymmetry) coefficient
    pub gamma: f64,
    /// Persistence coefficient
    pub beta: f64,
}

impl Egarch {
    /// Construct a new EGARCH model.
    pub fn new(omega: f64, alpha: f64, gamma: f64, beta: f64) -> Self {
        Egarch { omega, alpha, gamma, beta }
    }

    /// E[|z|] for a standard normal: sqrt(2/pi)
    const EXPECTED_ABS_Z: f64 = 0.7978845608;

    /// Filter a return series to produce conditional variance values.
    pub fn filter(&self, returns: &[f64]) -> Vec<f64> {
        let mut log_vars = Vec::with_capacity(returns.len() + 1);
        let init_var = returns.iter().map(|r| r * r).sum::<f64>() / returns.len().max(1) as f64;
        let mut log_var = init_var.max(1e-10).ln();
        log_vars.push(log_var.exp());
        for &r in returns {
            let sigma = log_var.exp().max(1e-10).sqrt();
            let z = r / sigma;
            log_var = self.omega + self.alpha * (z.abs() - Self::EXPECTED_ABS_Z) + self.gamma * z + self.beta * log_var;
            log_vars.push(log_var.exp().max(1e-20));
        }
        log_vars
    }

    /// News-impact curve: contribution of a standardised shock z to log-variance.
    pub fn news_impact(&self, z: f64) -> f64 {
        self.omega + self.alpha * (z.abs() - Self::EXPECTED_ABS_Z) + self.gamma * z
    }
}

/// Realized volatility estimator computed from a rolling window of high-frequency returns.
pub struct RealizedVolEstimator {
    /// Rolling window length (number of return observations)
    pub window: usize,
    buffer: VecDeque<f64>,
    /// Whether to annualize the realized variance estimate
    pub annualize: bool,
    /// Number of return periods per calendar year (e.g. 252 for daily, 252*78 for 5-min)
    pub periods_per_year: f64,
}

impl RealizedVolEstimator {
    /// Construct a new estimator with a given window and annualisation factor.
    pub fn new(window: usize, periods_per_year: f64) -> Self {
        RealizedVolEstimator { window, buffer: VecDeque::new(), annualize: true, periods_per_year }
    }

    /// Push a new return observation into the rolling buffer.
    pub fn add_return(&mut self, r: f64) {
        self.buffer.push_back(r);
        if self.buffer.len() > self.window {
            self.buffer.pop_front();
        }
    }

    /// Realized variance: sum of squared returns (optionally annualized).
    pub fn realized_variance(&self) -> f64 {
        let rv: f64 = self.buffer.iter().map(|r| r * r).sum();
        if self.annualize {
            rv * self.periods_per_year / self.buffer.len().max(1) as f64
        } else {
            rv
        }
    }

    /// Realized volatility (square root of realized variance).
    pub fn realized_vol(&self) -> f64 {
        self.realized_variance().sqrt()
    }

    /// Bipower variation (BPV): jump-robust variance estimator.
    ///
    /// BV = (pi/2) * sum |r_t| * |r_{t-1}|
    pub fn bipower_variation(&self) -> f64 {
        let buf: Vec<f64> = self.buffer.iter().copied().collect();
        if buf.len() < 2 { return 0.0; }
        let bv: f64 = buf.windows(2).map(|w| w[0].abs() * w[1].abs()).sum::<f64>() * (std::f64::consts::PI / 2.0);
        if self.annualize { bv * self.periods_per_year / (buf.len() - 1).max(1) as f64 } else { bv }
    }

    /// Jump component of realized variance: max(RV - BPV, 0).
    pub fn jump_component(&self) -> f64 {
        (self.realized_variance() - self.bipower_variation()).max(0.0)
    }
}
