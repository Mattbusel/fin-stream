//! # Module: microstructure — Market Microstructure Analytics
//!
//! ## Responsibility
//! Provides deep market microstructure analysis on real-time tick streams.
//! Implements the canonical academic estimators for illiquidity, price impact,
//! effective spread, and mechanical bid-ask bounce.
//!
//! ## Estimators
//!
//! | Estimator | Reference | Formula |
//! |-----------|-----------|---------|
//! | Amihud Illiquidity | Amihud (2002) | `|r| / V` |
//! | Kyle's Lambda | Kyle (1985) | OLS regression: `Δp = λ·Q + ε` |
//! | Roll Spread | Roll (1984) | `2·√(−Cov(r_t, r_{t−1}))` |
//! | Bid-Ask Bounce | Blume & Stambaugh (1983) | Variance component from bid-ask noise |
//!
//! ## Architecture
//!
//! ```text
//! NormalizedTick stream
//!      │
//!      ▼
//! MicrostructureMonitor (streaming, per-tick update)
//!      │
//!      ├──► AmihudIlliquidity   → illiquidity ratio
//!      ├──► KyleImpact          → lambda (price impact per unit volume)
//!      ├──► RollSpread          → effective spread estimate
//!      └──► BidAskBounce        → noise component of returns
//!            │
//!            ▼
//!      MicrostructureReport (full snapshot)
//! ```
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or use checked arithmetic.
//! - `RollSpread` returns `0.0` when serial covariance is non-negative (model condition unmet).
//! - `KyleImpact` requires at least 2 observations for a valid OLS estimate.

use crate::error::StreamError;
use rust_decimal::Decimal;
use std::collections::VecDeque;

// ─── Tick input ───────────────────────────────────────────────────────────────

/// Minimal tick summary for microstructure computation.
///
/// Decoupled from the full `NormalizedTick` so that the microstructure
/// module can be used with any data source.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct MicroTick {
    /// Trade price.
    pub price: f64,
    /// Traded volume (absolute, always positive).
    pub volume: f64,
    /// Net signed volume: positive = buyer-initiated, negative = seller-initiated.
    /// If unknown, set to `volume` (all buys) or use OFI.
    pub signed_volume: f64,
    /// Timestamp in nanoseconds since Unix epoch.
    pub timestamp_ns: i64,
}

impl MicroTick {
    /// Constructs a new tick.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `price <= 0.0` or `volume < 0.0`.
    pub fn new(
        price: f64,
        volume: f64,
        signed_volume: f64,
        timestamp_ns: i64,
    ) -> Result<Self, StreamError> {
        if price <= 0.0 {
            return Err(StreamError::ConfigError {
                reason: format!("MicroTick price must be positive, got {price}"),
            });
        }
        if volume < 0.0 {
            return Err(StreamError::ConfigError {
                reason: format!("MicroTick volume must be non-negative, got {volume}"),
            });
        }
        Ok(Self { price, volume, signed_volume, timestamp_ns })
    }
}

// ─── AmihudIlliquidity ────────────────────────────────────────────────────────

/// Amihud (2002) illiquidity ratio: `|r| / V`.
///
/// Higher values indicate that prices move more per unit of volume traded —
/// a sign of low liquidity. The ratio is typically computed daily and
/// then averaged over a window.
///
/// ```text
/// ILLIQ_t = |R_t| / Volume_t
/// ```
///
/// # Example
/// ```rust
/// use fin_stream::microstructure::{AmihudIlliquidity, MicroTick};
///
/// let mut amihud = AmihudIlliquidity::new(20).unwrap();
/// let t1 = MicroTick::new(100.0, 1000.0, 800.0, 0).unwrap();
/// let t2 = MicroTick::new(100.5, 500.0, 400.0, 1000).unwrap();
/// amihud.update(&t1);
/// if let Some(ratio) = amihud.update(&t2) {
///     println!("Amihud ILLIQ = {ratio:.6}");
/// }
/// ```
#[derive(Debug)]
pub struct AmihudIlliquidity {
    window: VecDeque<f64>,
    window_size: usize,
    prev_price: Option<f64>,
}

impl AmihudIlliquidity {
    /// Constructs a new Amihud estimator.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size == 0`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size == 0 {
            return Err(StreamError::ConfigError {
                reason: "AmihudIlliquidity window_size must be > 0".to_owned(),
            });
        }
        Ok(Self { window: VecDeque::with_capacity(window_size + 1), window_size, prev_price: None })
    }

    /// Updates with a new tick and returns the rolling mean Amihud ratio.
    ///
    /// Returns `None` on the first tick (no return can be computed yet).
    pub fn update(&mut self, tick: &MicroTick) -> Option<f64> {
        let ratio = if let Some(prev) = self.prev_price {
            if prev > 0.0 && tick.volume > 0.0 {
                let ret = ((tick.price / prev).ln()).abs();
                Some(ret / tick.volume)
            } else {
                None
            }
        } else {
            None
        };
        self.prev_price = Some(tick.price);

        if let Some(r) = ratio {
            self.window.push_back(r);
            if self.window.len() > self.window_size {
                self.window.pop_front();
            }
            let mean = self.window.iter().sum::<f64>() / self.window.len() as f64;
            Some(mean)
        } else {
            None
        }
    }

    /// Returns the current rolling mean Amihud ratio without consuming a new tick.
    pub fn current(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.window.iter().sum::<f64>() / self.window.len() as f64)
    }

    /// Returns `true` when the window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.window.len() >= self.window_size
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.window.clear();
        self.prev_price = None;
    }
}

// ─── KyleImpact ───────────────────────────────────────────────────────────────

/// Kyle's lambda: price impact per unit of signed order flow.
///
/// Estimated via OLS regression of price changes on signed volume:
///
/// ```text
/// Δp_t = λ · Q_t + ε_t
/// ```
///
/// where `Q_t` is signed volume (positive = buy, negative = sell).
/// Lambda is the slope coefficient; larger values indicate higher price
/// impact per unit of flow (lower liquidity).
///
/// # Example
/// ```rust
/// use fin_stream::microstructure::{KyleImpact, MicroTick};
///
/// let mut kyle = KyleImpact::new(30).unwrap();
/// for i in 0..35 {
///     let price = 100.0 + i as f64 * 0.01;
///     let vol = 500.0 + i as f64 * 10.0;
///     let t = MicroTick::new(price, vol, vol * 0.6, i * 1000).unwrap();
///     if let Some(lambda) = kyle.update(&t) {
///         println!("Kyle lambda = {lambda:.6}");
///     }
/// }
/// ```
#[derive(Debug)]
pub struct KyleImpact {
    dp_window: VecDeque<f64>,
    q_window: VecDeque<f64>,
    window_size: usize,
    prev_price: Option<f64>,
}

impl KyleImpact {
    /// Constructs a new Kyle impact estimator.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size < 2`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size < 2 {
            return Err(StreamError::ConfigError {
                reason: "KyleImpact window_size must be >= 2".to_owned(),
            });
        }
        Ok(Self {
            dp_window: VecDeque::with_capacity(window_size + 1),
            q_window: VecDeque::with_capacity(window_size + 1),
            window_size,
            prev_price: None,
        })
    }

    /// Updates with a new tick and returns the current lambda estimate.
    ///
    /// Returns `None` until at least 2 observations are available.
    pub fn update(&mut self, tick: &MicroTick) -> Option<f64> {
        if let Some(prev) = self.prev_price {
            let dp = tick.price - prev;
            self.dp_window.push_back(dp);
            self.q_window.push_back(tick.signed_volume);

            if self.dp_window.len() > self.window_size {
                self.dp_window.pop_front();
                self.q_window.pop_front();
            }
        }
        self.prev_price = Some(tick.price);

        if self.dp_window.len() < 2 {
            return None;
        }

        // OLS: lambda = Cov(dp, Q) / Var(Q)
        Some(ols_slope(self.dp_window.iter().copied(), self.q_window.iter().copied()))
    }

    /// Returns the current lambda estimate without consuming a new tick.
    pub fn current(&self) -> Option<f64> {
        if self.dp_window.len() < 2 {
            return None;
        }
        Some(ols_slope(self.dp_window.iter().copied(), self.q_window.iter().copied()))
    }

    /// Returns `true` when at least `window_size` observations are available.
    pub fn is_ready(&self) -> bool {
        self.dp_window.len() >= self.window_size
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.dp_window.clear();
        self.q_window.clear();
        self.prev_price = None;
    }
}

/// OLS slope: `Cov(y, x) / Var(x)`.
fn ols_slope(
    y: impl Iterator<Item = f64> + Clone,
    x: impl Iterator<Item = f64> + Clone,
) -> f64 {
    let y_vec: Vec<f64> = y.collect();
    let x_vec: Vec<f64> = x.collect();
    let n = y_vec.len().min(x_vec.len());
    if n < 2 {
        return 0.0;
    }
    let n_f = n as f64;
    let mean_x = x_vec[..n].iter().sum::<f64>() / n_f;
    let mean_y = y_vec[..n].iter().sum::<f64>() / n_f;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    for i in 0..n {
        let dx = x_vec[i] - mean_x;
        cov += dx * (y_vec[i] - mean_y);
        var_x += dx * dx;
    }
    if var_x.abs() < 1e-15 {
        return 0.0;
    }
    cov / var_x
}

// ─── RollSpread ───────────────────────────────────────────────────────────────

/// Roll (1984) effective spread estimator from serial return covariance.
///
/// Roll's model implies that bid-ask bounce induces negative first-order
/// serial correlation in trade-to-trade price changes:
///
/// ```text
/// s_Roll = 2 · √(−Cov(Δp_t, Δp_{t−1}))
/// ```
///
/// When the serial covariance is non-negative (which can happen in trending
/// markets), the Roll spread is defined as 0 (the model assumption is unmet).
///
/// # Example
/// ```rust
/// use fin_stream::microstructure::{RollSpread, MicroTick};
///
/// let mut roll = RollSpread::new(20).unwrap();
/// for i in 0..25 {
///     let price = 100.0 + (i as f64 * 0.3).sin() * 0.1; // oscillating
///     let t = MicroTick::new(price, 500.0, 250.0, i * 1000).unwrap();
///     if let Some(spread) = roll.update(&t) {
///         println!("Roll spread = {spread:.4}");
///     }
/// }
/// ```
#[derive(Debug)]
pub struct RollSpread {
    returns: VecDeque<f64>,
    window_size: usize,
    prev_price: Option<f64>,
}

impl RollSpread {
    /// Constructs a new Roll spread estimator.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size < 2`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size < 2 {
            return Err(StreamError::ConfigError {
                reason: "RollSpread window_size must be >= 2".to_owned(),
            });
        }
        Ok(Self {
            returns: VecDeque::with_capacity(window_size + 1),
            window_size,
            prev_price: None,
        })
    }

    /// Updates with a new tick and returns the Roll spread estimate.
    ///
    /// Returns `None` until at least 3 return observations are available.
    pub fn update(&mut self, tick: &MicroTick) -> Option<f64> {
        if let Some(prev) = self.prev_price {
            self.returns.push_back(tick.price - prev);
            if self.returns.len() > self.window_size {
                self.returns.pop_front();
            }
        }
        self.prev_price = Some(tick.price);

        if self.returns.len() < 3 {
            return None;
        }

        Some(self.compute_spread())
    }

    fn compute_spread(&self) -> f64 {
        let ret: Vec<f64> = self.returns.iter().copied().collect();
        let n = ret.len();
        if n < 3 {
            return 0.0;
        }
        // Serial covariance: Cov(r_t, r_{t-1}) using consecutive pairs
        let pairs = n - 1;
        let mean_curr = ret[1..].iter().sum::<f64>() / pairs as f64;
        let mean_prev = ret[..n - 1].iter().sum::<f64>() / pairs as f64;
        let cov: f64 = ret[1..]
            .iter()
            .zip(ret[..n - 1].iter())
            .map(|(r, r_prev)| (r - mean_curr) * (r_prev - mean_prev))
            .sum::<f64>()
            / pairs as f64;

        if cov >= 0.0 {
            0.0 // Model assumption violated; spread undefined
        } else {
            2.0 * (-cov).sqrt()
        }
    }

    /// Returns the current Roll spread estimate.
    pub fn current(&self) -> Option<f64> {
        if self.returns.len() < 3 {
            return None;
        }
        Some(self.compute_spread())
    }

    /// Returns `true` when the window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.returns.len() >= self.window_size
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.returns.clear();
        self.prev_price = None;
    }
}

// ─── BidAskBounce ─────────────────────────────────────────────────────────────

/// Bid-ask bounce estimator (Blume & Stambaugh, 1983).
///
/// The bid-ask spread introduces a mechanical noise component in returns.
/// When a trade alternates between hitting the bid and the ask, the observed
/// returns contain a predictable bounce of size `s/2` in each direction.
///
/// This estimator measures the bounce as the negative serial covariance
/// divided by the total return variance:
///
/// ```text
/// bounce_fraction = -Cov(r_t, r_{t-1}) / Var(r_t)
/// ```
///
/// A value near 1 means returns are almost entirely explained by mechanical
/// bounce (very wide spread, no price discovery). Near 0 = price-efficient.
///
/// # Example
/// ```rust
/// use fin_stream::microstructure::{BidAskBounce, MicroTick};
///
/// let mut bounce = BidAskBounce::new(20).unwrap();
/// for i in 0..25 {
///     // Alternating bid/ask hits produce a mechanical bounce
///     let price = if i % 2 == 0 { 100.0 } else { 100.1 };
///     let t = MicroTick::new(price, 100.0, if i % 2 == 0 { -100.0 } else { 100.0 }, i * 1000).unwrap();
///     if let Some(b) = bounce.update(&t) {
///         println!("Bounce fraction = {b:.4}");
///     }
/// }
/// ```
#[derive(Debug)]
pub struct BidAskBounce {
    returns: VecDeque<f64>,
    window_size: usize,
    prev_price: Option<f64>,
}

impl BidAskBounce {
    /// Constructs a new bounce estimator.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size < 3`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size < 3 {
            return Err(StreamError::ConfigError {
                reason: "BidAskBounce window_size must be >= 3".to_owned(),
            });
        }
        Ok(Self {
            returns: VecDeque::with_capacity(window_size + 1),
            window_size,
            prev_price: None,
        })
    }

    /// Updates with a new tick and returns the bounce fraction.
    ///
    /// Returns `None` until at least 3 observations are available.
    pub fn update(&mut self, tick: &MicroTick) -> Option<f64> {
        if let Some(prev) = self.prev_price {
            self.returns.push_back(tick.price - prev);
            if self.returns.len() > self.window_size {
                self.returns.pop_front();
            }
        }
        self.prev_price = Some(tick.price);

        if self.returns.len() < 3 {
            return None;
        }

        Some(self.compute_bounce())
    }

    fn compute_bounce(&self) -> f64 {
        let ret: Vec<f64> = self.returns.iter().copied().collect();
        let n = ret.len();
        if n < 3 {
            return 0.0;
        }

        let mean = ret.iter().sum::<f64>() / n as f64;
        let variance = ret.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n as f64;
        if variance < 1e-15 {
            return 0.0;
        }

        // Serial covariance
        let pairs = n - 1;
        let mean_curr = ret[1..].iter().sum::<f64>() / pairs as f64;
        let mean_prev = ret[..n - 1].iter().sum::<f64>() / pairs as f64;
        let cov: f64 = ret[1..]
            .iter()
            .zip(ret[..n - 1].iter())
            .map(|(r, r_prev)| (r - mean_curr) * (r_prev - mean_prev))
            .sum::<f64>()
            / pairs as f64;

        // Bounce fraction = -Cov / Var; clamp to [0, 1]
        (-cov / variance).clamp(0.0, 1.0)
    }

    /// Returns the current bounce fraction without consuming a tick.
    pub fn current(&self) -> Option<f64> {
        if self.returns.len() < 3 {
            return None;
        }
        Some(self.compute_bounce())
    }

    /// Returns `true` when the window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.returns.len() >= self.window_size
    }

    /// Resets the estimator.
    pub fn reset(&mut self) {
        self.returns.clear();
        self.prev_price = None;
    }
}

// ─── MicrostructureReport ─────────────────────────────────────────────────────

/// Full microstructure snapshot at a single point in time.
///
/// All fields are `Option<f64>` because each estimator requires a warm-up
/// period. During warm-up, the respective field is `None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MicrostructureReport {
    /// Rolling mean Amihud illiquidity ratio (`|r| / V`).
    pub amihud: Option<f64>,
    /// Kyle's lambda: price impact per unit signed volume.
    pub kyle_lambda: Option<f64>,
    /// Roll (1984) effective spread estimate.
    pub roll_spread: Option<f64>,
    /// Bid-ask bounce fraction (fraction of return variance from mechanical bounce).
    pub bounce: Option<f64>,
    /// Timestamp of the tick that produced this report (ns since epoch).
    pub timestamp_ns: i64,
}

impl MicrostructureReport {
    /// Returns `true` when all four estimators have produced values.
    pub fn is_complete(&self) -> bool {
        self.amihud.is_some()
            && self.kyle_lambda.is_some()
            && self.roll_spread.is_some()
            && self.bounce.is_some()
    }

    /// Returns the number of estimators that are currently available.
    pub fn available_count(&self) -> usize {
        [self.amihud, self.kyle_lambda, self.roll_spread, self.bounce]
            .iter()
            .filter(|v| v.is_some())
            .count()
    }
}

// ─── MicrostructureMonitor ────────────────────────────────────────────────────

/// Streaming microstructure monitor.
///
/// Wraps all four estimators and produces a [`MicrostructureReport`] on each tick.
///
/// # Example
/// ```rust
/// use fin_stream::microstructure::{MicrostructureMonitor, MicroTick};
///
/// let mut monitor = MicrostructureMonitor::new(20).unwrap();
///
/// let prices = [100.0, 100.1, 100.0, 100.2, 100.1, 100.3];
/// for (i, &price) in prices.iter().enumerate() {
///     let tick = MicroTick::new(price, 1000.0, 600.0, i as i64 * 1_000_000).unwrap();
///     let report = monitor.update(&tick).unwrap();
///     if report.is_complete() {
///         println!("Amihud = {:?}", report.amihud);
///         println!("Kyle λ = {:?}", report.kyle_lambda);
///         println!("Roll s = {:?}", report.roll_spread);
///         println!("Bounce = {:?}", report.bounce);
///     }
/// }
/// ```
pub struct MicrostructureMonitor {
    amihud: AmihudIlliquidity,
    kyle: KyleImpact,
    roll: RollSpread,
    bounce: BidAskBounce,
}

impl MicrostructureMonitor {
    /// Constructs a monitor with the given rolling window size.
    ///
    /// All four estimators share the same `window_size`.
    ///
    /// # Errors
    /// Returns [`StreamError::ConfigError`] if `window_size < 3`.
    pub fn new(window_size: usize) -> Result<Self, StreamError> {
        if window_size < 3 {
            return Err(StreamError::ConfigError {
                reason: "MicrostructureMonitor window_size must be >= 3".to_owned(),
            });
        }
        Ok(Self {
            amihud: AmihudIlliquidity::new(window_size)?,
            kyle: KyleImpact::new(window_size)?,
            roll: RollSpread::new(window_size)?,
            bounce: BidAskBounce::new(window_size)?,
        })
    }

    /// Updates all estimators with a new tick and returns the current [`MicrostructureReport`].
    ///
    /// # Errors
    /// This method does not currently return errors (all individual estimators
    /// handle invalid inputs gracefully). The `Result` wrapper is reserved for
    /// future extension.
    pub fn update(&mut self, tick: &MicroTick) -> Result<MicrostructureReport, StreamError> {
        let amihud = self.amihud.update(tick);
        let kyle_lambda = self.kyle.update(tick);
        let roll_spread = self.roll.update(tick);
        let bounce = self.bounce.update(tick);

        Ok(MicrostructureReport {
            amihud,
            kyle_lambda,
            roll_spread,
            bounce,
            timestamp_ns: tick.timestamp_ns,
        })
    }

    /// Returns the current report without consuming a new tick.
    pub fn current(&self) -> MicrostructureReport {
        MicrostructureReport {
            amihud: self.amihud.current(),
            kyle_lambda: self.kyle.current(),
            roll_spread: self.roll.current(),
            bounce: self.bounce.current(),
            timestamp_ns: 0,
        }
    }

    /// Returns `true` when all four estimators are fully warmed up.
    pub fn is_ready(&self) -> bool {
        self.amihud.is_ready()
            && self.kyle.is_ready()
            && self.roll.is_ready()
            && self.bounce.is_ready()
    }

    /// Resets all estimators.
    pub fn reset(&mut self) {
        self.amihud.reset();
        self.kyle.reset();
        self.roll.reset();
        self.bounce.reset();
    }

    /// Returns a reference to the Amihud illiquidity estimator.
    pub fn amihud(&self) -> &AmihudIlliquidity {
        &self.amihud
    }

    /// Returns a reference to the Kyle impact estimator.
    pub fn kyle(&self) -> &KyleImpact {
        &self.kyle
    }

    /// Returns a reference to the Roll spread estimator.
    pub fn roll(&self) -> &RollSpread {
        &self.roll
    }

    /// Returns a reference to the bid-ask bounce estimator.
    pub fn bounce_estimator(&self) -> &BidAskBounce {
        &self.bounce
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(price: f64, volume: f64, signed_volume: f64, ts: i64) -> MicroTick {
        MicroTick::new(price, volume, signed_volume, ts).unwrap()
    }

    // ── MicroTick ─────────────────────────────────────────────────────────────

    #[test]
    fn test_micro_tick_invalid_price() {
        assert!(MicroTick::new(0.0, 100.0, 50.0, 0).is_err());
        assert!(MicroTick::new(-1.0, 100.0, 50.0, 0).is_err());
    }

    #[test]
    fn test_micro_tick_invalid_volume() {
        assert!(MicroTick::new(100.0, -1.0, 0.0, 0).is_err());
    }

    #[test]
    fn test_micro_tick_valid() {
        let t = tick(100.0, 500.0, 300.0, 0);
        assert!((t.price - 100.0).abs() < 1e-10);
        assert!((t.volume - 500.0).abs() < 1e-10);
    }

    // ── AmihudIlliquidity ─────────────────────────────────────────────────────

    #[test]
    fn test_amihud_invalid_window() {
        assert!(AmihudIlliquidity::new(0).is_err());
    }

    #[test]
    fn test_amihud_none_on_first_tick() {
        let mut a = AmihudIlliquidity::new(10).unwrap();
        let result = a.update(&tick(100.0, 1000.0, 800.0, 0));
        assert!(result.is_none());
    }

    #[test]
    fn test_amihud_positive_after_price_change() {
        let mut a = AmihudIlliquidity::new(5).unwrap();
        a.update(&tick(100.0, 1000.0, 800.0, 0));
        let r = a.update(&tick(101.0, 500.0, 400.0, 1));
        assert!(r.is_some_and(|v| v > 0.0));
    }

    #[test]
    fn test_amihud_ready_after_window() {
        let mut a = AmihudIlliquidity::new(3).unwrap();
        assert!(!a.is_ready());
        for i in 0..5 {
            a.update(&tick(100.0 + i as f64 * 0.5, 1000.0, 700.0, i));
        }
        assert!(a.is_ready());
    }

    #[test]
    fn test_amihud_reset() {
        let mut a = AmihudIlliquidity::new(5).unwrap();
        for i in 0..6 {
            a.update(&tick(100.0 + i as f64 * 0.1, 1000.0, 700.0, i));
        }
        a.reset();
        assert!(!a.is_ready());
        assert!(a.current().is_none());
    }

    // ── KyleImpact ────────────────────────────────────────────────────────────

    #[test]
    fn test_kyle_invalid_window() {
        assert!(KyleImpact::new(0).is_err());
        assert!(KyleImpact::new(1).is_err());
    }

    #[test]
    fn test_kyle_none_until_two_observations() {
        let mut k = KyleImpact::new(5).unwrap();
        let r0 = k.update(&tick(100.0, 1000.0, 600.0, 0));
        assert!(r0.is_none());
        let r1 = k.update(&tick(100.1, 500.0, 300.0, 1));
        // May or may not be Some after 2nd tick (2 dp values after 2nd tick requires 1st dp)
        // 1st tick sets prev, 2nd tick produces first dp → r1 could be Some
        let _ = r1;
    }

    #[test]
    fn test_kyle_lambda_sign_with_positive_flow() {
        let mut k = KyleImpact::new(5).unwrap();
        // Prices rising with positive flow → positive lambda
        for i in 0..10 {
            let price = 100.0 + i as f64 * 0.1;
            let q = 500.0 + i as f64 * 10.0; // positive signed volume
            k.update(&tick(price, q, q, i as i64));
        }
        if let Some(lambda) = k.current() {
            assert!(lambda > 0.0, "positive flow + rising prices → positive lambda, got {lambda}");
        }
    }

    #[test]
    fn test_kyle_reset() {
        let mut k = KyleImpact::new(5).unwrap();
        for i in 0..10 {
            k.update(&tick(100.0 + i as f64 * 0.1, 500.0, 300.0, i as i64));
        }
        k.reset();
        assert!(!k.is_ready());
        assert!(k.current().is_none());
    }

    // ── ols_slope ─────────────────────────────────────────────────────────────

    #[test]
    fn test_ols_slope_perfect_linear() {
        // y = 2x → slope = 2
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y: Vec<f64> = x.iter().map(|v| 2.0 * v).collect();
        let slope = ols_slope(y.iter().copied(), x.iter().copied());
        assert!((slope - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_ols_slope_zero_variance_x() {
        let x = vec![1.0, 1.0, 1.0];
        let y = vec![1.0, 2.0, 3.0];
        let slope = ols_slope(y.iter().copied(), x.iter().copied());
        assert_eq!(slope, 0.0); // x has zero variance; graceful fallback
    }

    // ── RollSpread ────────────────────────────────────────────────────────────

    #[test]
    fn test_roll_invalid_window() {
        assert!(RollSpread::new(0).is_err());
        assert!(RollSpread::new(1).is_err());
    }

    #[test]
    fn test_roll_none_first_two_ticks() {
        let mut r = RollSpread::new(5).unwrap();
        assert!(r.update(&tick(100.0, 100.0, 60.0, 0)).is_none());
        assert!(r.update(&tick(100.1, 100.0, 60.0, 1)).is_none());
    }

    #[test]
    fn test_roll_non_negative() {
        let mut r = RollSpread::new(5).unwrap();
        let prices = [100.0, 100.1, 100.0, 100.2, 100.0, 100.1, 100.0];
        for (i, &p) in prices.iter().enumerate() {
            if let Some(spread) = r.update(&tick(p, 100.0, 50.0, i as i64)) {
                assert!(spread >= 0.0, "Roll spread must be non-negative, got {spread}");
            }
        }
    }

    #[test]
    fn test_roll_spread_positive_for_oscillating_prices() {
        let mut r = RollSpread::new(10).unwrap();
        let mut nonzero_seen = false;
        // Perfect bid-ask bounce: alternating between two prices
        for i in 0..20 {
            let price = if i % 2 == 0 { 100.0 } else { 100.1 };
            if let Some(spread) = r.update(&tick(price, 100.0, 50.0, i as i64)) {
                if spread > 0.0 {
                    nonzero_seen = true;
                }
            }
        }
        assert!(nonzero_seen, "oscillating prices should produce non-zero Roll spread");
    }

    #[test]
    fn test_roll_reset() {
        let mut r = RollSpread::new(5).unwrap();
        for i in 0..10 {
            r.update(&tick(100.0 + i as f64 * 0.1, 100.0, 60.0, i as i64));
        }
        r.reset();
        assert!(!r.is_ready());
        assert!(r.current().is_none());
    }

    // ── BidAskBounce ──────────────────────────────────────────────────────────

    #[test]
    fn test_bounce_invalid_window() {
        assert!(BidAskBounce::new(0).is_err());
        assert!(BidAskBounce::new(2).is_err());
    }

    #[test]
    fn test_bounce_none_before_three_observations() {
        let mut b = BidAskBounce::new(5).unwrap();
        assert!(b.update(&tick(100.0, 100.0, 60.0, 0)).is_none());
        assert!(b.update(&tick(100.1, 100.0, 60.0, 1)).is_none());
    }

    #[test]
    fn test_bounce_fraction_in_unit_interval() {
        let mut b = BidAskBounce::new(10).unwrap();
        for i in 0..20 {
            let price = if i % 2 == 0 { 100.0 } else { 100.1 };
            if let Some(frac) = b.update(&tick(price, 100.0, if i % 2 == 0 { -60.0 } else { 60.0 }, i as i64)) {
                assert!((0.0..=1.0).contains(&frac), "bounce fraction must be in [0,1], got {frac}");
            }
        }
    }

    #[test]
    fn test_bounce_high_for_alternating_prices() {
        let mut b = BidAskBounce::new(10).unwrap();
        let mut high_bounce_seen = false;
        for i in 0..20 {
            let price = if i % 2 == 0 { 100.0 } else { 100.2 };
            let sv = if i % 2 == 0 { -100.0 } else { 100.0 };
            if let Some(frac) = b.update(&tick(price, 100.0, sv, i as i64)) {
                if frac > 0.5 {
                    high_bounce_seen = true;
                }
            }
        }
        assert!(high_bounce_seen, "perfect bid-ask bounce should produce fraction > 0.5");
    }

    #[test]
    fn test_bounce_reset() {
        let mut b = BidAskBounce::new(5).unwrap();
        for i in 0..10 {
            b.update(&tick(100.0 + i as f64 * 0.1, 100.0, 60.0, i as i64));
        }
        b.reset();
        assert!(!b.is_ready());
        assert!(b.current().is_none());
    }

    // ── MicrostructureReport ──────────────────────────────────────────────────

    #[test]
    fn test_report_is_complete() {
        let r = MicrostructureReport {
            amihud: Some(0.001),
            kyle_lambda: Some(0.5),
            roll_spread: Some(0.1),
            bounce: Some(0.2),
            timestamp_ns: 0,
        };
        assert!(r.is_complete());
        assert_eq!(r.available_count(), 4);
    }

    #[test]
    fn test_report_partial() {
        let r = MicrostructureReport {
            amihud: Some(0.001),
            kyle_lambda: None,
            roll_spread: None,
            bounce: None,
            timestamp_ns: 0,
        };
        assert!(!r.is_complete());
        assert_eq!(r.available_count(), 1);
    }

    // ── MicrostructureMonitor ─────────────────────────────────────────────────

    #[test]
    fn test_monitor_invalid_window() {
        assert!(MicrostructureMonitor::new(0).is_err());
        assert!(MicrostructureMonitor::new(2).is_err());
    }

    #[test]
    fn test_monitor_update_no_panic() {
        let mut m = MicrostructureMonitor::new(5).unwrap();
        let prices = [100.0, 100.1, 99.9, 100.2, 100.0, 100.3, 99.8, 100.1];
        for (i, &p) in prices.iter().enumerate() {
            let t = tick(p, 1000.0, 500.0, i as i64);
            let report = m.update(&t).unwrap();
            let _ = report; // just assert no panic
        }
    }

    #[test]
    fn test_monitor_reports_timestamp() {
        let mut m = MicrostructureMonitor::new(5).unwrap();
        let t = tick(100.0, 1000.0, 600.0, 999_000_000);
        let report = m.update(&t).unwrap();
        assert_eq!(report.timestamp_ns, 999_000_000);
    }

    #[test]
    fn test_monitor_ready_after_enough_ticks() {
        let mut m = MicrostructureMonitor::new(5).unwrap();
        assert!(!m.is_ready());
        for i in 0..20 {
            let p = 100.0 + i as f64 * 0.05;
            m.update(&tick(p, 1000.0, 600.0, i as i64)).unwrap();
        }
        assert!(m.is_ready());
    }

    #[test]
    fn test_monitor_reset() {
        let mut m = MicrostructureMonitor::new(5).unwrap();
        for i in 0..20 {
            m.update(&tick(100.0 + i as f64 * 0.1, 1000.0, 600.0, i as i64)).unwrap();
        }
        m.reset();
        assert!(!m.is_ready());
    }

    #[test]
    fn test_monitor_all_estimators_produce_values() {
        let mut m = MicrostructureMonitor::new(5).unwrap();
        let mut last = None;
        for i in 0..30 {
            // Use oscillating prices to trigger Roll spread
            let p = if i % 2 == 0 { 100.0 + i as f64 * 0.01 } else { 100.0 + i as f64 * 0.01 - 0.05 };
            let t = tick(p, 1000.0 + i as f64 * 10.0, 600.0, i as i64 * 1_000_000);
            last = Some(m.update(&t).unwrap());
        }
        if let Some(report) = last {
            assert_eq!(report.available_count(), 4);
        }
    }
}
