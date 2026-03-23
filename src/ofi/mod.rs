//! # Module: ofi — Order Flow Imbalance
//!
//! ## Responsibility
//! Compute Order Flow Imbalance (OFI), a widely-used HFT signal measuring
//! instantaneous buy/sell pressure at the top of the limit order book.
//!
//! ## Definition
//!
//! OFI measures the net change in depth at the best bid and ask:
//!
//! ```text
//! OFI = ΔBid_Volume - ΔAsk_Volume
//! ```
//!
//! where:
//! - `ΔBid_Volume` = change in best-bid quantity (positive = bid deepened/improved,
//!   negative = bid thinned/worsened)
//! - `ΔAsk_Volume` = change in best-ask quantity (same convention)
//!
//! Positive OFI → buy pressure (bids accumulating, asks thinning).
//! Negative OFI → sell pressure.
//!
//! ## VPIN (Volume-Synchronized PIN)
//!
//! The `ToxicityEstimator` computes a simplified VPIN following
//! Easley, López de Prado & O'Hara (2012):
//!
//! ```text
//! VPIN = |V_buy - V_sell| / V_total
//! ```
//!
//! A VPIN approaching 1 indicates heavily imbalanced order flow, consistent
//! with informed trading or adverse selection.
//!
//! ## Architecture
//!
//! ```text
//! BookDelta stream
//!      │
//!      ▼
//! OrderFlowImbalance (per-tick OFI from top-of-book delta)
//!      │
//!      ▼
//! OfiAccumulator (rolling window → OfiSignal)
//!      │
//!      ├──► OfiMetrics (z-score, percentile rank for cross-asset comparison)
//!      │
//!      └──► ToxicityEstimator (VPIN → VpinResult)
//! ```
//!
//! ## Guarantees
//! - Non-panicking: all arithmetic uses checked paths or explicit guards.
//! - `OfiMetrics::zscore` is `0.0` when standard deviation is zero.
//! - `VpinResult::toxicity` is always in `[0.0, 1.0]`.

use crate::error::StreamError;
use rust_decimal::Decimal;
use std::collections::VecDeque;

// ─── Side ─────────────────────────────────────────────────────────────────────

/// Direction implied by the current OFI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    /// Net buying pressure (OFI > 0).
    Buy,
    /// Net selling pressure (OFI < 0).
    Sell,
    /// Balanced (OFI == 0).
    Neutral,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "Buy"),
            Side::Sell => write!(f, "Sell"),
            Side::Neutral => write!(f, "Neutral"),
        }
    }
}

// ─── NanoTimestamp ────────────────────────────────────────────────────────────

/// Nanosecond-precision timestamp for OFI events.
///
/// Stored as nanoseconds since the Unix epoch (`i64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NanoTimestamp(pub i64);

impl NanoTimestamp {
    /// Returns `0` epoch timestamp (useful in tests and stubs).
    pub fn zero() -> Self {
        Self(0)
    }

    /// Returns the underlying nanosecond value.
    pub fn as_nanos(self) -> i64 {
        self.0
    }
}

// ─── TopOfBook ────────────────────────────────────────────────────────────────

/// Snapshot of the best bid and ask at a single point in time.
///
/// Pass consecutive snapshots to [`OrderFlowImbalance::update`] to compute OFI.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct TopOfBook {
    /// Best bid price.
    pub bid_price: Decimal,
    /// Resting quantity at the best bid.
    pub bid_qty: Decimal,
    /// Best ask price.
    pub ask_price: Decimal,
    /// Resting quantity at the best ask.
    pub ask_qty: Decimal,
    /// Timestamp of this snapshot.
    pub timestamp: NanoTimestamp,
}

impl TopOfBook {
    /// Constructs a new `TopOfBook` snapshot.
    pub fn new(
        bid_price: Decimal,
        bid_qty: Decimal,
        ask_price: Decimal,
        ask_qty: Decimal,
        timestamp: NanoTimestamp,
    ) -> Self {
        Self { bid_price, bid_qty, ask_price, ask_qty, timestamp }
    }

    /// Mid-price: `(bid + ask) / 2`.
    pub fn mid_price(&self) -> Decimal {
        (self.bid_price + self.ask_price) / Decimal::from(2u32)
    }

    /// Bid-ask spread: `ask - bid`.
    pub fn spread(&self) -> Decimal {
        self.ask_price - self.bid_price
    }
}

// ─── OfiSignal ────────────────────────────────────────────────────────────────

/// A single OFI measurement produced by [`OfiAccumulator`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OfiSignal {
    /// Raw OFI value: `sum(ΔBid_qty) - sum(ΔAsk_qty)` over the window.
    pub value: f64,
    /// Implied direction of the OFI.
    pub direction: Side,
    /// Signal strength normalized to `[0.0, 1.0]` via tanh scaling.
    ///
    /// Values near 1.0 indicate very strong one-sided flow.
    pub strength: f64,
    /// Timestamp of the last tick included in this OFI window.
    pub timestamp: NanoTimestamp,
    /// Number of ticks included in the rolling window.
    pub tick_count: usize,
}

impl OfiSignal {
    fn from_raw(value: f64, scale: f64, timestamp: NanoTimestamp, tick_count: usize) -> Self {
        let direction = if value > 0.0 {
            Side::Buy
        } else if value < 0.0 {
            Side::Sell
        } else {
            Side::Neutral
        };
        // tanh maps (-∞, ∞) → (-1, 1); we take abs for strength
        let strength = if scale > 0.0 { (value / scale).tanh().abs() } else { 0.0 };
        Self { value, direction, strength, timestamp, tick_count }
    }
}

// ─── OfiMetrics ───────────────────────────────────────────────────────────────

/// Standardized OFI metrics for cross-asset comparison.
///
/// Z-score and percentile rank allow comparing OFI magnitudes across
/// instruments with different tick sizes and volumes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OfiMetrics {
    /// Rolling mean of OFI values over the lookback window.
    pub rolling_mean: f64,
    /// Rolling standard deviation of OFI values.
    pub rolling_std: f64,
    /// Z-score of the current OFI: `(ofi - mean) / std`. Zero when `std == 0`.
    pub zscore: f64,
    /// Fraction of historical OFI values below the current value, in `[0.0, 1.0]`.
    pub percentile_rank: f64,
    /// The raw OFI value this snapshot was computed from.
    pub raw_ofi: f64,
}

impl OfiMetrics {
    /// Returns `true` if the current OFI is statistically significant (|z| > threshold).
    pub fn is_significant(&self, z_threshold: f64) -> bool {
        self.zscore.abs() > z_threshold
    }
}

// ─── OrderFlowImbalance ───────────────────────────────────────────────────────

/// Computes per-tick OFI from consecutive top-of-book snapshots.
///
/// Call [`update`](Self::update) with each new `TopOfBook` snapshot.
/// OFI is defined as:
///
/// ```text
/// bid_delta = curr.bid_qty - prev.bid_qty  (if prices match; else full curr qty)
/// ask_delta = curr.ask_qty - prev.ask_qty  (if prices match; else full curr qty)
/// OFI = bid_delta - ask_delta
/// ```
///
/// # Example
/// ```rust
/// use fin_stream::ofi::{OrderFlowImbalance, TopOfBook, NanoTimestamp};
/// use rust_decimal_macros::dec;
///
/// let mut ofi = OrderFlowImbalance::new();
/// let snap1 = TopOfBook::new(dec!(100), dec!(10), dec!(100.1), dec!(8), NanoTimestamp(1_000));
/// let snap2 = TopOfBook::new(dec!(100), dec!(12), dec!(100.1), dec!(6), NanoTimestamp(2_000));
///
/// ofi.update(snap1); // first tick — sets baseline
/// let raw_ofi = ofi.update(snap2); // bid grew +2, ask shrank −2 → OFI = +4
/// assert!(raw_ofi > 0.0, "should indicate buy pressure");
/// ```
#[derive(Debug, Clone)]
pub struct OrderFlowImbalance {
    prev: Option<TopOfBook>,
    tick_count: u64,
}

impl OrderFlowImbalance {
    /// Constructs a new `OrderFlowImbalance` computer.
    pub fn new() -> Self {
        Self { prev: None, tick_count: 0 }
    }

    /// Updates with a new top-of-book snapshot and returns the current OFI tick value.
    ///
    /// Returns `0.0` on the first call (no previous snapshot to delta against).
    pub fn update(&mut self, snap: TopOfBook) -> f64 {
        self.tick_count += 1;
        let ofi = if let Some(ref prev) = self.prev {
            // Bid delta: if bid price moved, treat as full new quantity (signed by price direction)
            let bid_delta = if snap.bid_price >= prev.bid_price {
                to_f64(snap.bid_qty) - to_f64(prev.bid_qty)
            } else {
                -to_f64(snap.bid_qty)
            };
            // Ask delta: if ask price moved, treat as full new quantity
            let ask_delta = if snap.ask_price <= prev.ask_price {
                to_f64(snap.ask_qty) - to_f64(prev.ask_qty)
            } else {
                to_f64(snap.ask_qty)
            };
            bid_delta - ask_delta
        } else {
            0.0
        };
        self.prev = Some(snap);
        ofi
    }

    /// Returns the total number of ticks processed.
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Resets to initial state.
    pub fn reset(&mut self) {
        self.prev = None;
        self.tick_count = 0;
    }
}

impl Default for OrderFlowImbalance {
    fn default() -> Self {
        Self::new()
    }
}

// ─── OfiAccumulator ───────────────────────────────────────────────────────────

/// Rolling OFI accumulator over a configurable window (in ticks).
///
/// Maintains a ring buffer of per-tick OFI values and computes a rolling
/// sum, which is then returned as an [`OfiSignal`].
///
/// # Example
/// ```rust
/// use fin_stream::ofi::{OfiAccumulator, TopOfBook, NanoTimestamp};
/// use rust_decimal_macros::dec;
///
/// let mut acc = OfiAccumulator::new(10).unwrap();
/// for i in 0..15u64 {
///     let snap = TopOfBook::new(
///         dec!(100), rust_decimal::Decimal::from(i % 5 + 1),
///         dec!(100.1), rust_decimal::Decimal::from(i % 3 + 1),
///         NanoTimestamp(i as i64 * 1000),
///     );
///     let signal = acc.update(snap);
///     println!("OFI = {:.2}, direction = {}", signal.value, signal.direction);
/// }
/// ```
#[derive(Debug)]
pub struct OfiAccumulator {
    ofi: OrderFlowImbalance,
    window: VecDeque<f64>,
    window_size: usize,
    /// Normalization scale: a representative OFI magnitude for tanh scaling.
    /// Set to the running max absolute OFI, updated on each tick.
    scale: f64,
    last_timestamp: NanoTimestamp,
}

impl OfiAccumulator {
    /// Constructs a new `OfiAccumulator`.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size == 0`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size == 0 {
            return Err(StreamError::ConfigError {
                reason: "OfiAccumulator window_size must be > 0".to_owned(),
            });
        }
        Ok(Self {
            ofi: OrderFlowImbalance::new(),
            window: VecDeque::with_capacity(window_size + 1),
            window_size,
            scale: 1.0,
            last_timestamp: NanoTimestamp::zero(),
        })
    }

    /// Updates with a new top-of-book snapshot and returns the rolling [`OfiSignal`].
    pub fn update(&mut self, snap: TopOfBook) -> OfiSignal {
        self.last_timestamp = snap.timestamp;
        let tick_ofi = self.ofi.update(snap);

        self.window.push_back(tick_ofi);
        if self.window.len() > self.window_size {
            self.window.pop_front();
        }

        // Update scale (running max absolute value for tanh normalization)
        let abs_ofi = tick_ofi.abs();
        if abs_ofi > self.scale {
            self.scale = abs_ofi;
        }

        let rolling_ofi: f64 = self.window.iter().sum();
        OfiSignal::from_raw(rolling_ofi, self.scale, self.last_timestamp, self.window.len())
    }

    /// Returns the current rolling OFI sum without consuming a new tick.
    pub fn current_value(&self) -> f64 {
        self.window.iter().sum()
    }

    /// Number of ticks currently in the window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Returns `true` if no ticks have been accumulated yet.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Returns `true` when the window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.window.len() >= self.window_size
    }

    /// Resets the accumulator.
    pub fn reset(&mut self) {
        self.ofi.reset();
        self.window.clear();
        self.scale = 1.0;
        self.last_timestamp = NanoTimestamp::zero();
    }
}

// ─── OfiMetricsComputer ───────────────────────────────────────────────────────

/// Streaming standardized OFI metrics for cross-asset comparison.
///
/// Maintains a rolling history of OFI values and computes mean, std, z-score,
/// and percentile rank on each update.
///
/// # Example
/// ```rust
/// use fin_stream::ofi::{OfiMetricsComputer, OfiAccumulator, TopOfBook, NanoTimestamp};
/// use rust_decimal_macros::dec;
///
/// let mut acc = OfiAccumulator::new(5).unwrap();
/// let mut metrics = OfiMetricsComputer::new(50).unwrap();
///
/// for i in 0..60u64 {
///     let snap = TopOfBook::new(
///         dec!(100), rust_decimal::Decimal::from(i % 5 + 1),
///         dec!(100.1), rust_decimal::Decimal::from(i % 4 + 1),
///         NanoTimestamp(i as i64 * 1000),
///     );
///     let signal = acc.update(snap);
///     let m = metrics.update(signal.value);
///     if metrics.is_ready() {
///         println!("OFI zscore = {:.2}, percentile = {:.2}", m.zscore, m.percentile_rank);
///     }
/// }
/// ```
#[derive(Debug)]
pub struct OfiMetricsComputer {
    history: VecDeque<f64>,
    lookback: usize,
    /// Welford running mean.
    welford_mean: f64,
    /// Welford running M2.
    welford_m2: f64,
    welford_count: usize,
}

impl OfiMetricsComputer {
    /// Constructs a new metrics computer.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `lookback == 0`.
    pub fn new(lookback: usize) -> Result<Self, StreamError> {
        if lookback == 0 {
            return Err(StreamError::ConfigError {
                reason: "OfiMetricsComputer lookback must be > 0".to_owned(),
            });
        }
        Ok(Self {
            history: VecDeque::with_capacity(lookback + 1),
            lookback,
            welford_mean: 0.0,
            welford_m2: 0.0,
            welford_count: 0,
        })
    }

    /// Updates with a new OFI value and returns the current [`OfiMetrics`].
    pub fn update(&mut self, ofi: f64) -> OfiMetrics {
        // Welford online mean/variance
        self.welford_count += 1;
        let delta = ofi - self.welford_mean;
        self.welford_mean += delta / self.welford_count as f64;
        let delta2 = ofi - self.welford_mean;
        self.welford_m2 += delta * delta2;

        self.history.push_back(ofi);
        if self.history.len() > self.lookback {
            self.history.pop_front();
        }

        let n = self.history.len();
        let rolling_mean = if n > 0 {
            self.history.iter().sum::<f64>() / n as f64
        } else {
            0.0
        };
        let rolling_std = if n > 1 {
            let var = self.history.iter().map(|x| (x - rolling_mean).powi(2)).sum::<f64>()
                / (n as f64 - 1.0);
            var.sqrt()
        } else {
            0.0
        };
        let zscore = if rolling_std > 0.0 {
            (ofi - rolling_mean) / rolling_std
        } else {
            0.0
        };

        // Percentile rank: fraction of history strictly below current ofi
        let below = self.history.iter().filter(|&&v| v < ofi).count();
        let percentile_rank = if n > 1 { below as f64 / (n as f64 - 1.0) } else { 0.5 };

        OfiMetrics { rolling_mean, rolling_std, zscore, percentile_rank, raw_ofi: ofi }
    }

    /// Returns `true` when the lookback window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.history.len() >= self.lookback
    }

    /// Number of values in the rolling window.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Returns `true` if no values have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Resets the computer to its initial state.
    pub fn reset(&mut self) {
        self.history.clear();
        self.welford_mean = 0.0;
        self.welford_m2 = 0.0;
        self.welford_count = 0;
    }
}

// ─── VPIN / ToxicityEstimator ─────────────────────────────────────────────────

/// Result of a VPIN (Volume-synchronized PIN) computation.
///
/// Toxicity measures the probability that a trade is informed, derived from
/// volume imbalance over a rolling bucket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VpinResult {
    /// VPIN estimate in `[0.0, 1.0]`. Values near 1 indicate toxic (informed) flow.
    pub toxicity: f64,
    /// Estimated buy volume in the current bucket.
    pub buy_vol: f64,
    /// Estimated sell volume in the current bucket.
    pub sell_vol: f64,
    /// Raw volume imbalance: `|buy_vol - sell_vol|`.
    pub imbalance: f64,
    /// Total volume processed in the current bucket.
    pub total_vol: f64,
}

impl VpinResult {
    /// Returns `true` if toxicity exceeds the given threshold.
    pub fn is_toxic(&self, threshold: f64) -> bool {
        self.toxicity >= threshold
    }
}

/// Estimates order flow toxicity using a simplified VPIN formula.
///
/// VPIN (Easley, López de Prado & O'Hara, 2012) classifies each volume
/// observation as buy or sell using OFI sign, then computes:
///
/// ```text
/// VPIN = |V_buy - V_sell| / V_total
/// ```
///
/// over a rolling volume bucket.
///
/// # Example
/// ```rust
/// use fin_stream::ofi::{ToxicityEstimator, NanoTimestamp};
///
/// let mut estimator = ToxicityEstimator::new(100.0, 10).unwrap();
/// for i in 0..15 {
///     let ofi = if i < 10 { 5.0 } else { -2.0 }; // mostly buy flow
///     let vol = 10.0;
///     let result = estimator.update(ofi, vol);
///     if let Some(r) = result {
///         println!("VPIN = {:.3}", r.toxicity);
///     }
/// }
/// ```
#[derive(Debug)]
pub struct ToxicityEstimator {
    /// Target volume per bucket (V*).
    bucket_size: f64,
    /// Number of buckets in the rolling VPIN window.
    n_buckets: usize,
    /// Rolling history of per-bucket VPIN values.
    bucket_vpins: VecDeque<f64>,
    /// Accumulated buy volume in the current (incomplete) bucket.
    current_buy: f64,
    /// Accumulated sell volume in the current (incomplete) bucket.
    current_sell: f64,
    /// Volume accumulated so far in the current bucket.
    current_vol: f64,
}

impl ToxicityEstimator {
    /// Constructs a new VPIN estimator.
    ///
    /// - `bucket_size`: target volume per time bucket (V*). Must be positive.
    /// - `n_buckets`: number of completed buckets in the rolling VPIN average.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if parameters are invalid.
    pub fn new(bucket_size: f64, n_buckets: usize) -> Result<Self, StreamError> {
        if bucket_size <= 0.0 {
            return Err(StreamError::ConfigError {
                reason: "VPIN bucket_size must be positive".to_owned(),
            });
        }
        if n_buckets == 0 {
            return Err(StreamError::ConfigError {
                reason: "VPIN n_buckets must be > 0".to_owned(),
            });
        }
        Ok(Self {
            bucket_size,
            n_buckets,
            bucket_vpins: VecDeque::with_capacity(n_buckets + 1),
            current_buy: 0.0,
            current_sell: 0.0,
            current_vol: 0.0,
        })
    }

    /// Updates the estimator with an OFI value and its associated traded volume.
    ///
    /// Returns `Some(VpinResult)` once at least one bucket has been completed,
    /// or `None` while the first bucket is still accumulating.
    pub fn update(&mut self, ofi: f64, volume: f64) -> Option<VpinResult> {
        if volume <= 0.0 {
            return self.current_result();
        }

        // Classify volume as buy or sell based on OFI sign
        let buy_fraction = if ofi > 0.0 { 1.0 } else if ofi < 0.0 { 0.0 } else { 0.5 };
        let buy_vol = volume * buy_fraction;
        let sell_vol = volume * (1.0 - buy_fraction);

        self.current_buy += buy_vol;
        self.current_sell += sell_vol;
        self.current_vol += volume;

        // Check if the bucket is full
        if self.current_vol >= self.bucket_size {
            let total = self.current_vol;
            let imbalance = (self.current_buy - self.current_sell).abs();
            let bucket_vpin = if total > 0.0 { imbalance / total } else { 0.0 };

            self.bucket_vpins.push_back(bucket_vpin);
            if self.bucket_vpins.len() > self.n_buckets {
                self.bucket_vpins.pop_front();
            }

            // Reset current bucket
            self.current_buy = 0.0;
            self.current_sell = 0.0;
            self.current_vol = 0.0;
        }

        self.current_result()
    }

    fn current_result(&self) -> Option<VpinResult> {
        if self.bucket_vpins.is_empty() {
            return None;
        }
        let toxicity =
            self.bucket_vpins.iter().sum::<f64>() / self.bucket_vpins.len() as f64;
        let total = self.current_buy + self.current_sell;
        let imbalance = (self.current_buy - self.current_sell).abs();
        Some(VpinResult {
            toxicity: toxicity.clamp(0.0, 1.0),
            buy_vol: self.current_buy,
            sell_vol: self.current_sell,
            imbalance,
            total_vol: total,
        })
    }

    /// Returns `true` when at least one bucket has been completed and VPIN is available.
    pub fn is_ready(&self) -> bool {
        !self.bucket_vpins.is_empty()
    }

    /// Returns the number of completed buckets in the rolling window.
    pub fn completed_buckets(&self) -> usize {
        self.bucket_vpins.len()
    }

    /// Resets all accumulated state.
    pub fn reset(&mut self) {
        self.bucket_vpins.clear();
        self.current_buy = 0.0;
        self.current_sell = 0.0;
        self.current_vol = 0.0;
    }
}

// ─── Helper ───────────────────────────────────────────────────────────────────

fn to_f64(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

// ─── Cumulative Volume Delta ──────────────────────────────────────────────────

/// Side of a trade for CVD and flow classification purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TradeSide {
    /// Aggressor is a buyer (lifted the ask).
    Buy,
    /// Aggressor is a seller (hit the bid).
    Sell,
}

/// Cumulative Volume Delta (CVD) — running sum of buy volume − sell volume.
///
/// Rising CVD → sustained buying pressure.
/// Falling CVD → sustained selling pressure.
/// CVD divergence from price → potential reversal signal.
#[derive(Debug, Default)]
pub struct CumulativeVolumeDelta {
    /// Current cumulative delta value.
    pub cvd: f64,
    /// Total buy volume seen since last reset.
    pub total_buy: f64,
    /// Total sell volume seen since last reset.
    pub total_sell: f64,
}

impl CumulativeVolumeDelta {
    /// Create a new CVD tracker starting at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with a new trade. Returns the updated CVD.
    pub fn update_trade(&mut self, side: TradeSide, volume: f64) -> f64 {
        match side {
            TradeSide::Buy => {
                self.cvd += volume;
                self.total_buy += volume;
            }
            TradeSide::Sell => {
                self.cvd -= volume;
                self.total_sell += volume;
            }
        }
        self.cvd
    }

    /// Reset CVD to zero (e.g., at session open).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Buy/sell volume ratio for the session. Returns `None` if no sell volume.
    pub fn buy_sell_ratio(&self) -> Option<f64> {
        if self.total_sell < f64::EPSILON {
            return None;
        }
        Some(self.total_buy / self.total_sell)
    }
}

// ─── Trade Intensity ──────────────────────────────────────────────────────────

/// Rolling trade intensity: trades per second over a millisecond time window.
///
/// High intensity signals a fast market with elevated microstructure risk.
pub struct TradeIntensity {
    window_ms: i64,
    timestamps: std::collections::VecDeque<i64>,
}

impl TradeIntensity {
    /// Create a new trade intensity estimator.
    ///
    /// - `window_ms`: rolling window in milliseconds.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if `window_ms == 0`.
    pub fn new(window_ms: i64) -> Result<Self, crate::error::StreamError> {
        if window_ms <= 0 {
            return Err(crate::error::StreamError::InvalidInput(
                "window_ms must be positive".to_owned(),
            ));
        }
        Ok(Self { window_ms, timestamps: std::collections::VecDeque::new() })
    }

    /// Record a trade at `timestamp_ms`. Returns current trades-per-second.
    pub fn record(&mut self, timestamp_ms: i64) -> f64 {
        self.timestamps.push_back(timestamp_ms);
        let cutoff = timestamp_ms - self.window_ms;
        while self.timestamps.front().map_or(false, |&t| t < cutoff) {
            self.timestamps.pop_front();
        }
        self.trades_per_second()
    }

    /// Current trades-per-second based on ticks in the window.
    pub fn trades_per_second(&self) -> f64 {
        let count = self.timestamps.len() as f64;
        let window_sec = self.window_ms as f64 / 1000.0;
        if window_sec < f64::EPSILON { 0.0 } else { count / window_sec }
    }

    /// Number of trades currently in the rolling window.
    pub fn count_in_window(&self) -> usize {
        self.timestamps.len()
    }
}

// ─── Flow Classifier ──────────────────────────────────────────────────────────

/// Classification of a trade as institutional or retail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FlowKind {
    /// Trade size exceeds the institutional threshold — likely informed flow.
    Institutional,
    /// Trade size is below the threshold — likely retail/noise flow.
    Retail,
}

/// Size-heuristic trade flow classifier.
///
/// Orders above `institutional_threshold` volume are classified as institutional.
/// A rolling ratio tracks the fraction of volume attributed to institutional flow.
pub struct FlowClassifier {
    threshold: f64,
    window_ms: i64,
    /// (timestamp_ms, kind, volume)
    events: std::collections::VecDeque<(i64, FlowKind, f64)>,
}

impl FlowClassifier {
    /// Create a new flow classifier.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if threshold or window_ms is non-positive.
    pub fn new(
        institutional_threshold: f64,
        window_ms: i64,
    ) -> Result<Self, crate::error::StreamError> {
        if institutional_threshold <= 0.0 || window_ms <= 0 {
            return Err(crate::error::StreamError::InvalidInput(
                "threshold and window_ms must be positive".to_owned(),
            ));
        }
        Ok(Self {
            threshold: institutional_threshold,
            window_ms,
            events: std::collections::VecDeque::new(),
        })
    }

    /// Classify a trade. Returns `(FlowKind, institutional_volume_fraction_in_window)`.
    pub fn classify_trade(
        &mut self,
        timestamp_ms: i64,
        _side: TradeSide,
        volume: f64,
    ) -> (FlowKind, f64) {
        let kind = if volume >= self.threshold { FlowKind::Institutional } else { FlowKind::Retail };
        self.events.push_back((timestamp_ms, kind, volume));
        let cutoff = timestamp_ms - self.window_ms;
        while self.events.front().map_or(false, |e| e.0 < cutoff) {
            self.events.pop_front();
        }
        let (inst_vol, total_vol) =
            self.events.iter().fold((0.0_f64, 0.0_f64), |(iv, tv), e| {
                let inst = if e.1 == FlowKind::Institutional { e.2 } else { 0.0 };
                (iv + inst, tv + e.2)
            });
        let fraction = if total_vol > f64::EPSILON { inst_vol / total_vol } else { 0.0 };
        (kind, fraction)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn snap(bid_qty: u32, ask_qty: u32, ts: i64) -> TopOfBook {
        TopOfBook::new(
            dec!(100),
            Decimal::from(bid_qty),
            dec!(100.1),
            Decimal::from(ask_qty),
            NanoTimestamp(ts),
        )
    }

    // ── Side ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_side_display() {
        assert_eq!(Side::Buy.to_string(), "Buy");
        assert_eq!(Side::Sell.to_string(), "Sell");
        assert_eq!(Side::Neutral.to_string(), "Neutral");
    }

    // ── TopOfBook ─────────────────────────────────────────────────────────────

    #[test]
    fn test_top_of_book_mid_and_spread() {
        let t = TopOfBook::new(dec!(100), dec!(5), dec!(100.2), dec!(3), NanoTimestamp(0));
        assert_eq!(t.mid_price(), dec!(100.1));
        assert_eq!(t.spread(), dec!(0.2));
    }

    // ── OrderFlowImbalance ────────────────────────────────────────────────────

    #[test]
    fn test_ofi_first_tick_returns_zero() {
        let mut ofi = OrderFlowImbalance::new();
        let v = ofi.update(snap(10, 8, 1));
        assert_eq!(v, 0.0);
    }

    #[test]
    fn test_ofi_buy_pressure() {
        let mut ofi = OrderFlowImbalance::new();
        ofi.update(snap(10, 8, 1));
        // Bid grew +2, ask shrank -2 → OFI = (12-10) - (6-8) = 2 - (-2) = 4
        let v = ofi.update(snap(12, 6, 2));
        assert!(v > 0.0, "expected positive OFI, got {v}");
    }

    #[test]
    fn test_ofi_sell_pressure() {
        let mut ofi = OrderFlowImbalance::new();
        ofi.update(snap(10, 8, 1));
        // Bid shrank, ask grew → negative OFI
        let v = ofi.update(snap(8, 12, 2));
        assert!(v < 0.0, "expected negative OFI, got {v}");
    }

    #[test]
    fn test_ofi_tick_count() {
        let mut ofi = OrderFlowImbalance::new();
        ofi.update(snap(10, 8, 1));
        ofi.update(snap(12, 6, 2));
        assert_eq!(ofi.tick_count(), 2);
    }

    #[test]
    fn test_ofi_reset() {
        let mut ofi = OrderFlowImbalance::new();
        ofi.update(snap(10, 8, 1));
        ofi.reset();
        assert_eq!(ofi.tick_count(), 0);
        let v = ofi.update(snap(10, 8, 2));
        assert_eq!(v, 0.0); // first tick again
    }

    // ── OfiAccumulator ────────────────────────────────────────────────────────

    #[test]
    fn test_accumulator_invalid_window() {
        assert!(OfiAccumulator::new(0).is_err());
    }

    #[test]
    fn test_accumulator_ready_after_window() {
        let mut acc = OfiAccumulator::new(3).unwrap();
        assert!(!acc.is_ready());
        for i in 0..4 {
            acc.update(snap(10, 8, i));
        }
        assert!(acc.is_ready());
    }

    #[test]
    fn test_accumulator_direction_positive_ofi() {
        let mut acc = OfiAccumulator::new(5).unwrap();
        acc.update(snap(10, 10, 1)); // baseline
        for i in 2..10 {
            let sig = acc.update(snap(12, 8, i)); // consistently bid-heavy
            if sig.value > 0.0 {
                assert_eq!(sig.direction, Side::Buy);
            }
        }
    }

    #[test]
    fn test_accumulator_strength_bounds() {
        let mut acc = OfiAccumulator::new(5).unwrap();
        for i in 0..20 {
            let sig = acc.update(snap(10 + i % 3, 8 + i % 2, i as i64));
            assert!((0.0..=1.0).contains(&sig.strength));
        }
    }

    #[test]
    fn test_accumulator_reset() {
        let mut acc = OfiAccumulator::new(3).unwrap();
        for i in 0..10 {
            acc.update(snap(10, 8, i));
        }
        acc.reset();
        assert!(acc.is_empty());
        assert!(!acc.is_ready());
    }

    // ── OfiMetricsComputer ────────────────────────────────────────────────────

    #[test]
    fn test_metrics_invalid_lookback() {
        assert!(OfiMetricsComputer::new(0).is_err());
    }

    #[test]
    fn test_metrics_zscore_zero_when_constant() {
        let mut m = OfiMetricsComputer::new(10).unwrap();
        for _ in 0..10 {
            let metrics = m.update(5.0);
            // Once the window has more than one value, std will be 0 for constant series
            if m.is_ready() {
                assert_eq!(metrics.zscore, 0.0);
            }
        }
    }

    #[test]
    fn test_metrics_percentile_rank_bounds() {
        let mut m = OfiMetricsComputer::new(20).unwrap();
        for i in 0..25 {
            let metrics = m.update(i as f64 - 12.0);
            assert!((0.0..=1.0).contains(&metrics.percentile_rank));
        }
    }

    #[test]
    fn test_metrics_is_significant() {
        let metrics = OfiMetrics {
            rolling_mean: 0.0,
            rolling_std: 1.0,
            zscore: 2.5,
            percentile_rank: 0.99,
            raw_ofi: 2.5,
        };
        assert!(metrics.is_significant(2.0));
        assert!(!metrics.is_significant(3.0));
    }

    #[test]
    fn test_metrics_reset() {
        let mut m = OfiMetricsComputer::new(10).unwrap();
        for i in 0..10 {
            m.update(i as f64);
        }
        m.reset();
        assert!(m.is_empty());
        assert!(!m.is_ready());
    }

    // ── VpinResult ────────────────────────────────────────────────────────────

    #[test]
    fn test_vpin_result_is_toxic() {
        let r = VpinResult {
            toxicity: 0.75,
            buy_vol: 75.0,
            sell_vol: 25.0,
            imbalance: 50.0,
            total_vol: 100.0,
        };
        assert!(r.is_toxic(0.7));
        assert!(!r.is_toxic(0.8));
    }

    // ── ToxicityEstimator ─────────────────────────────────────────────────────

    #[test]
    fn test_toxicity_invalid_params() {
        assert!(ToxicityEstimator::new(0.0, 10).is_err());
        assert!(ToxicityEstimator::new(-1.0, 10).is_err());
        assert!(ToxicityEstimator::new(100.0, 0).is_err());
    }

    #[test]
    fn test_toxicity_none_before_first_bucket() {
        let mut est = ToxicityEstimator::new(100.0, 5).unwrap();
        // Small volume per update; bucket not filled yet
        let result = est.update(5.0, 1.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_toxicity_produced_after_bucket_complete() {
        let mut est = ToxicityEstimator::new(10.0, 3).unwrap();
        let mut got_result = false;
        for i in 0..20 {
            let ofi = if i % 3 == 0 { 1.0 } else { -0.5 };
            if let Some(r) = est.update(ofi, 2.0) {
                assert!((0.0..=1.0).contains(&r.toxicity));
                got_result = true;
            }
        }
        assert!(got_result);
    }

    #[test]
    fn test_toxicity_reset() {
        let mut est = ToxicityEstimator::new(5.0, 3).unwrap();
        for _ in 0..20 {
            est.update(1.0, 2.0);
        }
        est.reset();
        assert!(!est.is_ready());
        assert_eq!(est.completed_buckets(), 0);
    }

    #[test]
    fn test_toxicity_high_for_one_sided_flow() {
        let mut est = ToxicityEstimator::new(10.0, 3).unwrap();
        // All buy flow
        for _ in 0..50 {
            est.update(5.0, 2.0);
        }
        if let Some(r) = est.update(5.0, 2.0) {
            assert!(r.toxicity > 0.5, "one-sided flow should produce high VPIN, got {}", r.toxicity);
        }
    }

    // ── CVD tests ──────────────────────────────────────────────────────────────
    #[test]
    fn test_cvd_accumulates_buy_minus_sell() {
        let mut cvd = CumulativeVolumeDelta::new();
        cvd.update_trade(TradeSide::Buy, 100.0);
        cvd.update_trade(TradeSide::Sell, 40.0);
        assert!((cvd.cvd - 60.0).abs() < 1e-9);
    }

    #[test]
    fn test_cvd_reset() {
        let mut cvd = CumulativeVolumeDelta::new();
        cvd.update_trade(TradeSide::Buy, 100.0);
        cvd.reset();
        assert_eq!(cvd.cvd, 0.0);
    }

    // ── TradeIntensity tests ───────────────────────────────────────────────────
    #[test]
    fn test_trade_intensity_increases_with_trades() {
        let mut ti = TradeIntensity::new(1_000).unwrap();
        for i in 0..10i64 {
            ti.record(i * 50);
        }
        assert!(ti.trades_per_second() > 0.0);
    }

    #[test]
    fn test_trade_intensity_invalid_window() {
        assert!(TradeIntensity::new(0).is_err());
    }

    // ── FlowClassifier tests ───────────────────────────────────────────────────
    #[test]
    fn test_flow_classifier_institutional_large_trade() {
        let mut fc = FlowClassifier::new(100.0, 5_000).unwrap();
        let (kind, _) = fc.classify_trade(0, TradeSide::Buy, 500.0);
        assert_eq!(kind, FlowKind::Institutional);
    }

    #[test]
    fn test_flow_classifier_retail_small_trade() {
        let mut fc = FlowClassifier::new(100.0, 5_000).unwrap();
        let (kind, _) = fc.classify_trade(0, TradeSide::Buy, 10.0);
        assert_eq!(kind, FlowKind::Retail);
    }

    #[test]
    fn test_flow_classifier_fraction() {
        let mut fc = FlowClassifier::new(100.0, 10_000).unwrap();
        fc.classify_trade(0, TradeSide::Buy, 200.0);   // institutional
        fc.classify_trade(100, TradeSide::Sell, 50.0); // retail
        let (_, frac) = fc.classify_trade(200, TradeSide::Buy, 50.0); // retail
        assert!((frac - 200.0 / 300.0).abs() < 1e-9);
    }
}
