//! Predictive order book — short-term direction forecasting.
//!
//! ## Responsibility
//!
//! Use the current L2 order book snapshot together with recent trade flow to
//! predict the next-tick price direction (up / flat / down) using an
//! **online logistic regression** trained on three features:
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `imbalance` | `(bid_depth - ask_depth) / (bid_depth + ask_depth)` ∈ [-1, 1] |
//! | `spread` | `(ask_best - bid_best) / mid_price` |
//! | `depth_ratio` | `total_bid_depth / total_ask_depth` |
//!
//! The model is updated with stochastic gradient descent each time a new
//! trade tick arrives (the label is the sign of the subsequent price move).
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or `Option`.
//! - No heap allocation on the hot-path update step.
//! - Stateless construction: weights are initialised to zero and converge
//!   online from the data stream.

use crate::error::StreamError;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

// ── Feature vector (3-dimensional) ───────────────────────────────────────────

/// Features extracted from an L2 snapshot + recent trade flow.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BookFeatures {
    /// Order-book imbalance: `(bid_depth - ask_depth) / (bid_depth + ask_depth)`.
    pub imbalance: f64,
    /// Relative bid-ask spread: `(ask - bid) / mid`.
    pub spread: f64,
    /// Depth ratio: `total_bid_depth / total_ask_depth`.
    pub depth_ratio: f64,
}

impl BookFeatures {
    /// Compute features from raw bid/ask depth totals and best prices.
    ///
    /// Returns `None` when the inputs are degenerate (zero total depth, zero
    /// mid-price, or a crossed book).
    pub fn compute(
        total_bid_depth: Decimal,
        total_ask_depth: Decimal,
        best_bid: Decimal,
        best_ask: Decimal,
    ) -> Option<Self> {
        let bid_f = total_bid_depth.to_f64()?;
        let ask_f = total_ask_depth.to_f64()?;
        let total = bid_f + ask_f;
        if total < 1e-12 || bid_f < 0.0 || ask_f < 0.0 {
            return None;
        }
        let mid = ((best_bid + best_ask) / Decimal::from(2)).to_f64()?;
        if mid <= 0.0 {
            return None;
        }
        let spread_dec = (best_ask - best_bid).to_f64()? / mid;
        let depth_ratio = if ask_f > 1e-12 { bid_f / ask_f } else { f64::MAX };

        Some(Self {
            imbalance: (bid_f - ask_f) / total,
            spread: spread_dec,
            depth_ratio: depth_ratio.min(100.0), // cap to avoid numerical issues
        })
    }

    /// Return the feature vector as `[imbalance, spread, depth_ratio]`.
    #[inline]
    pub fn as_array(&self) -> [f64; 3] {
        [self.imbalance, self.spread, self.depth_ratio]
    }
}

// ── Prediction ────────────────────────────────────────────────────────────────

/// Predicted next-tick direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TickDirection {
    /// Price expected to move up.
    Up,
    /// Price expected to remain flat (within noise threshold).
    Flat,
    /// Price expected to move down.
    Down,
}

impl std::fmt::Display for TickDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TickDirection::Up => write!(f, "Up"),
            TickDirection::Flat => write!(f, "Flat"),
            TickDirection::Down => write!(f, "Down"),
        }
    }
}

/// The output of one prediction cycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Prediction {
    /// Predicted direction.
    pub direction: TickDirection,
    /// Logistic probability that price goes up, in (0, 1).
    pub prob_up: f64,
    /// Features used to generate this prediction.
    pub features: BookFeatures,
}

// ── Online logistic regression ────────────────────────────────────────────────

/// Configuration for the predictive order book model.
#[derive(Debug, Clone)]
pub struct PredictiveBookConfig {
    /// SGD learning rate η.
    pub learning_rate: f64,

    /// L2 regularisation coefficient λ (weight decay).
    pub l2_lambda: f64,

    /// Minimum number of labelled samples required before predictions are
    /// considered reliable.
    pub min_samples: usize,

    /// Threshold on `|log_return|` above which a tick is labelled Up or Down
    /// (returns smaller than this in absolute value are labelled Flat / ignored
    /// for SGD since the label is ambiguous).
    pub flat_threshold: f64,
}

impl Default for PredictiveBookConfig {
    fn default() -> Self {
        Self {
            learning_rate: 0.01,
            l2_lambda: 0.001,
            min_samples: 30,
            flat_threshold: 1e-5,
        }
    }
}

/// Online logistic regression model trained on order-book features.
///
/// The model maintains a weight vector `w ∈ ℝ³` and bias `b ∈ ℝ`.
/// The predicted probability is `σ(w·x + b)` where `σ` is the logistic
/// sigmoid.  The label `y ∈ {0, 1}` encodes Down → 0, Up → 1.
///
/// The SGD update on each labelled sample is:
///
/// ```text
/// w ← w - η·[(σ(w·x+b) - y)·x + λ·w]
/// b ← b - η·(σ(w·x+b) - y)
/// ```
#[derive(Debug, Clone)]
pub struct PredictiveOrderBook {
    cfg: PredictiveBookConfig,
    /// Weight vector [w_imbalance, w_spread, w_depth_ratio].
    weights: [f64; 3],
    /// Bias term.
    bias: f64,
    /// Number of labelled training samples processed.
    sample_count: usize,
    /// Last price seen (used to compute direction label for the previous tick).
    last_price: Option<f64>,
    /// Features from the previous tick (paired with the current tick's label).
    pending_features: Option<BookFeatures>,
}

impl PredictiveOrderBook {
    /// Create a new model with zero-initialised weights.
    pub fn new(cfg: PredictiveBookConfig) -> Self {
        Self {
            cfg,
            weights: [0.0; 3],
            bias: 0.0,
            sample_count: 0,
            last_price: None,
            pending_features: None,
        }
    }

    /// Ingest a new order-book snapshot and the current mid-price.
    ///
    /// - If there is a pending training sample (features from the previous
    ///   tick + the current tick's price as label), the model is updated.
    /// - Returns a [`Prediction`] when there are enough samples, else `None`.
    pub fn update(
        &mut self,
        features: BookFeatures,
        current_mid: f64,
    ) -> Result<Option<Prediction>, StreamError> {
        // Label the previous tick's features using the current price move.
        if let (Some(prev_price), Some(prev_features)) =
            (self.last_price, self.pending_features.take())
        {
            let log_return = if prev_price > 1e-12 {
                (current_mid / prev_price).ln()
            } else {
                0.0
            };

            if log_return.abs() >= self.cfg.flat_threshold {
                let label = if log_return > 0.0 { 1.0_f64 } else { 0.0_f64 };
                self.sgd_step(&prev_features.as_array(), label);
                self.sample_count += 1;
            }
        }

        self.last_price = Some(current_mid);
        self.pending_features = Some(features.clone());

        if self.sample_count < self.cfg.min_samples {
            return Ok(None);
        }

        let prob_up = sigmoid(dot(&self.weights, &features.as_array()) + self.bias);
        let direction = if prob_up > 0.55 {
            TickDirection::Up
        } else if prob_up < 0.45 {
            TickDirection::Down
        } else {
            TickDirection::Flat
        };

        Ok(Some(Prediction {
            direction,
            prob_up,
            features,
        }))
    }

    /// Current model weights (for inspection / serialisation).
    pub fn weights(&self) -> [f64; 3] {
        self.weights
    }

    /// Current bias term.
    pub fn bias(&self) -> f64 {
        self.bias
    }

    /// Number of labelled samples used to train the model.
    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    // ── private ───────────────────────────────────────────────────────────────

    fn sgd_step(&mut self, x: &[f64; 3], y: f64) {
        let logit = dot(&self.weights, x) + self.bias;
        let pred = sigmoid(logit);
        let error = pred - y;
        let lr = self.cfg.learning_rate;
        let lam = self.cfg.l2_lambda;
        for i in 0..3 {
            self.weights[i] -= lr * (error * x[i] + lam * self.weights[i]);
        }
        self.bias -= lr * error;
    }
}

#[inline]
fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

#[inline]
fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn book_features_compute_valid() {
        let f = BookFeatures::compute(dec!(100), dec!(80), dec!(99.9), dec!(100.1));
        assert!(f.is_some());
        let f = f.unwrap();
        // Bid-heavy book → positive imbalance.
        assert!(f.imbalance > 0.0, "imbalance should be positive for bid-heavy book");
    }

    #[test]
    fn book_features_none_for_zero_depth() {
        let f = BookFeatures::compute(dec!(0), dec!(0), dec!(100), dec!(101));
        assert!(f.is_none(), "should return None for zero total depth");
    }

    #[test]
    fn model_returns_none_before_min_samples() {
        let cfg = PredictiveBookConfig { min_samples: 100, ..Default::default() };
        let mut model = PredictiveOrderBook::new(cfg);
        let f = BookFeatures { imbalance: 0.1, spread: 0.001, depth_ratio: 1.1 };
        let res = model.update(f, 100.0).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn model_trains_and_produces_prediction() {
        let cfg = PredictiveBookConfig { min_samples: 5, ..Default::default() };
        let mut model = PredictiveOrderBook::new(cfg);
        // Simulate alternating up/down with a bullish imbalance signal.
        let mut price = 100.0_f64;
        for i in 0..30 {
            let f = BookFeatures { imbalance: 0.3, spread: 0.001, depth_ratio: 1.5 };
            price += if i % 2 == 0 { 0.01 } else { 0.005 };
            let _ = model.update(f, price);
        }
        // After 30 samples the model should produce a prediction.
        let f = BookFeatures { imbalance: 0.3, spread: 0.001, depth_ratio: 1.5 };
        let pred = model.update(f, price + 0.01).unwrap();
        assert!(pred.is_some(), "expected prediction after sufficient training");
    }

    #[test]
    fn prob_up_in_unit_interval() {
        let cfg = PredictiveBookConfig { min_samples: 2, ..Default::default() };
        let mut model = PredictiveOrderBook::new(cfg);
        let mut price = 100.0_f64;
        for _ in 0..10 {
            let f = BookFeatures { imbalance: 0.5, spread: 0.001, depth_ratio: 2.0 };
            price += 0.01;
            if let Ok(Some(pred)) = model.update(f, price) {
                assert!(pred.prob_up > 0.0 && pred.prob_up < 1.0);
            }
        }
    }
}
