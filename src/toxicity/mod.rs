// SPDX-License-Identifier: MIT
//! Order flow toxicity analysis — identifying informed (smart-money) trading.
//!
//! ## Responsibility
//!
//! Maintain rolling microstructure statistics that measure how much of the
//! observed order flow is likely to originate from informed traders.  Four
//! complementary measures are implemented:
//!
//! | Metric | Signal |
//! |--------|--------|
//! | **PIN** | Probability of Informed Trading — fraction of order flow that is information-driven |
//! | **VPIN** | Volume-synchronised PIN — a high-frequency PIN proxy that buckets by traded volume |
//! | **Kyle λ** | Price-impact coefficient — how much does price move per unit of net order flow |
//! | **Amihud ratio** | Absolute return divided by volume — illiquidity measure |
//!
//! ## Background
//!
//! ### PIN (Easley, Kiefer, O'Hara & Paperman 1996)
//!
//! A structural model where, on any given day, an information event occurs with
//! probability `α`.  Given an event, informed traders arrive at rate `μ` on one
//! side only; uninformed traders arrive at rate `ε` on both sides.
//!
//! ```text
//! PIN  =  α·μ / (α·μ + 2·ε)
//! ```
//!
//! Parameters `(α, μ, ε)` are estimated via moment-matching:
//!
//! * `E[buys]  = α·μ/2 + ε`
//! * `E[sells] = α·μ/2 + ε`
//! * `Var[buys] > Var[sells]` on informed days → `α` estimated from excess variance
//!
//! ### VPIN (Easley, Lopez de Prado & O'Hara 2012)
//!
//! Divide trades into equal-volume buckets of size `V_bucket`.  Within each
//! bucket compute the fraction of buy volume `V_B` and sell volume `V_S`:
//!
//! ```text
//! VPIN  =  |V_B - V_S| / V_bucket        (per bucket)
//! VPIN  =  rolling mean over last n buckets
//! ```
//!
//! ### Kyle λ (Kyle 1985)
//!
//! Price impact per unit of signed order flow, estimated by OLS:
//!
//! ```text
//! ΔP_t  =  λ · x_t  +  ε_t
//! λ      =  cov(ΔP, x) / var(x)
//! ```
//!
//! where `x_t` is signed volume (+buy, -sell) and `ΔP_t = P_t − P_{t−1}`.
//!
//! ### Amihud illiquidity ratio (Amihud 2002)
//!
//! ```text
//! ILLIQ  =  |r_t| / volume_t
//! ```
//!
//! ## Legacy API
//!
//! The types [`TradeTick`], [`VpinConfig`], [`VpinCalculator`], [`VolumeBucket`],
//! and [`ToxicityAlert`] from the original single-metric implementation are
//! retained for backward compatibility.  New code should prefer
//! [`OrderFlowToxicityAnalyzer`] which computes all four metrics in one pass.

use crate::tick::{NormalizedTick, TradeSide};
use std::collections::VecDeque;

// ────────────────────────────────────────────────────────────────────────────
// Legacy VPIN types (preserved for backward compatibility)
// ────────────────────────────────────────────────────────────────────────────

/// A single trade tick used for VPIN computation (legacy API).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TradeTick {
    /// Trade price.
    pub price: f64,
    /// Trade volume (always positive).
    pub volume: f64,
}

/// Configuration for the legacy [`VpinCalculator`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VpinConfig {
    /// Volume units per bucket.
    pub bucket_volume: f64,
    /// Number of completed buckets to retain for the rolling mean.
    pub n_buckets: usize,
    /// VPIN level above which a [`ToxicityAlert::HighToxicity`] is generated.
    pub alert_threshold: f64,
}

impl Default for VpinConfig {
    fn default() -> Self {
        Self { bucket_volume: 1000.0, n_buckets: 50, alert_threshold: 0.7 }
    }
}

/// A completed VPIN volume bucket (legacy API).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeBucket {
    /// Total buyer-initiated volume in this bucket.
    pub buy_volume: f64,
    /// Total seller-initiated volume in this bucket.
    pub sell_volume: f64,
    /// Imbalance fraction: `|buy − sell| / (buy + sell)`.
    pub imbalance: f64,
}

/// Alert emitted by [`VpinCalculator`] when VPIN crosses the configured threshold.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ToxicityAlert {
    /// VPIN exceeded the alert threshold.
    HighToxicity {
        /// Current VPIN value that triggered the alert.
        vpin: f64,
        /// Configured threshold.
        threshold: f64,
    },
}

impl std::fmt::Display for ToxicityAlert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToxicityAlert::HighToxicity { vpin, threshold } => {
                write!(f, "HighToxicity: VPIN={vpin:.4} > threshold={threshold:.4}")
            }
        }
    }
}

/// Single-metric VPIN calculator (legacy interface).
///
/// For the full four-metric suite (PIN, VPIN, Kyle λ, Amihud) use
/// [`OrderFlowToxicityAnalyzer`] instead.
#[derive(Debug)]
pub struct VpinCalculator {
    cfg: VpinConfig,
    completed: VecDeque<VolumeBucket>,
    current_buy: f64,
    current_sell: f64,
    current_vol: f64,
    last_price: Option<f64>,
}

impl VpinCalculator {
    /// Construct from a [`VpinConfig`].
    pub fn new(cfg: VpinConfig) -> Self {
        let cap = cfg.n_buckets + 1;
        Self {
            cfg,
            completed: VecDeque::with_capacity(cap),
            current_buy: 0.0,
            current_sell: 0.0,
            current_vol: 0.0,
            last_price: None,
        }
    }

    /// Feed one [`TradeTick`].  Returns any alerts generated.
    pub fn update(&mut self, tick: TradeTick) -> Vec<ToxicityAlert> {
        let is_buy = match self.last_price {
            Some(prev) => tick.price >= prev,
            None => true,
        };
        self.last_price = Some(tick.price);

        let mut remaining = tick.volume;
        let mut alerts = Vec::new();

        while remaining > 0.0 {
            let space = self.cfg.bucket_volume - self.current_vol;
            let fill = remaining.min(space);
            if is_buy { self.current_buy += fill; } else { self.current_sell += fill; }
            self.current_vol += fill;
            remaining -= fill;

            if self.current_vol >= self.cfg.bucket_volume {
                let total = self.current_buy + self.current_sell;
                let imbalance = if total > 0.0 {
                    (self.current_buy - self.current_sell).abs() / total
                } else {
                    0.0
                };
                let bucket = VolumeBucket {
                    buy_volume: self.current_buy,
                    sell_volume: self.current_sell,
                    imbalance,
                };
                if self.completed.len() == self.cfg.n_buckets {
                    self.completed.pop_front();
                }
                self.completed.push_back(bucket);
                self.current_buy = 0.0;
                self.current_sell = 0.0;
                self.current_vol = 0.0;

                if let Some(v) = self.vpin() {
                    if v > self.cfg.alert_threshold {
                        alerts.push(ToxicityAlert::HighToxicity {
                            vpin: v,
                            threshold: self.cfg.alert_threshold,
                        });
                    }
                }
            }
        }
        alerts
    }

    /// Current VPIN estimate, or `None` if no buckets have been completed.
    pub fn vpin(&self) -> Option<f64> {
        if self.completed.is_empty() {
            return None;
        }
        let sum: f64 = self.completed.iter().map(|b| b.imbalance).sum();
        Some((sum / self.completed.len() as f64).clamp(0.0, 1.0))
    }

    /// Reference to the completed bucket history.
    pub fn buckets(&self) -> &VecDeque<VolumeBucket> {
        &self.completed
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Full four-metric implementation
// ────────────────────────────────────────────────────────────────────────────

/// Snapshot of all four toxicity metrics at a point in time.
///
/// All values are `f64` and are `NaN` until enough data has been accumulated
/// (typically 50–100 ticks for PIN / Kyle λ).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToxicityMetrics {
    /// Probability of Informed Trading (0–1).
    ///
    /// High PIN (> 0.3) suggests a large fraction of order flow is driven by
    /// agents with private information.
    pub pin: f64,

    /// Volume-synchronised PIN (0–1).
    ///
    /// Responds faster to regime changes than the classical PIN estimate.
    pub vpin: f64,

    /// Kyle's λ — OLS price-impact coefficient (price units per volume unit).
    ///
    /// A large positive λ means prices move strongly in the direction of order
    /// flow; near-zero indicates a liquid, resilient market.
    pub kyle_lambda: f64,

    /// Amihud illiquidity ratio — |return| / volume (dimensionless).
    ///
    /// Higher values signal a less liquid, more easily moved market.
    pub amihud_illiquidity: f64,

    /// Number of ticks processed so far.
    pub tick_count: usize,
}

impl ToxicityMetrics {
    /// Returns `true` when all four metrics carry valid (non-NaN) estimates.
    pub fn is_valid(&self) -> bool {
        self.pin.is_finite()
            && self.vpin.is_finite()
            && self.kyle_lambda.is_finite()
            && self.amihud_illiquidity.is_finite()
    }
}

// Internal VPIN bucket for the four-metric analyser.
#[derive(Debug, Clone, Default)]
struct VpinBucket {
    buy_volume: f64,
    sell_volume: f64,
    total_volume: f64,
}

impl VpinBucket {
    fn imbalance_fraction(&self) -> f64 {
        if self.total_volume <= 0.0 { return 0.0; }
        (self.buy_volume - self.sell_volume).abs() / self.total_volume
    }
}

// Per-tick record for Kyle λ OLS.
#[derive(Debug, Clone, Copy)]
struct FlowRecord {
    signed_volume: f64,
    delta_price: f64,
}

/// Rolling-window order flow toxicity analyser (four-metric).
///
/// Feed ticks one at a time via [`update`](Self::update), then query any of
/// the four toxicity metrics on demand.
///
/// ## Window sizes
///
/// | Parameter | Default | Description |
/// |-----------|---------|-------------|
/// | `window`  | 200     | Tick window for PIN moment-matching, Kyle λ, and Amihud |
/// | `vpin_bucket_size` | 50 | Volume units per VPIN bucket |
/// | `vpin_window`      | 50 | Number of completed VPIN buckets to average |
///
/// ## Example
///
/// ```rust
/// use fin_stream::toxicity::OrderFlowToxicityAnalyzer;
///
/// let mut analyzer = OrderFlowToxicityAnalyzer::new(200, 50, 50);
/// // … feed NormalizedTicks …
/// let metrics = analyzer.metrics();
/// println!("PIN = {:.3}", metrics.pin);
/// ```
#[derive(Debug)]
pub struct OrderFlowToxicityAnalyzer {
    window: usize,

    // PIN moment-matching
    buy_flow: VecDeque<f64>,
    sell_flow: VecDeque<f64>,

    // VPIN
    vpin_bucket_size: f64,
    vpin_window: usize,
    current_bucket: VpinBucket,
    completed_buckets: VecDeque<VpinBucket>,

    // Kyle λ / OLS
    flow_records: VecDeque<FlowRecord>,
    last_price: Option<f64>,

    // Amihud
    amihud_samples: VecDeque<f64>,
    last_price_for_amihud: Option<f64>,

    tick_count: usize,
}

impl OrderFlowToxicityAnalyzer {
    /// Construct a new analyser.
    ///
    /// # Panics
    ///
    /// Panics if `window < 2`, `vpin_bucket_size == 0`, or `vpin_window == 0`.
    /// These are API misuse guards; they never fire on valid production inputs.
    pub fn new(window: usize, vpin_bucket_size: usize, vpin_window: usize) -> Self {
        assert!(window >= 2, "toxicity window must be >= 2");
        assert!(vpin_bucket_size > 0, "vpin_bucket_size must be > 0");
        assert!(vpin_window >= 1, "vpin_window must be >= 1");
        Self {
            window,
            buy_flow: VecDeque::with_capacity(window),
            sell_flow: VecDeque::with_capacity(window),
            vpin_bucket_size: vpin_bucket_size as f64,
            vpin_window,
            current_bucket: VpinBucket::default(),
            completed_buckets: VecDeque::with_capacity(vpin_window + 1),
            flow_records: VecDeque::with_capacity(window),
            last_price: None,
            amihud_samples: VecDeque::with_capacity(window),
            last_price_for_amihud: None,
            tick_count: 0,
        }
    }

    /// Construct with default parameters: window=200, bucket=50 vol units, vpin_window=50.
    pub fn default_params() -> Self {
        Self::new(200, 50, 50)
    }

    /// Ingest one tick.  All internal state is updated in O(1) amortised time.
    pub fn update(&mut self, tick: &NormalizedTick) {
        let price = decimal_to_f64(tick.price);
        let volume = decimal_to_f64(tick.quantity);

        let (buy_vol, sell_vol) = match tick.side {
            Some(TradeSide::Sell) => (0.0_f64, volume),
            _ => (volume, 0.0_f64),
        };
        let signed_vol = buy_vol - sell_vol;

        // PIN moment-matching rolling window
        if self.buy_flow.len() == self.window { self.buy_flow.pop_front(); }
        if self.sell_flow.len() == self.window { self.sell_flow.pop_front(); }
        self.buy_flow.push_back(buy_vol);
        self.sell_flow.push_back(sell_vol);

        // Kyle λ records
        let delta_price = self.last_price.map(|prev| price - prev).unwrap_or(0.0);
        if self.flow_records.len() == self.window { self.flow_records.pop_front(); }
        self.flow_records.push_back(FlowRecord { signed_volume: signed_vol, delta_price });
        self.last_price = Some(price);

        // Amihud
        if let Some(prev_price) = self.last_price_for_amihud {
            let ret = if prev_price != 0.0 {
                ((price - prev_price) / prev_price).abs()
            } else {
                0.0
            };
            let illiq = if volume > 0.0 { ret / volume } else { 0.0 };
            if self.amihud_samples.len() == self.window { self.amihud_samples.pop_front(); }
            self.amihud_samples.push_back(illiq);
        }
        self.last_price_for_amihud = Some(price);

        // VPIN bucket filling
        let mut remaining = volume;
        while remaining > 0.0 {
            let space = self.vpin_bucket_size - self.current_bucket.total_volume;
            let fill = remaining.min(space);
            let buy_fill = if volume > 0.0 { fill * (buy_vol / volume) } else { fill * 0.5 };
            let sell_fill = fill - buy_fill;
            self.current_bucket.buy_volume += buy_fill;
            self.current_bucket.sell_volume += sell_fill;
            self.current_bucket.total_volume += fill;
            remaining -= fill;

            if self.current_bucket.total_volume >= self.vpin_bucket_size {
                let finished = std::mem::take(&mut self.current_bucket);
                if self.completed_buckets.len() == self.vpin_window {
                    self.completed_buckets.pop_front();
                }
                self.completed_buckets.push_back(finished);
            }
        }

        self.tick_count += 1;
    }

    /// PIN estimate via moment-matching.
    ///
    /// Returns `f64::NAN` until `window` ticks have been observed.
    pub fn pin_estimate(&self) -> f64 {
        if self.buy_flow.len() < self.window { return f64::NAN; }

        let n = self.buy_flow.len() as f64;
        let mean_buy = self.buy_flow.iter().sum::<f64>() / n;
        let mean_sell = self.sell_flow.iter().sum::<f64>() / n;

        let var_buy = variance_f64(self.buy_flow.iter().copied());

        let epsilon = mean_buy.min(mean_sell).max(1e-9);
        let excess_var = (var_buy - mean_buy).max(0.0);
        let alpha_mu = (excess_var.sqrt()
            + (mean_buy - epsilon).max(0.0)
            + (mean_sell - epsilon).max(0.0))
            / 2.0;
        let alpha_mu = alpha_mu.max(0.0);
        let denom = alpha_mu + 2.0 * epsilon;
        if denom <= 0.0 { return 0.0; }
        (alpha_mu / denom).clamp(0.0, 1.0)
    }

    /// Volume-synchronised PIN over the most recent completed buckets.
    ///
    /// Returns `f64::NAN` until at least one bucket has been completed.
    pub fn vpin(&self) -> f64 {
        if self.completed_buckets.is_empty() { return f64::NAN; }
        let sum: f64 = self.completed_buckets.iter().map(|b| b.imbalance_fraction()).sum();
        (sum / self.completed_buckets.len() as f64).clamp(0.0, 1.0)
    }

    /// Kyle's λ — OLS estimate of price-impact coefficient.
    ///
    /// Returns `f64::NAN` until `window` records have been accumulated, or if
    /// the variance of signed volume is essentially zero.
    pub fn kyle_lambda(&self) -> f64 {
        if self.flow_records.len() < self.window { return f64::NAN; }
        let n = self.flow_records.len() as f64;
        let mean_x = self.flow_records.iter().map(|r| r.signed_volume).sum::<f64>() / n;
        let mean_y = self.flow_records.iter().map(|r| r.delta_price).sum::<f64>() / n;
        let cov_xy: f64 = self.flow_records.iter()
            .map(|r| (r.signed_volume - mean_x) * (r.delta_price - mean_y))
            .sum();
        let var_x: f64 = self.flow_records.iter()
            .map(|r| (r.signed_volume - mean_x).powi(2))
            .sum();
        if var_x.abs() < 1e-18 { return f64::NAN; }
        cov_xy / var_x
    }

    /// Amihud illiquidity ratio — rolling mean of `|return| / volume`.
    ///
    /// Returns `f64::NAN` until at least 2 ticks have been processed.
    pub fn amihud_ratio(&self) -> f64 {
        if self.amihud_samples.is_empty() { return f64::NAN; }
        let n = self.amihud_samples.len() as f64;
        self.amihud_samples.iter().sum::<f64>() / n
    }

    /// Returns a snapshot of all four metrics.
    pub fn metrics(&self) -> ToxicityMetrics {
        ToxicityMetrics {
            pin: self.pin_estimate(),
            vpin: self.vpin(),
            kyle_lambda: self.kyle_lambda(),
            amihud_illiquidity: self.amihud_ratio(),
            tick_count: self.tick_count,
        }
    }

    /// Number of ticks processed so far.
    pub fn tick_count(&self) -> usize {
        self.tick_count
    }

    /// Reset all accumulated state.
    pub fn reset(&mut self) {
        self.buy_flow.clear();
        self.sell_flow.clear();
        self.current_bucket = VpinBucket::default();
        self.completed_buckets.clear();
        self.flow_records.clear();
        self.last_price = None;
        self.amihud_samples.clear();
        self.last_price_for_amihud = None;
        self.tick_count = 0;
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

#[inline]
fn decimal_to_f64(d: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

fn variance_f64(iter: impl Iterator<Item = f64> + Clone) -> f64 {
    let v: Vec<f64> = iter.collect();
    let n = v.len() as f64;
    if n < 2.0 { return 0.0; }
    let mean = v.iter().sum::<f64>() / n;
    v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{Exchange, NormalizedTick, TradeSide};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn make_tick(price: &str, qty: &str, side: TradeSide) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTC-USD".into(),
            price: Decimal::from_str(price).unwrap_or_default(),
            quantity: Decimal::from_str(qty).unwrap_or_default(),
            side: Some(side),
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    // ── Legacy VpinCalculator ────────────────────────────────────────────

    #[test]
    fn legacy_vpin_none_before_bucket() {
        let cfg = VpinConfig { bucket_volume: 1000.0, n_buckets: 10, alert_threshold: 0.7 };
        let mut calc = VpinCalculator::new(cfg);
        calc.update(TradeTick { price: 100.0, volume: 10.0 });
        assert!(calc.vpin().is_none());
    }

    #[test]
    fn legacy_vpin_some_after_bucket() {
        let cfg = VpinConfig { bucket_volume: 5.0, n_buckets: 10, alert_threshold: 0.9 };
        let mut calc = VpinCalculator::new(cfg);
        for _ in 0..10 {
            calc.update(TradeTick { price: 100.0, volume: 1.0 });
        }
        assert!(calc.vpin().is_some());
    }

    #[test]
    fn legacy_alert_emitted() {
        let cfg = VpinConfig { bucket_volume: 5.0, n_buckets: 5, alert_threshold: 0.1 };
        let mut calc = VpinCalculator::new(cfg);
        let mut got_alert = false;
        for i in 0..30 {
            let price = 100.0 + i as f64 * 0.1;
            let alerts = calc.update(TradeTick { price, volume: 1.0 });
            if !alerts.is_empty() { got_alert = true; }
        }
        assert!(got_alert, "expected at least one alert with low threshold");
    }

    // ── OrderFlowToxicityAnalyzer ────────────────────────────────────────

    #[test]
    fn pin_nan_before_window_full() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        for _ in 0..9 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        assert!(a.pin_estimate().is_nan());
    }

    #[test]
    fn pin_valid_after_window_full() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        for i in 0..10 {
            let side = if i % 3 == 0 { TradeSide::Sell } else { TradeSide::Buy };
            a.update(&make_tick("100", "1", side));
        }
        let pin = a.pin_estimate();
        assert!(pin.is_finite());
        assert!((0.0..=1.0).contains(&pin), "PIN must be in [0,1], got {pin}");
    }

    #[test]
    fn vpin_nan_before_first_bucket() {
        let mut a = OrderFlowToxicityAnalyzer::new(20, 100, 5);
        for _ in 0..10 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        assert!(a.vpin().is_nan());
    }

    #[test]
    fn vpin_valid_after_bucket_filled() {
        let mut a = OrderFlowToxicityAnalyzer::new(200, 5, 5);
        for _ in 0..50 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        let vpin = a.vpin();
        assert!(vpin.is_finite());
        assert!((0.0..=1.0).contains(&vpin));
    }

    #[test]
    fn kyle_lambda_positive_on_buy_pressure() {
        let mut a = OrderFlowToxicityAnalyzer::new(20, 5, 5);
        let prices: Vec<f64> = (0..20).map(|i| 100.0 + i as f64 * 0.1).collect();
        for p in &prices {
            a.update(&make_tick(&format!("{p:.2}"), "10", TradeSide::Buy));
        }
        let lambda = a.kyle_lambda();
        assert!(lambda.is_finite());
        assert!(lambda >= 0.0, "lambda should be >=0 on sustained buy flow, got {lambda}");
    }

    #[test]
    fn amihud_finite_after_two_ticks() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        a.update(&make_tick("100", "1", TradeSide::Buy));
        a.update(&make_tick("101", "2", TradeSide::Sell));
        let amihud = a.amihud_ratio();
        assert!(amihud.is_finite());
        assert!(amihud >= 0.0);
    }

    #[test]
    fn metrics_tick_count() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        for _ in 0..7 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        assert_eq!(a.metrics().tick_count, 7);
    }

    #[test]
    fn reset_clears_state() {
        let mut a = OrderFlowToxicityAnalyzer::new(5, 2, 3);
        for _ in 0..10 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        a.reset();
        assert_eq!(a.tick_count(), 0);
        assert!(a.pin_estimate().is_nan());
    }
}
