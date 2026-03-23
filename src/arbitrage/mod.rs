//! # Module: arbitrage
//!
//! ## Responsibility
//! Real-time arbitrage opportunity detection:
//! 1. **Triangular arbitrage** — crypto triangle (BTC/USD, ETH/USD, ETH/BTC).
//! 2. **Statistical arbitrage** — pairs trading signal using a Kalman filter
//!    spread estimator.
//!
//! ## Outputs
//! Both detectors emit `ArbitrageOpportunity` with spread z-score, expected
//! profit in basis points, and a confidence measure.
//!
//! ## NOT Responsible For
//! - Order execution or routing
//! - Transaction cost modelling (callers should subtract fees from expected_profit_bps)
//! - Multiple-leg statistical arb (pairs only)

use crate::error::StreamError;

// ─── opportunity ─────────────────────────────────────────────────────────────

/// An identified arbitrage opportunity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrageOpportunity {
    /// Human-readable label, e.g. "BTC/USD→ETH/USD→ETH/BTC" or "AAPL/MSFT".
    pub pair: String,
    /// Spread z-score relative to recent history (statistical arb) or imbalance ratio
    /// for triangular arb. Positive = overpriced, negative = underpriced.
    pub spread_zscore: f64,
    /// Estimated profit in basis points (pre-fee).
    pub expected_profit_bps: f64,
    /// Confidence in [0, 1]. Higher = stronger signal.
    pub confidence: f64,
}

// ─── triangular arbitrage ─────────────────────────────────────────────────────

/// Mid-price snapshot for one leg of a triangular arbitrage chain.
#[derive(Debug, Clone, Copy)]
pub struct TriLeg {
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
}

impl TriLeg {
    /// Effective execution price for a buy (pay ask).
    pub fn buy_price(&self) -> f64 {
        self.ask
    }
    /// Effective execution price for a sell (receive bid).
    pub fn sell_price(&self) -> f64 {
        self.bid
    }
    /// Mid-price.
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }
}

/// Triangular arbitrage detector for the standard crypto triangle:
///   BTC/USD, ETH/USD, ETH/BTC.
///
/// Path 1 (forward): USD → buy BTC → sell BTC for ETH → sell ETH for USD.
/// Path 2 (reverse): USD → buy ETH → sell ETH for BTC → sell BTC for USD.
///
/// An opportunity exists when either path yields > 1.0 (profit after round-trip).
pub struct TriangularArbDetector {
    /// Minimum profit in bps to emit an opportunity.
    min_profit_bps: f64,
}

impl TriangularArbDetector {
    /// Create a new triangular arbitrage detector.
    ///
    /// - `min_profit_bps`: minimum round-trip profit threshold in basis points.
    pub fn new(min_profit_bps: f64) -> Self {
        Self { min_profit_bps }
    }

    /// Check for triangular arbitrage given current best bid/ask for each leg.
    ///
    /// - `btc_usd`: BTC/USD best bid & ask.
    /// - `eth_usd`: ETH/USD best bid & ask.
    /// - `eth_btc`: ETH/BTC best bid & ask.
    ///
    /// Returns `Some(opportunity)` if the profit threshold is exceeded.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if any bid or ask is non-positive.
    pub fn check(
        &self,
        btc_usd: TriLeg,
        eth_usd: TriLeg,
        eth_btc: TriLeg,
    ) -> Result<Option<ArbitrageOpportunity>, StreamError> {
        Self::validate_leg(btc_usd, "BTC/USD")?;
        Self::validate_leg(eth_usd, "ETH/USD")?;
        Self::validate_leg(eth_btc, "ETH/BTC")?;

        // Path 1 (forward): USD→BTC→ETH→USD
        // Start with 1 USD:
        //   1. Buy BTC at ask_btc_usd  → 1 / ask_btc_usd BTC
        //   2. Sell BTC for ETH at bid_eth_btc → (1/ask_btc_usd) / ask_eth_btc ETH
        //   3. Sell ETH for USD at bid_eth_usd → (...) * bid_eth_usd USD
        let forward = (1.0 / btc_usd.buy_price()) / eth_btc.buy_price() * eth_usd.sell_price();

        // Path 2 (reverse): USD→ETH→BTC→USD
        //   1. Buy ETH at ask_eth_usd → 1 / ask_eth_usd ETH
        //   2. Sell ETH for BTC at bid_eth_btc → (1/ask_eth_usd) * bid_eth_btc BTC
        //   3. Sell BTC for USD at bid_btc_usd → (...) * bid_btc_usd USD
        let reverse = (1.0 / eth_usd.buy_price()) * eth_btc.sell_price() * btc_usd.sell_price();

        let (profit_ratio, direction) = if forward > reverse {
            (forward, "USD→BTC→ETH→USD")
        } else {
            (reverse, "USD→ETH→BTC→USD")
        };

        let profit_bps = (profit_ratio - 1.0) * 10_000.0;

        if profit_bps < self.min_profit_bps {
            return Ok(None);
        }

        // Spread z-score: normalise profit_ratio relative to break-even (1.0)
        // Use a simple heuristic: profit_ratio - 1.0 is the "spread"
        let spread_zscore = profit_bps / 10.0; // rough: 1 bps = zscore 0.1
        let confidence = (profit_bps / (self.min_profit_bps * 5.0)).clamp(0.0, 1.0);

        Ok(Some(ArbitrageOpportunity {
            pair: direction.to_owned(),
            spread_zscore,
            expected_profit_bps: profit_bps,
            confidence,
        }))
    }

    fn validate_leg(leg: TriLeg, name: &str) -> Result<(), StreamError> {
        if leg.bid <= 0.0 || leg.ask <= 0.0 {
            return Err(StreamError::InvalidInput(format!(
                "Leg {name}: bid and ask must be positive"
            )));
        }
        if leg.bid > leg.ask {
            return Err(StreamError::InvalidInput(format!(
                "Leg {name}: bid {:.6} > ask {:.6} (crossed)", leg.bid, leg.ask
            )));
        }
        Ok(())
    }
}

// ─── statistical arbitrage (Kalman-filter spread) ────────────────────────────

/// Pairs trading statistical arbitrage detector using a 1D Kalman filter
/// to estimate the mean-reverting spread between two asset log-prices.
///
/// Model:
///   spread_t = ln(price_a_t) - β * ln(price_b_t)
///   where β is the cointegration coefficient (caller-supplied or 1.0 for log-ratio).
///
/// The Kalman filter tracks the level of the spread (state) and produces
/// a z-score = (spread - filtered_mean) / filtered_std.
pub struct StatisticalArbDetector {
    /// Label for the pair (e.g. "AAPL/MSFT").
    label: String,
    /// Hedge ratio β.
    beta: f64,
    /// Minimum |z-score| to emit an opportunity.
    z_threshold: f64,

    // Kalman filter state
    /// Filtered mean (μ_t).
    kf_mean: f64,
    /// Filtered variance (P_t).
    kf_var: f64,
    /// Process noise variance Q.
    kf_q: f64,
    /// Observation noise variance R.
    kf_r: f64,
    /// Running estimate of spread standard deviation.
    spread_std: f64,
    /// Number of observations processed.
    n: usize,
    /// Minimum observations before emitting signals.
    warmup: usize,
}

impl StatisticalArbDetector {
    /// Create a new pairs trading detector.
    ///
    /// - `label`: display name for the pair.
    /// - `beta`: hedge ratio (cointegration coefficient).
    /// - `z_threshold`: minimum absolute z-score to emit a signal (e.g. 2.0).
    /// - `warmup`: minimum bars before emitting signals.
    /// - `process_noise`: Kalman Q (state noise); larger → faster mean drift tracking.
    /// - `obs_noise`: Kalman R (observation noise); larger → smoother filter.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if `beta` is zero or `z_threshold` is non-positive.
    pub fn new(
        label: impl Into<String>,
        beta: f64,
        z_threshold: f64,
        warmup: usize,
        process_noise: f64,
        obs_noise: f64,
    ) -> Result<Self, StreamError> {
        if beta == 0.0 {
            return Err(StreamError::InvalidInput("beta must be non-zero".to_owned()));
        }
        if z_threshold <= 0.0 {
            return Err(StreamError::InvalidInput(
                "z_threshold must be positive".to_owned(),
            ));
        }
        Ok(Self {
            label: label.into(),
            beta,
            z_threshold,
            kf_mean: 0.0,
            kf_var: 1.0,
            kf_q: process_noise,
            kf_r: obs_noise,
            spread_std: 1.0,
            n: 0,
            warmup,
        })
    }

    /// Update the detector with current prices for asset A and asset B.
    ///
    /// Returns `Some(opportunity)` if the z-score exceeds the threshold and the
    /// detector has passed its warmup period.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if either price is non-positive.
    pub fn update(
        &mut self,
        price_a: f64,
        price_b: f64,
    ) -> Result<Option<ArbitrageOpportunity>, StreamError> {
        if price_a <= 0.0 || price_b <= 0.0 {
            return Err(StreamError::InvalidInput(
                "Prices must be positive for statistical arb".to_owned(),
            ));
        }

        let spread = price_a.ln() - self.beta * price_b.ln();

        // Kalman filter predict step
        let p_pred = self.kf_var + self.kf_q;

        // Update step
        let innovation = spread - self.kf_mean;
        let s = p_pred + self.kf_r; // innovation covariance
        let k = p_pred / s; // Kalman gain
        self.kf_mean += k * innovation;
        self.kf_var = (1.0 - k) * p_pred;

        // Update spread std using exponential moving variance
        self.n += 1;
        let alpha = 2.0 / (self.warmup as f64 + 1.0);
        let sq_err = innovation * innovation;
        self.spread_std = (alpha * sq_err + (1.0 - alpha) * self.spread_std * self.spread_std).sqrt();

        if self.n < self.warmup || self.spread_std < f64::EPSILON {
            return Ok(None);
        }

        let zscore = (spread - self.kf_mean) / self.spread_std;

        if zscore.abs() < self.z_threshold {
            return Ok(None);
        }

        // Expected profit scales roughly with |zscore| - threshold
        let excess = zscore.abs() - self.z_threshold;
        let expected_profit_bps = excess * 50.0; // heuristic: 1 sigma excess ≈ 50 bps
        let confidence = (excess / self.z_threshold).clamp(0.0, 1.0);

        Ok(Some(ArbitrageOpportunity {
            pair: self.label.clone(),
            spread_zscore: zscore,
            expected_profit_bps,
            confidence,
        }))
    }

    /// Reset Kalman filter state.
    pub fn reset(&mut self) {
        self.kf_mean = 0.0;
        self.kf_var = 1.0;
        self.spread_std = 1.0;
        self.n = 0;
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_triangular_no_arb_at_fair_price() {
        let det = TriangularArbDetector::new(1.0);
        // Fair prices: BTC=50000, ETH=2000, ETH/BTC=0.04
        let result = det
            .check(
                TriLeg { bid: 49999.0, ask: 50001.0 },
                TriLeg { bid: 1999.0, ask: 2001.0 },
                TriLeg { bid: 0.03999, ask: 0.04001 },
            )
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_triangular_detects_obvious_arb() {
        let det = TriangularArbDetector::new(0.1); // very low threshold
        // Skew ETH/BTC price to create arb
        let result = det
            .check(
                TriLeg { bid: 50000.0, ask: 50000.0 },
                TriLeg { bid: 3000.0, ask: 3000.0 }, // ETH/USD unusually high
                TriLeg { bid: 0.04, ask: 0.04 },     // ETH/BTC unchanged
            )
            .unwrap();
        // forward: 1/50000 / 0.04 * 3000 = 1.5 → 5000 bps profit
        assert!(result.is_some());
    }

    #[test]
    fn test_triangular_invalid_leg_errors() {
        let det = TriangularArbDetector::new(1.0);
        let err = det.check(
            TriLeg { bid: -1.0, ask: 50000.0 },
            TriLeg { bid: 2000.0, ask: 2001.0 },
            TriLeg { bid: 0.04, ask: 0.041 },
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_stat_arb_warmup() {
        let mut det = StatisticalArbDetector::new("A/B", 1.0, 2.0, 20, 1e-4, 1e-2).unwrap();
        for _ in 0..19 {
            assert!(det.update(100.0, 100.0).unwrap().is_none());
        }
    }

    #[test]
    fn test_stat_arb_no_signal_at_equilibrium() {
        let mut det = StatisticalArbDetector::new("A/B", 1.0, 2.0, 5, 1e-4, 1e-2).unwrap();
        let mut last = None;
        for _ in 0..50 {
            last = det.update(100.0, 100.0).unwrap();
        }
        // Spread is always 0, z-score will be 0 → no signal
        assert!(last.is_none());
    }

    #[test]
    fn test_stat_arb_invalid_price_errors() {
        let mut det = StatisticalArbDetector::new("A/B", 1.0, 2.0, 5, 1e-4, 1e-2).unwrap();
        assert!(det.update(-1.0, 100.0).is_err());
    }

    #[test]
    fn test_stat_arb_invalid_params() {
        assert!(StatisticalArbDetector::new("A/B", 0.0, 2.0, 5, 1e-4, 1e-2).is_err());
        assert!(StatisticalArbDetector::new("A/B", 1.0, -1.0, 5, 1e-4, 1e-2).is_err());
    }
}
