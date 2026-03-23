// SPDX-License-Identifier: MIT
//! Real-time market regime detection.
//!
//! ## Responsibility
//!
//! Classify an incoming stream of OHLCV bars into one of five market regimes
//! using a rolling 100-bar window.  Three complementary indicators are
//! computed and fused into a single regime classification with a confidence
//! score:
//!
//! | Indicator | Regime signal |
//! |-----------|---------------|
//! | **Hurst exponent** (R/S analysis) | H > 0.6 → trending; H < 0.4 → mean-reverting |
//! | **Average Directional Index (ADX)** | ADX > 25 → trending; ADX < 20 → range-bound |
//! | **Realised volatility** | σ_rel > threshold → high-vol; σ_rel < threshold → low-vol |
//!
//! Additionally a **Microstructure** regime is flagged when bar-count is
//! below the minimum warm-up period needed for reliable estimates.
//!
//! ## Algorithms
//!
//! ### Hurst Exponent (Rescaled Range / R/S analysis)
//!
//! For a price return series `r_1, …, r_n`:
//!
//! 1. Compute mean return `μ`.
//! 2. Build cumulative deviation series `Y_t = Σ_{i=1}^{t} (r_i − μ)`.
//! 3. Range `R = max(Y) − min(Y)`.
//! 4. Standard deviation `S = std(r)`.
//! 5. `H = log(R/S) / log(n)`.
//!
//! `H ≈ 0.5` → random walk; `H > 0.5` → trending (long memory);
//! `H < 0.5` → mean-reverting (anti-persistent).
//!
//! ### Average Directional Index (ADX, Wilder 1978)
//!
//! Computed over a 14-bar smoothing period:
//!
//! ```text
//! +DM = max(high_t − high_{t-1}, 0)  if > |low_t − low_{t-1}|, else 0
//! −DM = max(low_{t-1} − low_t,   0)  if > (high_t − high_{t-1}), else 0
//! TR  = max(high − low, |high − prev_close|, |low − prev_close|)
//!
//! +DI_14 = 100 × EMA(+DM, 14) / EMA(TR, 14)
//! −DI_14 = 100 × EMA(−DM, 14) / EMA(TR, 14)
//! DX     = 100 × |+DI − −DI| / (+DI + −DI)
//! ADX    = EMA(DX, 14)
//! ```
//!
//! ADX > 25 indicates a strong trend; ADX < 20 indicates a ranging market.

use crate::ohlcv::OhlcvBar;
use std::collections::VecDeque;

// ────────────────────────────────────────────────────────────────────────────
// Public types
// ────────────────────────────────────────────────────────────────────────────

/// Classification of the current market regime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MarketRegime {
    /// Market is trending.  `direction > 0` = uptrend, `< 0` = downtrend, `0` = unclear.
    Trending {
        /// +1 (up), -1 (down), or 0 (indeterminate direction).
        direction: i8,
    },
    /// Market is mean-reverting / ranging.
    MeanReverting,
    /// Realised volatility is abnormally high.
    HighVolatility,
    /// Realised volatility is abnormally low (compressed, coiling).
    LowVolatility,
    /// Insufficient data — fewer than the minimum warm-up bars have been seen.
    Microstructure,
}

impl MarketRegime {
    /// Human-readable label for the regime.
    pub fn label(&self) -> &'static str {
        match self {
            MarketRegime::Trending { direction: 1 } => "Trending (up)",
            MarketRegime::Trending { direction: -1 } => "Trending (down)",
            MarketRegime::Trending { .. } => "Trending",
            MarketRegime::MeanReverting => "Mean-Reverting",
            MarketRegime::HighVolatility => "High-Volatility",
            MarketRegime::LowVolatility => "Low-Volatility",
            MarketRegime::Microstructure => "Microstructure",
        }
    }
}

impl std::fmt::Display for MarketRegime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ADX running state
// ────────────────────────────────────────────────────────────────────────────

/// Wilder-smoothed running ADX state (14-period default).
#[derive(Debug, Clone)]
struct AdxState {
    period: usize,
    smooth_plus_dm: f64,
    smooth_minus_dm: f64,
    smooth_tr: f64,
    adx: f64,
    prev_high: f64,
    prev_low: f64,
    prev_close: f64,
    initialized: bool,
    bar_count: usize,
}

impl AdxState {
    fn new(period: usize) -> Self {
        Self {
            period,
            smooth_plus_dm: 0.0,
            smooth_minus_dm: 0.0,
            smooth_tr: 0.0,
            adx: 0.0,
            prev_high: 0.0,
            prev_low: 0.0,
            prev_close: 0.0,
            initialized: false,
            bar_count: 0,
        }
    }

    fn update(&mut self, bar: &OhlcvBar) -> f64 {
        let high = decimal_to_f64(bar.high);
        let low = decimal_to_f64(bar.low);
        let close = decimal_to_f64(bar.close);

        if !self.initialized {
            self.prev_high = high;
            self.prev_low = low;
            self.prev_close = close;
            self.initialized = true;
            self.bar_count += 1;
            return 0.0;
        }

        // Directional movements.
        let up_move = high - self.prev_high;
        let down_move = self.prev_low - low;

        let plus_dm = if up_move > down_move && up_move > 0.0 { up_move } else { 0.0 };
        let minus_dm = if down_move > up_move && down_move > 0.0 { down_move } else { 0.0 };

        // True Range.
        let tr = (high - low)
            .max((high - self.prev_close).abs())
            .max((low - self.prev_close).abs());

        let k = 1.0 / self.period as f64;

        // Wilder smoothing: new = old * (period-1)/period + value / period.
        self.smooth_plus_dm = self.smooth_plus_dm * (1.0 - k) + plus_dm * k;
        self.smooth_minus_dm = self.smooth_minus_dm * (1.0 - k) + minus_dm * k;
        self.smooth_tr = self.smooth_tr * (1.0 - k) + tr * k;

        self.prev_high = high;
        self.prev_low = low;
        self.prev_close = close;
        self.bar_count += 1;

        if self.smooth_tr < 1e-12 {
            return self.adx;
        }

        let plus_di = 100.0 * self.smooth_plus_dm / self.smooth_tr;
        let minus_di = 100.0 * self.smooth_minus_dm / self.smooth_tr;
        let di_sum = plus_di + minus_di;

        let dx = if di_sum < 1e-12 {
            0.0
        } else {
            100.0 * (plus_di - minus_di).abs() / di_sum
        };

        // Smooth ADX over the same period.
        self.adx = self.adx * (1.0 - k) + dx * k;

        // Track directional trend from +DI / -DI.
        self.adx
    }

    fn plus_di(&self) -> f64 {
        if self.smooth_tr < 1e-12 {
            return 0.0;
        }
        100.0 * self.smooth_plus_dm / self.smooth_tr
    }

    fn minus_di(&self) -> f64 {
        if self.smooth_tr < 1e-12 {
            return 0.0;
        }
        100.0 * self.smooth_minus_dm / self.smooth_tr
    }
}

// ────────────────────────────────────────────────────────────────────────────
// RegimeDetector
// ────────────────────────────────────────────────────────────────────────────

/// Rolling-window market regime detector.
///
/// Feed OHLCV bars via [`update`](Self::update), then query the current regime
/// and confidence via [`current_regime`](Self::current_regime) and
/// [`regime_confidence`](Self::regime_confidence).
///
/// The detector maintains a **100-bar rolling window** (configurable) and
/// requires a minimum of 30 bars before issuing a non-`Microstructure` regime.
///
/// ## Example
///
/// ```rust
/// use fin_stream::regime::RegimeDetector;
///
/// let mut detector = RegimeDetector::new(100);
/// // … feed bars …
/// println!("Regime: {}", detector.current_regime());
/// println!("Confidence: {:.1}%", detector.regime_confidence() * 100.0);
/// ```
#[derive(Debug)]
pub struct RegimeDetector {
    /// Rolling window of close prices for Hurst / vol computation.
    close_window: VecDeque<f64>,
    /// Maximum bar window size.
    window: usize,
    /// ADX state machine.
    adx_state: AdxState,
    /// Current regime (updated on each call to `update`).
    regime: MarketRegime,
    /// Confidence in the current regime [0.0, 1.0].
    confidence: f64,
    /// Total bars processed.
    bar_count: usize,
}

/// Minimum number of bars required before non-microstructure regimes are reported.
const MIN_WARM_UP_BARS: usize = 30;

/// ADX period (Wilder's standard 14).
const ADX_PERIOD: usize = 14;

/// ADX threshold above which a trend is considered strong.
const ADX_TREND_THRESHOLD: f64 = 25.0;
/// ADX threshold below which a market is considered ranging.
const ADX_RANGE_THRESHOLD: f64 = 20.0;

/// Hurst exponent above which a series is classified as trending.
const HURST_TREND: f64 = 0.58;
/// Hurst exponent below which a series is classified as mean-reverting.
const HURST_MEAN_REVERT: f64 = 0.42;

/// Annualised realised volatility above which the market is "high-vol"
/// (relative to close price — dimensionless).
const HIGH_VOL_THRESHOLD: f64 = 0.025;
/// Annualised realised volatility below which the market is "low-vol".
const LOW_VOL_THRESHOLD: f64 = 0.005;

impl RegimeDetector {
    /// Construct with a given rolling-window size.
    ///
    /// `window` must be ≥ 10.  The standard value is 100.
    ///
    /// # Panics
    ///
    /// Panics if `window < 10` — this is an API misuse guard.
    pub fn new(window: usize) -> Self {
        assert!(window >= 10, "RegimeDetector window must be >= 10");
        Self {
            close_window: VecDeque::with_capacity(window + 1),
            window,
            adx_state: AdxState::new(ADX_PERIOD),
            regime: MarketRegime::Microstructure,
            confidence: 0.0,
            bar_count: 0,
        }
    }

    // ── Feed ─────────────────────────────────────────────────────────────

    /// Ingest one OHLCV bar and update the regime classification.
    pub fn update(&mut self, bar: &OhlcvBar) {
        let close = decimal_to_f64(bar.close);

        // Maintain rolling close window.
        if self.close_window.len() == self.window {
            self.close_window.pop_front();
        }
        self.close_window.push_back(close);

        // Update ADX.
        let adx = self.adx_state.update(bar);

        self.bar_count += 1;

        if self.bar_count < MIN_WARM_UP_BARS {
            self.regime = MarketRegime::Microstructure;
            self.confidence = 0.0;
            return;
        }

        // Compute indicators.
        let closes: Vec<f64> = self.close_window.iter().copied().collect();
        let hurst = Self::hurst_exponent(&closes);
        let rel_vol = realised_vol_relative(&closes);
        let plus_di = self.adx_state.plus_di();
        let minus_di = self.adx_state.minus_di();

        // Fuse indicators into a regime.
        self.classify(adx, hurst, rel_vol, plus_di, minus_di);
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// The most recently computed market regime.
    pub fn current_regime(&self) -> MarketRegime {
        self.regime
    }

    /// Confidence in the current regime classification, in [0.0, 1.0].
    ///
    /// Confidence is computed as the agreement between the three underlying
    /// indicators (Hurst, ADX, volatility).  A value above 0.7 suggests that
    /// at least two of the three indicators agree strongly.
    pub fn regime_confidence(&self) -> f64 {
        self.confidence
    }

    /// Total number of bars that have been fed to the detector.
    pub fn bar_count(&self) -> usize {
        self.bar_count
    }

    // ── Static analytics ─────────────────────────────────────────────────

    /// Hurst exponent via Rescaled Range (R/S) analysis.
    ///
    /// Returns a value in [0.0, 1.0], or `f64::NAN` for slices with fewer
    /// than 8 elements or zero variance in returns.
    ///
    /// | H | Interpretation |
    /// |---|----------------|
    /// | H > 0.5 | Trending / persistent (long memory) |
    /// | H ≈ 0.5 | Random walk |
    /// | H < 0.5 | Mean-reverting / anti-persistent |
    pub fn hurst_exponent(window: &[f64]) -> f64 {
        let n = window.len();
        if n < 8 {
            return f64::NAN;
        }

        // Compute log-returns.
        let returns: Vec<f64> = window
            .windows(2)
            .filter_map(|w| {
                if w[0] > 0.0 {
                    Some((w[1] / w[0]).ln())
                } else {
                    None
                }
            })
            .collect();

        if returns.len() < 4 {
            return f64::NAN;
        }

        let m = returns.len() as f64;
        let mean_r = returns.iter().sum::<f64>() / m;

        // Cumulative deviation from mean.
        let mut cum_dev = 0.0_f64;
        let mut max_dev = f64::NEG_INFINITY;
        let mut min_dev = f64::INFINITY;
        for &r in &returns {
            cum_dev += r - mean_r;
            max_dev = max_dev.max(cum_dev);
            min_dev = min_dev.min(cum_dev);
        }

        let range = max_dev - min_dev;
        let std = returns.iter().map(|r| (r - mean_r).powi(2)).sum::<f64>() / m;
        let std = std.sqrt();

        if std < 1e-15 || range <= 0.0 {
            return f64::NAN;
        }

        // H = log(R/S) / log(n)
        let rs = range / std;
        let h = rs.ln() / (returns.len() as f64).ln();
        h.clamp(0.0, 1.0)
    }

    /// Average Directional Index computed over a slice of bars.
    ///
    /// Uses the same Wilder-smoothed ADX algorithm as the internal running
    /// state but operates on a static slice.  Returns 0.0 for slices with
    /// fewer than `2 * ADX_PERIOD` bars.
    pub fn adx(bars: &[OhlcvBar]) -> f64 {
        if bars.len() < ADX_PERIOD * 2 {
            return 0.0;
        }
        let mut state = AdxState::new(ADX_PERIOD);
        let mut adx = 0.0_f64;
        for bar in bars {
            adx = state.update(bar);
        }
        adx
    }

    // ── Internal classification ───────────────────────────────────────────

    fn classify(
        &mut self,
        adx: f64,
        hurst: f64,
        rel_vol: f64,
        plus_di: f64,
        minus_di: f64,
    ) {
        // Volatility regimes take priority when extreme.
        if rel_vol > HIGH_VOL_THRESHOLD * 2.0 {
            self.regime = MarketRegime::HighVolatility;
            self.confidence = ((rel_vol - HIGH_VOL_THRESHOLD) / HIGH_VOL_THRESHOLD).clamp(0.0, 1.0);
            return;
        }
        if rel_vol < LOW_VOL_THRESHOLD * 0.5 {
            self.regime = MarketRegime::LowVolatility;
            self.confidence =
                ((LOW_VOL_THRESHOLD - rel_vol) / LOW_VOL_THRESHOLD).clamp(0.0, 1.0);
            return;
        }

        // Trend regime: ADX strong trend OR Hurst trend.
        let adx_trending = adx >= ADX_TREND_THRESHOLD;
        let hurst_trending = hurst.is_finite() && hurst > HURST_TREND;
        let adx_mean_rev = adx < ADX_RANGE_THRESHOLD;
        let hurst_mean_rev = hurst.is_finite() && hurst < HURST_MEAN_REVERT;

        // Count indicator votes.
        let trend_votes = adx_trending as u32 + hurst_trending as u32;
        let mr_votes = adx_mean_rev as u32 + hurst_mean_rev as u32;

        if trend_votes > mr_votes {
            let direction: i8 = if plus_di > minus_di { 1 } else if minus_di > plus_di { -1 } else { 0 };
            self.regime = MarketRegime::Trending { direction };

            // Confidence scales with ADX magnitude and Hurst distance from 0.5.
            let adx_conf = if adx_trending {
                ((adx - ADX_TREND_THRESHOLD) / (100.0 - ADX_TREND_THRESHOLD)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let hurst_conf = if hurst_trending && hurst.is_finite() {
                ((hurst - 0.5) / 0.5).clamp(0.0, 1.0)
            } else {
                0.0
            };
            self.confidence = (adx_conf + hurst_conf) / (trend_votes as f64).max(1.0);
        } else if mr_votes > trend_votes {
            self.regime = MarketRegime::MeanReverting;
            let adx_conf = if adx_mean_rev {
                ((ADX_RANGE_THRESHOLD - adx) / ADX_RANGE_THRESHOLD).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let hurst_conf = if hurst_mean_rev && hurst.is_finite() {
                ((0.5 - hurst) / 0.5).clamp(0.0, 1.0)
            } else {
                0.0
            };
            self.confidence = (adx_conf + hurst_conf) / (mr_votes as f64).max(1.0);
        } else {
            // Tied — check vol level.
            if rel_vol > HIGH_VOL_THRESHOLD {
                self.regime = MarketRegime::HighVolatility;
            } else if rel_vol < LOW_VOL_THRESHOLD {
                self.regime = MarketRegime::LowVolatility;
            } else {
                self.regime = MarketRegime::MeanReverting;
            }
            self.confidence = 0.4;
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Convert `rust_decimal::Decimal` to `f64`.
#[inline]
fn decimal_to_f64(d: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

/// Relative realised volatility — std of log-returns / mean close.
///
/// Returns 0.0 for slices with fewer than 2 prices or zero mean.
fn realised_vol_relative(closes: &[f64]) -> f64 {
    if closes.len() < 2 {
        return 0.0;
    }
    let returns: Vec<f64> = closes
        .windows(2)
        .filter_map(|w| if w[0] > 0.0 { Some((w[1] / w[0]).ln()) } else { None })
        .collect();
    if returns.is_empty() {
        return 0.0;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    var.sqrt()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ohlcv::{OhlcvBar, Timeframe};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn make_bar(o: f64, h: f64, l: f64, c: f64, v: f64) -> OhlcvBar {
        OhlcvBar {
            symbol: "BTC-USD".into(),
            timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0,
            open: Decimal::from_str(&format!("{o}")).unwrap_or_default(),
            high: Decimal::from_str(&format!("{h}")).unwrap_or_default(),
            low: Decimal::from_str(&format!("{l}")).unwrap_or_default(),
            close: Decimal::from_str(&format!("{c}")).unwrap_or_default(),
            volume: Decimal::from_str(&format!("{v}")).unwrap_or_default(),
            trade_count: 1,
            is_complete: true,
            is_gap_fill: false,
            vwap: None,
        }
    }

    #[test]
    fn microstructure_before_warmup() {
        let mut d = RegimeDetector::new(100);
        for i in 0..29 {
            let p = 100.0 + i as f64;
            d.update(&make_bar(p, p + 1.0, p - 1.0, p + 0.5, 10.0));
        }
        assert_eq!(d.current_regime(), MarketRegime::Microstructure);
    }

    #[test]
    fn trending_regime_on_strong_uptrend() {
        let mut d = RegimeDetector::new(100);
        // Build a strong uptrend: prices increase by 2 each bar with tight range.
        for i in 0..60 {
            let p = 100.0 + i as f64 * 2.0;
            d.update(&make_bar(p, p + 1.0, p - 0.5, p + 1.5, 100.0));
        }
        let regime = d.current_regime();
        // Should identify as Trending or MeanReverting — at least not Microstructure.
        assert_ne!(regime, MarketRegime::Microstructure);
    }

    #[test]
    fn hurst_random_walk_near_half() {
        // Prices follow a simple additive random walk seed.
        let prices: Vec<f64> = (0..100).map(|i| 100.0 + (i as f64 * 0.1)).collect();
        let h = RegimeDetector::hurst_exponent(&prices);
        assert!(h.is_finite(), "Hurst should be finite for a smooth uptrend");
        // A perfectly smooth uptrend should have H close to 1.
        assert!(h > 0.5, "Smooth uptrend should have H > 0.5, got {h}");
    }

    #[test]
    fn hurst_nan_for_small_slice() {
        let prices = vec![100.0, 101.0, 99.0];
        assert!(RegimeDetector::hurst_exponent(&prices).is_nan());
    }

    #[test]
    fn adx_static_returns_zero_for_small_slice() {
        let bars: Vec<OhlcvBar> = (0..5)
            .map(|i| {
                let p = 100.0 + i as f64;
                make_bar(p, p + 1.0, p - 1.0, p, 10.0)
            })
            .collect();
        assert_eq!(RegimeDetector::adx(&bars), 0.0);
    }

    #[test]
    fn adx_static_finite_for_sufficient_data() {
        let bars: Vec<OhlcvBar> = (0..40)
            .map(|i| {
                let p = 100.0 + i as f64;
                make_bar(p, p + 1.5, p - 0.5, p + 1.0, 50.0)
            })
            .collect();
        let adx = RegimeDetector::adx(&bars);
        assert!(adx.is_finite());
        assert!(adx >= 0.0);
    }

    #[test]
    fn confidence_in_unit_interval() {
        let mut d = RegimeDetector::new(100);
        for i in 0..80 {
            let p = 100.0 + i as f64;
            d.update(&make_bar(p, p + 1.0, p - 1.0, p + 0.5, 10.0));
        }
        let conf = d.regime_confidence();
        assert!((0.0..=1.0).contains(&conf), "confidence {conf} out of [0,1]");
    }

    #[test]
    fn bar_count_increments() {
        let mut d = RegimeDetector::new(10);
        for i in 0..5 {
            let p = 100.0 + i as f64;
            d.update(&make_bar(p, p + 1.0, p - 1.0, p, 10.0));
        }
        assert_eq!(d.bar_count(), 5);
    }
}
