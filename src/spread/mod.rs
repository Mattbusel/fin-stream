//! # Module: spread
//!
//! ## Responsibility
//! Bid-ask spread estimation from trade data using microstructure models.
//!
//! ## Models
//!
//! | Type | Description |
//! |------|-------------|
//! | [`RollSpreadEstimator`] | Roll (1984): covariance-based spread from price changes |
//! | [`CorwinSchultzSpread`] | Corwin-Schultz (2012): high-low price range estimator |
//! | [`SpreadAnalyzer`] | Rolling window combining Roll's model with effective/realized spread |
//!
//! ## Guarantees
//! - All estimates return `None` when inputs are insufficient or degenerate.
//! - No panics on empty or constant series.

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// TradeData
// ---------------------------------------------------------------------------

/// A single trade observation for spread estimation.
#[derive(Debug, Clone, Copy)]
pub struct TradeData {
    /// Trade price.
    pub price: f64,
    /// Trade volume (used for volume-weighting if desired).
    pub volume: f64,
    /// Milliseconds since Unix epoch when the trade occurred.
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// Roll (1984) Spread Estimator
// ---------------------------------------------------------------------------

/// Roll's (1984) implied spread estimator.
///
/// Estimates the effective bid-ask spread from the serial covariance of
/// successive price changes:
///
/// ```text
/// spread = 2 * sqrt(max(0, -Cov(ΔP_t, ΔP_{t-1})))
/// ```
///
/// A negative serial covariance is the microstructure signature of bid-ask
/// bounce: prices alternate between bid and ask, creating mean-reversion
/// in successive price changes.
pub struct RollSpreadEstimator;

impl RollSpreadEstimator {
    /// Estimate the Roll implied spread from a trade sequence.
    ///
    /// # Returns
    /// - `Some(spread)` if at least 3 trades are provided and the serial
    ///   covariance of price changes is negative.
    /// - `None` if there are fewer than 3 trades, all prices are identical,
    ///   or the covariance is non-negative (no bid-ask bounce signal).
    pub fn estimate(trades: &[TradeData]) -> Option<f64> {
        if trades.len() < 3 {
            return None;
        }

        // Compute first differences ΔP_t = P_t - P_{t-1}.
        let diffs: Vec<f64> = trades
            .windows(2)
            .map(|w| w[1].price - w[0].price)
            .collect();

        // Compute serial covariance: Cov(ΔP_t, ΔP_{t-1}) over lagged pairs.
        // We need at least 2 differences → at least 3 trades.
        let n = diffs.len() as f64; // at least 2
        let mean1: f64 = diffs[..diffs.len() - 1].iter().sum::<f64>() / (n - 1.0);
        let mean2: f64 = diffs[1..].iter().sum::<f64>() / (n - 1.0);

        let cov: f64 = diffs[..diffs.len() - 1]
            .iter()
            .zip(diffs[1..].iter())
            .map(|(&a, &b)| (a - mean1) * (b - mean2))
            .sum::<f64>()
            / (n - 1.0);

        // Roll's formula only applies when covariance is negative.
        if cov >= 0.0 {
            return None;
        }

        Some(2.0 * (-cov).sqrt())
    }
}

// ---------------------------------------------------------------------------
// Corwin-Schultz (2012) High-Low Spread Estimator
// ---------------------------------------------------------------------------

/// Corwin-Schultz (2012) high-low spread estimator.
///
/// Estimates the effective spread from daily high and low prices using the
/// insight that price ranges over single and two-period windows reflect both
/// volatility and the bid-ask spread.
///
/// The estimator computes:
/// ```text
/// β = Σ [ln(H_t/L_t)]^2 (sum over two consecutive single-period ranges)
/// γ = [ln(H_{t,t+1}/L_{t,t+1})]^2 (two-period combined range)
/// α = (√(2β) - √β) / (3 - 2√2) - √(γ / (3 - 2√2))
/// spread = 2*(exp(α) - 1) / (1 + exp(α))
/// ```
///
/// Negative spread estimates are clamped to zero.
pub struct CorwinSchultzSpread;

impl CorwinSchultzSpread {
    /// Estimate the Corwin-Schultz spread from high and low price series.
    ///
    /// `high` and `low` must have the same length and at least 2 observations.
    ///
    /// # Returns
    /// - `Some(spread)` on success; clamped to `[0, ∞)`.
    /// - `None` if inputs are mismatched, too short, or contain invalid values
    ///   (non-positive prices, `H < L`).
    pub fn estimate(high: &[f64], low: &[f64]) -> Option<f64> {
        if high.len() != low.len() || high.len() < 2 {
            return None;
        }

        // Validate: H >= L > 0 for all observations.
        for (&h, &l) in high.iter().zip(low.iter()) {
            if l <= 0.0 || h < l || !h.is_finite() || !l.is_finite() {
                return None;
            }
        }

        let n_pairs = high.len() - 1;
        if n_pairs == 0 {
            return None;
        }

        let mut beta_sum = 0.0_f64;
        let mut gamma_sum = 0.0_f64;

        for i in 0..n_pairs {
            let ln_hl_t = (high[i] / low[i]).ln();
            let ln_hl_t1 = (high[i + 1] / low[i + 1]).ln();

            // β contribution: sum of squared single-period log-ratios.
            beta_sum += ln_hl_t * ln_hl_t + ln_hl_t1 * ln_hl_t1;

            // Two-period combined range.
            let h2 = high[i].max(high[i + 1]);
            let l2 = low[i].min(low[i + 1]);
            let ln_hl2 = (h2 / l2).ln();
            gamma_sum += ln_hl2 * ln_hl2;
        }

        let beta = beta_sum / n_pairs as f64;
        let gamma = gamma_sum / n_pairs as f64;

        const K: f64 = 3.0 - 2.0 * std::f64::consts::SQRT_2; // 3 - 2√2 ≈ 0.17157

        let alpha = ((2.0 * beta).sqrt() - beta.sqrt()) / K - (gamma / K).sqrt();

        let exp_alpha = alpha.exp();
        let spread = 2.0 * (exp_alpha - 1.0) / (1.0 + exp_alpha);

        Some(spread.max(0.0))
    }
}

// ---------------------------------------------------------------------------
// SpreadAnalyzer
// ---------------------------------------------------------------------------

/// Rolling bid-ask spread analyzer combining Roll's model with effective
/// and realized spread computations.
///
/// Maintains a fixed-size rolling window of trade observations; as new
/// trades are pushed, the oldest are evicted.
pub struct SpreadAnalyzer {
    window: VecDeque<TradeData>,
    max_window: usize,
}

impl SpreadAnalyzer {
    /// Create a new analyzer with the given rolling window size.
    ///
    /// # Panics
    ///
    /// Panics if `max_window` is zero.
    pub fn new(max_window: usize) -> Self {
        assert!(max_window > 0, "SpreadAnalyzer window must be > 0");
        Self {
            window: VecDeque::with_capacity(max_window),
            max_window,
        }
    }

    /// Push a new trade into the rolling window, evicting the oldest if full.
    pub fn push_trade(&mut self, trade: TradeData) {
        if self.window.len() == self.max_window {
            self.window.pop_front();
        }
        self.window.push_back(trade);
    }

    /// Estimate the Roll implied spread from the current window.
    ///
    /// Returns `None` if there are fewer than 3 trades in the window or
    /// the covariance is non-negative.
    pub fn roll_estimate(&self) -> Option<f64> {
        let trades: Vec<TradeData> = self.window.iter().copied().collect();
        RollSpreadEstimator::estimate(&trades)
    }

    /// Number of trades currently in the rolling window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Returns `true` if the window is empty.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Compute the effective spread for a single trade.
    ///
    /// ```text
    /// effective_spread = 2 * |trade_price - mid_price|
    /// ```
    ///
    /// This measures the actual cost of the trade relative to the mid-quote.
    pub fn effective_spread(trade_price: f64, mid_price: f64) -> f64 {
        2.0 * (trade_price - mid_price).abs()
    }

    /// Compute the realized spread for a single trade.
    ///
    /// ```text
    /// realized_spread = 2 * direction * (trade_price - mid_price_future)
    /// ```
    ///
    /// Where `direction` is +1.0 for buyer-initiated and −1.0 for
    /// seller-initiated trades.  The realized spread measures the
    /// market-maker's revenue after prices have moved.
    pub fn realized_spread(trade_price: f64, mid_price_future: f64, direction: f64) -> f64 {
        2.0 * direction * (trade_price - mid_price_future)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trade(price: f64, ts: u64) -> TradeData {
        TradeData { price, volume: 100.0, timestamp_ms: ts }
    }

    // -- Roll's model --

    #[test]
    fn roll_requires_at_least_3_trades() {
        assert!(RollSpreadEstimator::estimate(&[]).is_none());
        assert!(RollSpreadEstimator::estimate(&[make_trade(100.0, 0)]).is_none());
        assert!(RollSpreadEstimator::estimate(&[make_trade(100.0, 0), make_trade(101.0, 1)]).is_none());
    }

    #[test]
    fn roll_constant_prices_returns_none() {
        let trades: Vec<_> = (0..5).map(|i| make_trade(100.0, i)).collect();
        // All price changes are 0 → covariance is 0 (non-negative).
        assert!(RollSpreadEstimator::estimate(&trades).is_none());
    }

    #[test]
    fn roll_alternating_prices_gives_positive_spread() {
        // Simulate perfect bid-ask bounce: alternating 99.5 / 100.5.
        // Price changes alternate between +1 and -1 → strong negative serial cov.
        let prices = [99.5, 100.5, 99.5, 100.5, 99.5, 100.5, 99.5, 100.5];
        let trades: Vec<_> = prices.iter().enumerate().map(|(i, &p)| make_trade(p, i as u64)).collect();
        let spread = RollSpreadEstimator::estimate(&trades);
        assert!(spread.is_some(), "alternating prices should yield a spread estimate");
        let s = spread.unwrap();
        assert!(s > 0.0, "spread should be positive, got {s}");
    }

    #[test]
    fn roll_known_series() {
        // Construct a series with known serial covariance.
        // ΔP = [+1, -1, +1, -1] → Cov(ΔP_t, ΔP_{t-1}) should be negative.
        let prices = [100.0, 101.0, 100.0, 101.0, 100.0];
        let trades: Vec<_> = prices.iter().enumerate().map(|(i, &p)| make_trade(p, i as u64)).collect();
        let spread = RollSpreadEstimator::estimate(&trades);
        assert!(spread.is_some());
        // Spread should approximate 2 * sqrt(cov magnitude).
        let s = spread.unwrap();
        assert!(s > 0.0 && s < 5.0, "spread {s} out of expected range");
    }

    // -- Corwin-Schultz --

    #[test]
    fn corwin_schultz_requires_at_least_2_obs() {
        assert!(CorwinSchultzSpread::estimate(&[], &[]).is_none());
        assert!(CorwinSchultzSpread::estimate(&[10.0], &[9.0]).is_none());
    }

    #[test]
    fn corwin_schultz_mismatched_lengths() {
        assert!(CorwinSchultzSpread::estimate(&[10.0, 11.0], &[9.0]).is_none());
    }

    #[test]
    fn corwin_schultz_invalid_prices() {
        // H < L is invalid.
        assert!(CorwinSchultzSpread::estimate(&[9.0, 11.0], &[10.0, 10.0]).is_none());
        // L <= 0 is invalid.
        assert!(CorwinSchultzSpread::estimate(&[10.0, 11.0], &[0.0, 10.0]).is_none());
    }

    #[test]
    fn corwin_schultz_narrow_ranges_small_spread() {
        // Narrow H/L ranges → small spread estimate.
        let high = [100.01, 100.02, 100.01];
        let low  = [99.99,  100.00, 99.99];
        let spread = CorwinSchultzSpread::estimate(&high, &low);
        assert!(spread.is_some(), "should return a spread for narrow ranges");
        let s = spread.unwrap();
        assert!(s >= 0.0, "spread must be non-negative, got {s}");
    }

    #[test]
    fn corwin_schultz_wide_ranges_larger_spread() {
        let narrow_high = [100.01, 100.02];
        let narrow_low  = [99.99,  100.00];
        let wide_high = [105.0, 106.0];
        let wide_low  = [95.0,  94.0];

        let s_narrow = CorwinSchultzSpread::estimate(&narrow_high, &narrow_low).unwrap();
        let s_wide   = CorwinSchultzSpread::estimate(&wide_high, &wide_low).unwrap();
        assert!(s_wide > s_narrow, "wider ranges should yield a larger spread estimate");
    }

    #[test]
    fn corwin_schultz_clamped_to_zero() {
        // Equal H and L → no range → β=0, γ=0 → α could be ±, clamp to 0.
        let high = [100.0, 100.0];
        let low  = [100.0, 100.0];
        // H == L would fail the H >= L > 0 check since ln(100/100) = 0 → valid.
        let spread = CorwinSchultzSpread::estimate(&high, &low);
        assert!(spread.is_some());
        assert!((spread.unwrap()).abs() < 1e-10, "zero-range spread should be ~0");
    }

    // -- SpreadAnalyzer --

    #[test]
    fn analyzer_rolling_window_eviction() {
        let mut analyzer = SpreadAnalyzer::new(3);
        for i in 0..5u64 {
            analyzer.push_trade(make_trade(100.0 + i as f64, i));
        }
        assert_eq!(analyzer.len(), 3, "window should hold at most 3 trades");
    }

    #[test]
    fn analyzer_effective_spread() {
        let s = SpreadAnalyzer::effective_spread(100.5, 100.0);
        assert!((s - 1.0).abs() < 1e-10, "effective spread should be 1.0");
    }

    #[test]
    fn analyzer_realized_spread_buyer() {
        // Buyer-initiated: direction = +1.
        // trade_price=100.5, mid_future=100.2
        // realized = 2 * 1 * (100.5 - 100.2) = 0.6
        let s = SpreadAnalyzer::realized_spread(100.5, 100.2, 1.0);
        assert!((s - 0.6).abs() < 1e-10, "realized spread={s}");
    }

    #[test]
    fn analyzer_realized_spread_seller() {
        // Seller-initiated: direction = -1.
        let s = SpreadAnalyzer::realized_spread(99.5, 99.8, -1.0);
        // realized = 2 * (-1) * (99.5 - 99.8) = 2 * (-1) * (-0.3) = 0.6
        assert!((s - 0.6).abs() < 1e-10, "realized spread={s}");
    }

    #[test]
    fn analyzer_roll_estimate_from_window() {
        let mut analyzer = SpreadAnalyzer::new(20);
        let prices = [99.5, 100.5, 99.5, 100.5, 99.5, 100.5];
        for (i, &p) in prices.iter().enumerate() {
            analyzer.push_trade(make_trade(p, i as u64));
        }
        // Alternating prices → Roll should detect bid-ask bounce.
        let est = analyzer.roll_estimate();
        assert!(est.is_some(), "should get a Roll estimate from bouncing prices");
    }

    #[test]
    fn analyzer_empty_roll_estimate_none() {
        let analyzer = SpreadAnalyzer::new(10);
        assert!(analyzer.roll_estimate().is_none(), "empty window → None");
    }
}
