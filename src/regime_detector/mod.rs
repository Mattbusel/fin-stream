//! Hidden Markov model-based market regime detector.
//!
//! Identifies the current market regime (bull/bear, high/low volatility, crisis)
//! from a stream of return observations using Bayesian state-probability updates.

use std::collections::VecDeque;

/// Market regime classification.
#[derive(Clone, Debug, PartialEq)]
pub enum MarketRegime {
    /// Low volatility bull market.
    LowVolBull,
    /// High volatility bull market.
    HighVolBull,
    /// Low volatility bear market.
    LowVolBear,
    /// High volatility bear market.
    HighVolBear,
    /// Crisis regime: extreme volatility and negative returns.
    Crisis,
}

/// Rolling feature extractor for regime inference.
pub struct RegimeFeatures {
    /// Circular buffer of recent return values.
    pub returns: VecDeque<f64>,
    /// Circular buffer of recent per-period volatility estimates.
    pub volatility: VecDeque<f64>,
    /// Circular buffer of trend (EMA of returns) values.
    pub trend: VecDeque<f64>,
    /// Maximum window length for all buffers.
    pub max_window: usize,
}

impl RegimeFeatures {
    /// Create a new `RegimeFeatures` with the given lookback window.
    pub fn new(max_window: usize) -> Self {
        RegimeFeatures {
            returns: VecDeque::with_capacity(max_window),
            volatility: VecDeque::with_capacity(max_window),
            trend: VecDeque::with_capacity(max_window),
            max_window,
        }
    }

    /// Add a new return observation and update rolling buffers.
    pub fn add(&mut self, ret: f64) {
        if self.returns.len() == self.max_window {
            self.returns.pop_front();
        }
        self.returns.push_back(ret);

        let vol = self.current_vol();
        if self.volatility.len() == self.max_window {
            self.volatility.pop_front();
        }
        self.volatility.push_back(vol);

        let trend = self.current_trend();
        if self.trend.len() == self.max_window {
            self.trend.pop_front();
        }
        self.trend.push_back(trend);
    }

    /// EWMA standard deviation of returns (λ = 0.94).
    pub fn current_vol(&self) -> f64 {
        if self.returns.is_empty() {
            return 0.0;
        }
        let lambda = 0.94_f64;
        let mut var = 0.0_f64;
        let mut weight = 1.0_f64;
        let mut total_weight = 0.0_f64;
        for &r in self.returns.iter().rev() {
            var += weight * r * r;
            total_weight += weight;
            weight *= lambda;
        }
        if total_weight > 0.0 {
            (var / total_weight).sqrt()
        } else {
            0.0
        }
    }

    /// EMA of returns (λ = 0.9).
    pub fn current_trend(&self) -> f64 {
        if self.returns.is_empty() {
            return 0.0;
        }
        let lambda = 0.9_f64;
        let mut ema = 0.0_f64;
        let mut weight = 1.0_f64;
        let mut total_weight = 0.0_f64;
        for &r in self.returns.iter().rev() {
            ema += weight * r;
            total_weight += weight;
            weight *= lambda;
        }
        if total_weight > 0.0 {
            ema / total_weight
        } else {
            0.0
        }
    }

    /// Three-dimensional feature vector: [latest_return, current_vol, current_trend].
    pub fn feature_vector(&self) -> [f64; 3] {
        let ret = self.returns.back().copied().unwrap_or(0.0);
        [ret, self.current_vol(), self.current_trend()]
    }
}

/// HMM state definition.
pub struct HmmState {
    /// Expected return in this regime.
    pub mean_return: f64,
    /// Typical volatility level for this regime.
    pub vol: f64,
    /// Regime label.
    pub regime: MarketRegime,
}

/// Real-time market regime detector using a 5-state HMM with Bayes updates.
pub struct RegimeDetector {
    /// Prior state definitions.
    pub states: Vec<HmmState>,
    /// Transition probability matrix (states × states).
    pub transition_matrix: Vec<Vec<f64>>,
    /// Current posterior probability for each state.
    pub state_probs: Vec<f64>,
    /// Feature extractor.
    pub features: RegimeFeatures,
    current_regime_idx: usize,
    regime_period_count: usize,
}

impl RegimeDetector {
    /// Construct a detector with reasonable default priors.
    pub fn new() -> Self {
        let states = vec![
            HmmState { mean_return:  0.0005, vol: 0.008, regime: MarketRegime::LowVolBull  },
            HmmState { mean_return:  0.0010, vol: 0.020, regime: MarketRegime::HighVolBull },
            HmmState { mean_return: -0.0003, vol: 0.007, regime: MarketRegime::LowVolBear  },
            HmmState { mean_return: -0.0010, vol: 0.022, regime: MarketRegime::HighVolBear },
            HmmState { mean_return: -0.0030, vol: 0.040, regime: MarketRegime::Crisis      },
        ];

        // Transition matrix — high self-transition probability (persistence)
        let transition_matrix = vec![
            vec![0.92, 0.04, 0.02, 0.01, 0.01],
            vec![0.05, 0.88, 0.02, 0.04, 0.01],
            vec![0.03, 0.02, 0.90, 0.04, 0.01],
            vec![0.01, 0.03, 0.05, 0.86, 0.05],
            vec![0.01, 0.01, 0.02, 0.06, 0.90],
        ];

        // Uniform initial prior
        let n = states.len();
        let state_probs = vec![1.0 / n as f64; n];

        RegimeDetector {
            states,
            transition_matrix,
            state_probs,
            features: RegimeFeatures::new(60),
            current_regime_idx: 0,
            regime_period_count: 1,
        }
    }

    /// Update the regime detector with a new return observation.
    pub fn update(&mut self, return_val: f64) {
        self.features.add(return_val);
        let [ret, vol, _trend] = self.features.feature_vector();

        // Transition: mix state probs through transition matrix
        let n = self.states.len();
        let mut transitioned = vec![0.0_f64; n];
        for j in 0..n {
            for i in 0..n {
                transitioned[j] += self.state_probs[i] * self.transition_matrix[i][j];
            }
        }

        // Likelihood: Gaussian(return | mean_i, vol_i)
        let mut posterior = vec![0.0_f64; n];
        for (j, state) in self.states.iter().enumerate() {
            let sigma = state.vol.max(1e-10);
            let diff = ret - state.mean_return;
            let likelihood = (-0.5 * (diff / sigma).powi(2)).exp() / sigma;
            posterior[j] = transitioned[j] * likelihood;
        }

        // Normalize
        let sum: f64 = posterior.iter().sum();
        if sum > 0.0 {
            for p in posterior.iter_mut() {
                *p /= sum;
            }
        } else {
            let uniform = 1.0 / n as f64;
            posterior.fill(uniform);
        }
        self.state_probs = posterior;

        // Track current regime
        let new_idx = self
            .state_probs
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i)
            .unwrap_or(0);

        let _ = vol; // suppress unused warning

        if new_idx == self.current_regime_idx {
            self.regime_period_count += 1;
        } else {
            self.current_regime_idx = new_idx;
            self.regime_period_count = 1;
        }
    }

    /// Return the most probable current regime.
    pub fn current_regime(&self) -> &MarketRegime {
        &self.states[self.current_regime_idx].regime
    }

    /// Confidence in the current regime (maximum state probability).
    pub fn confidence(&self) -> f64 {
        self.state_probs[self.current_regime_idx]
    }

    /// Number of consecutive periods in the current regime.
    pub fn regime_duration(&self) -> usize {
        self.regime_period_count
    }
}

impl Default for RegimeDetector {
    fn default() -> Self {
        Self::new()
    }
}
