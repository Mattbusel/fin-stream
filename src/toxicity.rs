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
//! Because this is an online approximation (not full MLE), estimates are
//! reliable only after a few hundred ticks.
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
//! Rolling mean of the per-tick ratio gives a smoothed illiquidity estimate.

use crate::tick::{NormalizedTick, TradeSide};
use std::collections::VecDeque;

// ────────────────────────────────────────────────────────────────────────────
// Public types
// ────────────────────────────────────────────────────────────────────────────

/// Snapshot of all toxicity metrics at a point in time.
///
/// All values are `f64` in their natural units and are `NaN` until enough data
/// has been accumulated (typically 50–100 ticks for PIN / Kyle λ).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToxicityMetrics {
    /// Probability of Informed Trading (0–1).
    ///
    /// High PIN (> 0.3) suggests that a large fraction of order flow is
    /// driven by agents with private information.
    pub pin: f64,

    /// Volume-synchronised PIN (0–1).
    ///
    /// Computed over the most recent `vpin_window` volume buckets.  Responds
    /// faster to regime changes than the classical PIN estimate.
    pub vpin: f64,

    /// Kyle's λ — OLS price-impact coefficient (price units per volume unit).
    ///
    /// A large positive λ means prices move strongly in the direction of the
    /// order flow; a value near zero indicates a liquid, resilient market.
    pub kyle_lambda: f64,

    /// Amihud illiquidity ratio — |return| / volume (dimensionless).
    ///
    /// Higher values signal a less liquid, more easily moved market.
    pub amihud_illiquidity: f64,

    /// Number of ticks that have been processed so far.
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

// ────────────────────────────────────────────────────────────────────────────
// Internal accumulation records
// ────────────────────────────────────────────────────────────────────────────

/// A single VPIN volume bucket (partial or complete).
#[derive(Debug, Clone, Default)]
struct VpinBucket {
    buy_volume: f64,
    sell_volume: f64,
    total_volume: f64,
}

impl VpinBucket {
    fn imbalance_fraction(&self) -> f64 {
        if self.total_volume <= 0.0 {
            return 0.0;
        }
        (self.buy_volume - self.sell_volume).abs() / self.total_volume
    }
}

/// Per-tick record used for Kyle λ OLS regression.
#[derive(Debug, Clone, Copy)]
struct FlowRecord {
    /// Signed volume: positive for buys, negative for sells.
    signed_volume: f64,
    /// Price change from the previous tick.
    delta_price: f64,
}

// ────────────────────────────────────────────────────────────────────────────
// OrderFlowToxicityAnalyzer
// ────────────────────────────────────────────────────────────────────────────

/// Rolling-window order flow toxicity analyser.
///
/// Feed ticks one at a time via [`update`](Self::update), then query any of the
/// four toxicity metrics on demand.
///
/// ## Window sizes
///
/// | Parameter | Default | Description |
/// |-----------|---------|-------------|
/// | `window`  | 200     | Tick window for PIN moment-matching, Kyle λ, and Amihud |
/// | `vpin_bucket_size` | 50 | Volume units per VPIN bucket |
/// | `vpin_window`      | 50 | Number of completed VPIN buckets to average |
///
/// # Example
///
/// ```rust
/// use fin_stream::toxicity::OrderFlowToxicityAnalyzer;
///
/// let mut analyzer = OrderFlowToxicityAnalyzer::new(200, 50, 50);
/// // … feed ticks …
/// let metrics = analyzer.metrics();
/// println!("PIN = {:.3}", metrics.pin);
/// ```
#[derive(Debug)]
pub struct OrderFlowToxicityAnalyzer {
    /// Tick-level window for PIN / Kyle / Amihud.
    window: usize,

    // ── PIN moment-matching state ─────────────────────────────────────────
    /// Rolling buffer of (buy_count_per_unit, sell_count_per_unit) observations.
    /// Here a "unit" is one tick; we track the rolling means and variances of
    /// signed volume to estimate α·μ and ε.
    buy_flow: VecDeque<f64>,
    sell_flow: VecDeque<f64>,

    // ── VPIN state ────────────────────────────────────────────────────────
    vpin_bucket_size: f64,
    vpin_window: usize,
    current_bucket: VpinBucket,
    completed_buckets: VecDeque<VpinBucket>,

    // ── Kyle λ / OLS state ────────────────────────────────────────────────
    flow_records: VecDeque<FlowRecord>,
    last_price: Option<f64>,

    // ── Amihud state ──────────────────────────────────────────────────────
    amihud_samples: VecDeque<f64>,
    last_price_for_amihud: Option<f64>,

    // ── Tick counter ──────────────────────────────────────────────────────
    tick_count: usize,
}

impl OrderFlowToxicityAnalyzer {
    /// Construct a new analyser.
    ///
    /// # Arguments
    ///
    /// * `window` — tick history depth for PIN / Kyle λ / Amihud (minimum 2).
    /// * `vpin_bucket_size` — volume units per VPIN bucket (must be > 0).
    /// * `vpin_window` — number of completed buckets to average for VPIN (must be ≥ 1).
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

    // ── Feed ──────────────────────────────────────────────────────────────

    /// Ingest one tick.  All internal state is updated in O(1) amortised time.
    pub fn update(&mut self, tick: &NormalizedTick) {
        let price = decimal_to_f64(tick.price);
        let volume = decimal_to_f64(tick.quantity);

        // Determine signed volume based on tick side; default to buy for unknown.
        let (buy_vol, sell_vol) = match tick.side {
            Some(TradeSide::Sell) => (0.0_f64, volume),
            _ => (volume, 0.0_f64),
        };
        let signed_vol = buy_vol - sell_vol;

        // ── PIN moment-matching ─────────────────────────────────────────
        if self.buy_flow.len() == self.window {
            self.buy_flow.pop_front();
            self.sell_flow.pop_front();
        }
        self.buy_flow.push_back(buy_vol);
        self.sell_flow.push_back(sell_vol);

        // ── Kyle λ records ──────────────────────────────────────────────
        let delta_price = match self.last_price {
            Some(prev) => price - prev,
            None => 0.0,
        };
        if self.flow_records.len() == self.window {
            self.flow_records.pop_front();
        }
        self.flow_records.push_back(FlowRecord { signed_volume: signed_vol, delta_price });
        self.last_price = Some(price);

        // ── Amihud ──────────────────────────────────────────────────────
        if let Some(prev_price) = self.last_price_for_amihud {
            let ret = if prev_price != 0.0 { ((price - prev_price) / prev_price).abs() } else { 0.0 };
            let illiq = if volume > 0.0 { ret / volume } else { 0.0 };
            if self.amihud_samples.len() == self.window {
                self.amihud_samples.pop_front();
            }
            self.amihud_samples.push_back(illiq);
        }
        self.last_price_for_amihud = Some(price);

        // ── VPIN bucket filling ─────────────────────────────────────────
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
                // Bucket is full — archive it.
                let finished = std::mem::take(&mut self.current_bucket);
                if self.completed_buckets.len() == self.vpin_window {
                    self.completed_buckets.pop_front();
                }
                self.completed_buckets.push_back(finished);
            }
        }

        self.tick_count += 1;
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// PIN estimate via moment-matching.
    ///
    /// Uses the relationship between the mean and variance of signed order
    /// imbalance across the rolling window to back out `α·μ` (informed flow
    /// intensity) and `ε` (uninformed arrival rate).
    ///
    /// Returns `f64::NAN` until `window` ticks have been observed.
    pub fn pin_estimate(&self) -> f64 {
        if self.buy_flow.len() < self.window {
            return f64::NAN;
        }

        let n = self.buy_flow.len() as f64;
        let mean_buy = self.buy_flow.iter().sum::<f64>() / n;
        let mean_sell = self.sell_flow.iter().sum::<f64>() / n;

        // Variance of buy and sell arrivals across the window.
        let var_buy = variance_f64(self.buy_flow.iter().copied());
        let var_sell = variance_f64(self.sell_flow.iter().copied());

        // Under the PIN model:
        //   E[B]    = ε + α·μ·p_buy   (where p_buy ≈ 0.5 for symmetric events)
        //   E[S]    = ε + α·μ·p_sell
        //   Var[B]  ≈ ε + α·μ·(1-α·μ/...) — simplified via Poisson approximation
        //
        // Moment matching (simplified):
        //   epsilon ≈ 0.5 * (mean_buy + mean_sell) / 2
        //   alpha_mu ≈ max(0, mean_buy + mean_sell - 2*epsilon) but use excess variance
        //
        // Robust approximation:  epsilon estimated from the lower of the two means
        // (uninformed component), alpha*mu from the excess.
        let epsilon = mean_buy.min(mean_sell).max(1e-9);

        // Excess variance beyond a Poisson baseline (σ² > mean implies clustering).
        let excess_var = (var_buy - mean_buy).max(0.0) + (var_sell - mean_sell).max(0.0);
        let alpha_mu = (excess_var.sqrt() + (mean_buy - epsilon).max(0.0)
            + (mean_sell - epsilon).max(0.0))
            / 2.0;
        let alpha_mu = alpha_mu.max(0.0);

        // PIN = α·μ / (α·μ + 2·ε)
        let denom = alpha_mu + 2.0 * epsilon;
        if denom <= 0.0 {
            return 0.0;
        }
        (alpha_mu / denom).clamp(0.0, 1.0)
    }

    /// Volume-synchronised PIN over the most recent completed buckets.
    ///
    /// Returns `f64::NAN` until at least one VPIN bucket has been completed.
    pub fn vpin(&self) -> f64 {
        if self.completed_buckets.is_empty() {
            return f64::NAN;
        }
        let sum: f64 = self.completed_buckets.iter().map(|b| b.imbalance_fraction()).sum();
        (sum / self.completed_buckets.len() as f64).clamp(0.0, 1.0)
    }

    /// Kyle's λ — OLS estimate of the price-impact coefficient.
    ///
    /// Regresses price changes (ΔP) on signed volume (x):
    /// `ΔP = λ·x + ε`
    ///
    /// Returns `f64::NAN` until `window` records have been accumulated, or if
    /// the variance of signed volume is essentially zero (flat flow).
    pub fn kyle_lambda(&self) -> f64 {
        if self.flow_records.len() < self.window {
            return f64::NAN;
        }

        let n = self.flow_records.len() as f64;
        let mean_x = self.flow_records.iter().map(|r| r.signed_volume).sum::<f64>() / n;
        let mean_y = self.flow_records.iter().map(|r| r.delta_price).sum::<f64>() / n;

        let cov_xy: f64 = self
            .flow_records
            .iter()
            .map(|r| (r.signed_volume - mean_x) * (r.delta_price - mean_y))
            .sum::<f64>();
        let var_x: f64 = self
            .flow_records
            .iter()
            .map(|r| (r.signed_volume - mean_x).powi(2))
            .sum::<f64>();

        if var_x.abs() < 1e-18 {
            return f64::NAN;
        }
        cov_xy / var_x
    }

    /// Amihud illiquidity ratio — rolling mean of `|return| / volume`.
    ///
    /// Returns `f64::NAN` until at least 2 ticks have been processed.
    pub fn amihud_ratio(&self) -> f64 {
        if self.amihud_samples.is_empty() {
            return f64::NAN;
        }
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

    /// Number of ticks that have been fed to the analyser so far.
    pub fn tick_count(&self) -> usize {
        self.tick_count
    }

    /// Reset all accumulated state, as if newly constructed.
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

/// Convert `rust_decimal::Decimal` to `f64` for statistical calculations.
#[inline]
fn decimal_to_f64(d: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

/// Population variance of an iterator of `f64`.
fn variance_f64(iter: impl Iterator<Item = f64> + Clone) -> f64 {
    let v: Vec<f64> = iter.collect();
    let n = v.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
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
        assert!(pin.is_finite(), "PIN should be finite after window is full");
        assert!((0.0..=1.0).contains(&pin), "PIN must be in [0,1], got {pin}");
    }

    #[test]
    fn vpin_nan_before_first_bucket() {
        let mut a = OrderFlowToxicityAnalyzer::new(20, 100, 5); // large bucket size
        for _ in 0..10 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        // Only 10 volume units fed, bucket requires 100 → no completed bucket yet.
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
    fn kyle_lambda_nan_before_window_full() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        for _ in 0..9 {
            a.update(&make_tick("100", "1", TradeSide::Buy));
        }
        assert!(a.kyle_lambda().is_nan());
    }

    #[test]
    fn kyle_lambda_positive_on_buy_pressure() {
        // Create a price series that rises with buy flow.
        let mut a = OrderFlowToxicityAnalyzer::new(20, 5, 5);
        let prices: Vec<f64> = (0..20).map(|i| 100.0 + i as f64 * 0.1).collect();
        for p in &prices {
            let s = format!("{p:.2}");
            a.update(&make_tick(&s, "10", TradeSide::Buy));
        }
        let lambda = a.kyle_lambda();
        // With rising prices and all-buy volume, lambda should be positive.
        assert!(lambda.is_finite());
        assert!(lambda >= 0.0, "lambda should be >=0 on sustained buy flow, got {lambda}");
    }

    #[test]
    fn amihud_nan_before_two_ticks() {
        let mut a = OrderFlowToxicityAnalyzer::new(10, 5, 5);
        a.update(&make_tick("100", "1", TradeSide::Buy));
        // Only one tick — no price change computed yet, so no Amihud samples.
        assert!(a.amihud_ratio().is_nan());
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
        for i in 0..7u64 {
            let mut t = make_tick("100", "1", TradeSide::Buy);
            t.received_at_ms = i;
            a.update(&t);
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
