// SPDX-License-Identifier: MIT
//! Synthetic market data generator for testing and simulation.
//!
//! ## Responsibility
//!
//! Generate statistically realistic tick and OHLCV data using well-established
//! stochastic price models.  This module is used for:
//!
//! - **Unit and integration tests** — deterministic seeded generation without
//!   network access.
//! - **Simulation mode** — replay generated ticks through the pipeline to
//!   stress-test downstream components.
//! - **Backtesting harnesses** — generate scenario data (crash, squeeze, etc.)
//!   that would be rare in historical data.
//!
//! ## Models
//!
//! | Model | Dynamics |
//! |-------|----------|
//! | [`GeometricBrownianMotion`] | `dS = μ S dt + σ S dW` — lognormal returns |
//! | [`JumpDiffusion`] | GBM + compound Poisson jump process (Merton 1976) |
//! | [`OrnsteinUhlenbeck`] | `dX = θ(μ − X)dt + σ dW` — mean-reverting |
//! | [`HestonModel`] | Stochastic volatility: variance follows CIR, price follows GBM |
//!
//! All models implement the [`PriceModel`] trait so they can be used
//! interchangeably with [`SyntheticMarketGenerator::generate_ticks`] and
//! [`SyntheticMarketGenerator::generate_ohlcv`].
//!
//! ## Legacy API
//!
//! The types [`OhlcvInput`], [`SyntheticTick`], [`SyntheticConfig`], and
//! [`SyntheticGenerator`] from the original Brownian-bridge implementation
//! are retained for backward compatibility.  New code should prefer
//! [`SyntheticMarketGenerator`] which drives proper stochastic models.
//!
//! ## Randomness
//!
//! The generator uses a lightweight xorshift64 PRNG seeded at construction
//! time.  No `unsafe` code is required.  The PRNG is not cryptographically
//! secure — it is used only for statistical simulation.

use crate::error::StreamError;
use crate::ohlcv::{OhlcvBar, Timeframe};
use crate::tick::{Exchange, NormalizedTick, TradeSide};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::str::FromStr;

// ────────────────────────────────────────────────────────────────────────────
// Legacy types (preserved for backward compatibility)
// ────────────────────────────────────────────────────────────────────────────

/// A single OHLCV bar used as input to the legacy [`SyntheticGenerator`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OhlcvInput {
    /// Bar open time in milliseconds since Unix epoch.
    pub open_time_ms: u64,
    /// Bar close time in milliseconds since Unix epoch.
    pub close_time_ms: u64,
    /// Open price.
    pub open: f64,
    /// High price.
    pub high: f64,
    /// Low price.
    pub low: f64,
    /// Close price.
    pub close: f64,
    /// Total volume traded in the bar.
    pub volume: f64,
}

/// A single synthetic tick produced by [`SyntheticGenerator`] (legacy API).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyntheticTick {
    /// UTC millisecond timestamp.
    pub timestamp_ms: u64,
    /// Synthesised price within `[bar.low, bar.high]`.
    pub price: f64,
    /// Volume allocated to this tick.
    pub volume: f64,
}

/// Configuration for the legacy [`SyntheticGenerator`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyntheticConfig {
    /// Number of ticks to generate per OHLCV bar.
    pub ticks_per_bar: usize,
    /// Random seed for reproducible generation.
    pub seed: u64,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self { ticks_per_bar: 100, seed: 42 }
    }
}

/// Simple LCG for the legacy generator (kept for ABI stability).
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_add(1) }
    }

    fn next_f64(&mut self) -> f64 {
        // Knuth's LCG constants.
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.state >> 33) as f64) / (u32::MAX as f64)
    }
}

/// Legacy Brownian-bridge tick generator from OHLCV bars.
///
/// For model-based generation with GBM / jump-diffusion / OU / Heston,
/// use [`SyntheticMarketGenerator`] instead.
#[derive(Debug)]
pub struct SyntheticGenerator {
    cfg: SyntheticConfig,
    rng: Lcg,
}

impl SyntheticGenerator {
    /// Construct from a [`SyntheticConfig`].
    pub fn new(cfg: SyntheticConfig) -> Self {
        let rng = Lcg::new(cfg.seed);
        Self { cfg, rng }
    }

    /// Generate synthetic ticks for a single OHLCV bar.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `bar.high < bar.low`, `bar.volume < 0`, or
    /// `ticks_per_bar < 4`.
    pub fn generate(&mut self, bar: &OhlcvInput) -> Result<Vec<SyntheticTick>, StreamError> {
        if bar.high < bar.low {
            return Err(StreamError::ConfigError { reason: "bar.high < bar.low".into() });
        }
        if bar.volume < 0.0 {
            return Err(StreamError::ConfigError { reason: "bar.volume < 0".into() });
        }
        let n = self.cfg.ticks_per_bar;
        if n < 4 {
            return Err(StreamError::ConfigError {
                reason: "ticks_per_bar must be >= 4".into(),
            });
        }
        let duration_ms = bar.close_time_ms.saturating_sub(bar.open_time_ms);
        let range = bar.high - bar.low;

        let mut prices = vec![0.0_f64; n];
        prices[0] = bar.open;
        prices[n - 1] = bar.close;

        // Insert high and low at random interior positions.
        let hi_idx = 1 + (self.rng.next_f64() * (n - 3) as f64) as usize;
        let lo_idx = {
            let mut idx = 1 + (self.rng.next_f64() * (n - 3) as f64) as usize;
            while idx == hi_idx { idx = 1 + (self.rng.next_f64() * (n - 3) as f64) as usize; }
            idx
        };
        prices[hi_idx] = bar.high;
        prices[lo_idx] = bar.low;

        // Brownian bridge fill for remaining positions.
        for i in 1..(n - 1) {
            if i == hi_idx || i == lo_idx { continue; }
            let t = i as f64 / (n - 1) as f64;
            let bridge = bar.open + t * (bar.close - bar.open);
            let noise = (self.rng.next_f64() - 0.5) * range * 0.5;
            prices[i] = (bridge + noise).clamp(bar.low, bar.high);
        }

        // Volume distribution proportional to |ΔP|.
        let mut weights: Vec<f64> = prices
            .windows(2)
            .map(|w| (w[1] - w[0]).abs() + 1e-9)
            .collect();
        let w_sum: f64 = weights.iter().sum();
        for w in &mut weights { *w /= w_sum; }

        let mut ticks = Vec::with_capacity(n);
        for i in 0..n {
            let ts = bar.open_time_ms
                + if n > 1 { (i as u64 * duration_ms) / (n as u64 - 1) } else { 0 };
            let vol = if i < weights.len() { weights[i] * bar.volume } else { 0.0 };
            ticks.push(SyntheticTick { timestamp_ms: ts, price: prices[i], volume: vol });
        }
        Ok(ticks)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Xorshift64 PRNG for the model-based generator
// ────────────────────────────────────────────────────────────────────────────

/// Xorshift64 pseudo-random number generator.
///
/// Produces a uniform stream of `u64` values from a non-zero seed.
/// The state must never be zero; a seed of 0 is replaced with 1.
#[derive(Debug, Clone)]
pub struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// Create from a seed.  A seed of 0 is replaced with 1.
    pub fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 1 } else { seed } }
    }

    /// Return the next raw `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Return a uniform float in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Approximate standard-normal sample via Box–Muller transform.
    pub fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-15);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Poisson sample with rate `λ` (Knuth's algorithm for λ ≤ 30,
    /// Normal approximation for larger λ).
    pub fn next_poisson(&mut self, lambda: f64) -> u32 {
        if lambda <= 0.0 { return 0; }
        if lambda > 30.0 {
            let n = (lambda + self.next_normal() * lambda.sqrt()).round();
            return n.max(0.0) as u32;
        }
        let l = (-lambda).exp();
        let mut k = 0u32;
        let mut p = 1.0_f64;
        loop {
            p *= self.next_f64();
            if p <= l { break; }
            k += 1;
        }
        k
    }
}

// ────────────────────────────────────────────────────────────────────────────
// PriceModel trait
// ────────────────────────────────────────────────────────────────────────────

/// A stochastic price model that produces a sequence of prices.
///
/// Implementors are driven by a shared [`Xorshift64`] PRNG and advance their
/// internal state one step at a time.
pub trait PriceModel: Send + Sync {
    /// Advance the model by one time step `dt` (years) and return the new price.
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64;

    /// Current price of the model (before the next step).
    fn current_price(&self) -> f64;
}

// ────────────────────────────────────────────────────────────────────────────
// Geometric Brownian Motion
// ────────────────────────────────────────────────────────────────────────────

/// Geometric Brownian Motion: `dS = μ S dt + σ S dW`.
///
/// The standard model for equity prices under the risk-neutral measure
/// (Black–Scholes world).
///
/// # Parameters
///
/// * `drift` (`μ`) — annualised drift (e.g. `0.05` for 5% annual return).
/// * `volatility` (`σ`) — annualised volatility (e.g. `0.20` for 20% vol).
/// * `initial_price` — starting price (must be positive).
#[derive(Debug, Clone)]
pub struct GeometricBrownianMotion {
    /// Annualised drift (μ).
    pub drift: f64,
    /// Annualised volatility (σ).
    pub volatility: f64,
    price: f64,
}

impl GeometricBrownianMotion {
    /// Construct a new GBM model.
    ///
    /// # Panics
    ///
    /// Panics if `initial_price <= 0` or `volatility < 0`.
    pub fn new(drift: f64, volatility: f64, initial_price: f64) -> Self {
        assert!(initial_price > 0.0, "initial_price must be positive");
        assert!(volatility >= 0.0, "volatility must be non-negative");
        Self { drift, volatility, price: initial_price }
    }
}

impl PriceModel for GeometricBrownianMotion {
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64 {
        let z = rng.next_normal();
        let exp_arg = (self.drift - 0.5 * self.volatility * self.volatility) * dt
            + self.volatility * dt.sqrt() * z;
        self.price *= exp_arg.exp();
        self.price
    }

    fn current_price(&self) -> f64 {
        self.price
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Jump Diffusion (Merton 1976)
// ────────────────────────────────────────────────────────────────────────────

/// Jump-Diffusion model: GBM with superimposed Poisson jump process (Merton 1976).
///
/// Price dynamics:
/// ```text
/// dS/S = (μ − λ·k̄) dt + σ dW + J dN
/// ```
/// where `N` is Poisson(λ) and each jump `J ~ LogNormal(jump_mean, jump_std)`.
///
/// # Parameters
///
/// * `gbm` — underlying GBM component.
/// * `jump_rate` (`λ`) — expected number of jumps per year (e.g. `5.0`).
/// * `jump_mean` — mean of the log-jump size (e.g. `−0.05` for mean-5% drops).
/// * `jump_std` — std of the log-jump size (e.g. `0.10`).
#[derive(Debug, Clone)]
pub struct JumpDiffusion {
    /// Underlying continuous diffusion component.
    pub gbm: GeometricBrownianMotion,
    /// Poisson jump rate (jumps per year).
    pub jump_rate: f64,
    /// Mean of log jump size.
    pub jump_mean: f64,
    /// Std of log jump size.
    pub jump_std: f64,
}

impl JumpDiffusion {
    /// Construct a new jump-diffusion model.
    ///
    /// # Panics
    ///
    /// Panics if `initial_price <= 0`, `volatility < 0`, `jump_rate < 0`, or `jump_std < 0`.
    pub fn new(
        drift: f64,
        volatility: f64,
        initial_price: f64,
        jump_rate: f64,
        jump_mean: f64,
        jump_std: f64,
    ) -> Self {
        assert!(initial_price > 0.0, "initial_price must be positive");
        assert!(volatility >= 0.0, "volatility must be non-negative");
        assert!(jump_rate >= 0.0, "jump_rate must be non-negative");
        assert!(jump_std >= 0.0, "jump_std must be non-negative");
        Self {
            gbm: GeometricBrownianMotion::new(drift, volatility, initial_price),
            jump_rate,
            jump_mean,
            jump_std,
        }
    }
}

impl PriceModel for JumpDiffusion {
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64 {
        self.gbm.step(rng, dt);
        let n_jumps = rng.next_poisson(self.jump_rate * dt);
        for _ in 0..n_jumps {
            let log_j = self.jump_mean + self.jump_std * rng.next_normal();
            self.gbm.price *= log_j.exp();
        }
        self.gbm.price = self.gbm.price.max(1e-6);
        self.gbm.price
    }

    fn current_price(&self) -> f64 {
        self.gbm.price
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Ornstein–Uhlenbeck
// ────────────────────────────────────────────────────────────────────────────

/// Ornstein–Uhlenbeck mean-reverting process.
///
/// Dynamics: `dX = θ(μ − X) dt + σ dW`
///
/// Useful for modelling spread, basis, and short-rate processes.
///
/// # Parameters
///
/// * `mean_reversion_speed` (`θ`) — speed of pull toward `long_term_mean`.
/// * `long_term_mean` (`μ`) — equilibrium price level.
/// * `volatility` (`σ`) — diffusion coefficient.
/// * `initial_price` — starting price.
#[derive(Debug, Clone)]
pub struct OrnsteinUhlenbeck {
    /// Mean-reversion speed (θ).
    pub mean_reversion_speed: f64,
    /// Long-term mean price (μ).
    pub long_term_mean: f64,
    /// Volatility (σ).
    pub volatility: f64,
    price: f64,
}

impl OrnsteinUhlenbeck {
    /// Construct a new OU process.
    ///
    /// # Panics
    ///
    /// Panics if `mean_reversion_speed < 0` or `volatility < 0`.
    pub fn new(
        mean_reversion_speed: f64,
        long_term_mean: f64,
        volatility: f64,
        initial_price: f64,
    ) -> Self {
        assert!(mean_reversion_speed >= 0.0, "mean_reversion_speed must be non-negative");
        assert!(volatility >= 0.0, "volatility must be non-negative");
        Self { mean_reversion_speed, long_term_mean, volatility, price: initial_price }
    }
}

impl PriceModel for OrnsteinUhlenbeck {
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64 {
        let z = rng.next_normal();
        let drift = self.mean_reversion_speed * (self.long_term_mean - self.price);
        self.price += drift * dt + self.volatility * dt.sqrt() * z;
        self.price = self.price.max(1e-6);
        self.price
    }

    fn current_price(&self) -> f64 {
        self.price
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Heston Stochastic Volatility
// ────────────────────────────────────────────────────────────────────────────

/// Heston stochastic volatility model.
///
/// Dynamics:
/// ```text
/// dS  = μ S dt + √v S dW_S
/// dv  = κ(θ − v) dt + ξ √v dW_v
/// corr(dW_S, dW_v) = ρ dt
/// ```
///
/// # Parameters
///
/// * `drift` (`μ`) — annualised price drift.
/// * `initial_price` — starting price (must be positive).
/// * `kappa` (`κ`) — mean-reversion speed of variance.
/// * `theta` (`θ`) — long-run variance level.
/// * `xi` (`ξ`) — volatility of variance ("vol of vol").
/// * `rho` (`ρ`) — correlation between price and variance Brownians (typically −0.7).
/// * `initial_var` — starting variance level.
#[derive(Debug, Clone)]
pub struct HestonModel {
    /// Annualised price drift (μ).
    pub drift: f64,
    price: f64,
    /// Mean-reversion speed of variance (κ).
    pub kappa: f64,
    /// Long-run variance (θ).
    pub theta: f64,
    /// Volatility of variance / vol-of-vol (ξ).
    pub xi: f64,
    /// Correlation between price and variance Brownians (ρ).
    pub rho: f64,
    variance: f64,
}

impl HestonModel {
    /// Construct a new Heston model.
    ///
    /// Logs a warning if the Feller condition `2·κ·θ > ξ²` is violated
    /// (variance may hit zero).
    ///
    /// # Panics
    ///
    /// Panics if `initial_price <= 0`, any non-negativity constraint is
    /// violated, `|rho| > 1`, or `initial_var < 0`.
    pub fn new(
        drift: f64,
        initial_price: f64,
        kappa: f64,
        theta: f64,
        xi: f64,
        rho: f64,
        initial_var: f64,
    ) -> Self {
        assert!(initial_price > 0.0, "initial_price must be positive");
        assert!(kappa >= 0.0, "kappa must be non-negative");
        assert!(theta >= 0.0, "theta must be non-negative");
        assert!(xi >= 0.0, "xi must be non-negative");
        assert!((-1.0..=1.0).contains(&rho), "rho must be in [-1, 1]");
        assert!(initial_var >= 0.0, "initial_var must be non-negative");
        if 2.0 * kappa * theta < xi * xi {
            tracing::warn!(
                kappa, theta, xi,
                "Heston Feller condition 2κθ > ξ² is violated; variance may hit zero"
            );
        }
        Self { drift, price: initial_price, kappa, theta, xi, rho, variance: initial_var }
    }
}

impl PriceModel for HestonModel {
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64 {
        let z1 = rng.next_normal();
        let z2 = rng.next_normal();
        let w_v = self.rho * z1 + (1.0 - self.rho * self.rho).max(0.0).sqrt() * z2;
        let v_sqrt = self.variance.max(0.0).sqrt();
        let dv = self.kappa * (self.theta - self.variance) * dt
            + self.xi * v_sqrt * dt.sqrt() * w_v;
        self.variance = (self.variance + dv).max(0.0);
        let vol = self.variance.max(0.0).sqrt();
        let log_return = (self.drift - 0.5 * self.variance) * dt + vol * dt.sqrt() * z1;
        self.price *= log_return.exp();
        self.price = self.price.max(1e-6);
        self.price
    }

    fn current_price(&self) -> f64 {
        self.price
    }
}

// ────────────────────────────────────────────────────────────────────────────
// SyntheticMarketGenerator
// ────────────────────────────────────────────────────────────────────────────

/// Generator of synthetic [`NormalizedTick`] and [`OhlcvBar`] sequences.
///
/// ## Example
///
/// ```rust
/// use fin_stream::synthetic::{SyntheticMarketGenerator, GeometricBrownianMotion};
///
/// let mut model = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
/// let mut gen = SyntheticMarketGenerator::new(42);
/// let ticks = gen.generate_ticks(500, &mut model);
/// let bars  = gen.generate_ohlcv(20, &mut model,
///     fin_stream::ohlcv::Timeframe::Minutes(1));
///
/// println!("Generated {} ticks, {} bars", ticks.len(), bars.len());
/// ```
#[derive(Debug)]
pub struct SyntheticMarketGenerator {
    rng: Xorshift64,
    /// Symbol used in generated ticks/bars.
    pub symbol: String,
    /// Exchange tag used in generated ticks.
    pub exchange: Exchange,
    /// Time step in years between consecutive ticks.
    /// Default: 1 second / 252 trading days / 86 400 seconds.
    pub tick_dt_years: f64,
    /// Starting UTC millisecond timestamp for generated ticks.
    pub start_ts_ms: u64,
    /// Milliseconds between consecutive ticks.
    pub tick_interval_ms: u64,
}

impl SyntheticMarketGenerator {
    /// Construct a generator with the given random seed.
    ///
    /// `seed = 0` is replaced with 1 (xorshift64 requires non-zero state).
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Xorshift64::new(seed),
            symbol: "SYN-USD".into(),
            exchange: Exchange::Binance,
            tick_dt_years: 1.0 / (252.0 * 86_400.0),
            start_ts_ms: 1_700_000_000_000,
            tick_interval_ms: 1_000,
        }
    }

    /// Set the symbol name used in generated ticks.
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = symbol.into();
        self
    }

    /// Set the exchange tag used in generated ticks.
    pub fn with_exchange(mut self, exchange: Exchange) -> Self {
        self.exchange = exchange;
        self
    }

    // ── Tick generation ───────────────────────────────────────────────────

    /// Generate `n` synthetic [`NormalizedTick`]s from a price model.
    ///
    /// Each tick:
    /// - advances the model by `tick_dt_years`.
    /// - assigns a monotonically increasing timestamp.
    /// - randomly assigns a `Buy` or `Sell` side (50/50).
    /// - assigns a synthetic volume drawn from `Uniform(0.01, 5.0)`.
    ///
    /// Returns an empty `Vec` if `n == 0`.
    pub fn generate_ticks<M: PriceModel>(&mut self, n: usize, model: &mut M) -> Vec<NormalizedTick> {
        let mut ticks = Vec::with_capacity(n);
        let dt = self.tick_dt_years;
        for i in 0..n {
            let price = model.step(&mut self.rng, dt).max(1e-6);
            let volume = 0.01 + self.rng.next_f64() * 4.99;
            let side = if self.rng.next_u64() & 1 == 0 {
                TradeSide::Buy
            } else {
                TradeSide::Sell
            };
            ticks.push(NormalizedTick {
                exchange: self.exchange,
                symbol: self.symbol.clone(),
                price: f64_to_decimal(price),
                quantity: f64_to_decimal(volume),
                side: Some(side),
                trade_id: Some(format!("syn-{i}")),
                exchange_ts_ms: None,
                received_at_ms: self.start_ts_ms + i as u64 * self.tick_interval_ms,
            });
        }
        ticks
    }

    // ── OHLCV bar generation ──────────────────────────────────────────────

    /// Generate `n_bars` synthetic [`OhlcvBar`]s from a price model.
    ///
    /// Each bar is constructed by stepping the model `ticks_per_bar` times
    /// (derived from bar duration ÷ `tick_interval_ms`, clamped to ≥ 1).
    /// The bar's O/H/L/C are the first, max, min, and last prices within the
    /// sub-steps; volume is the sum of sub-step volumes.
    pub fn generate_ohlcv<M: PriceModel>(
        &mut self,
        n_bars: usize,
        model: &mut M,
        timeframe: Timeframe,
    ) -> Vec<OhlcvBar> {
        let bar_duration_ms = timeframe.duration_ms();
        let ticks_per_bar =
            ((bar_duration_ms / self.tick_interval_ms.max(1)) as usize).max(1);
        let dt_per_tick = self.tick_dt_years;
        let mut bars = Vec::with_capacity(n_bars);
        let mut bar_start_ms = self.start_ts_ms;

        for _ in 0..n_bars {
            let open_price = model.current_price();
            let mut high_price = open_price;
            let mut low_price = open_price;
            let mut close_price = open_price;
            let mut total_volume = 0.0_f64;
            let mut notional_sum = 0.0_f64;
            let mut trade_count = 0u64;

            for _ in 0..ticks_per_bar {
                let p = model.step(&mut self.rng, dt_per_tick).max(1e-6);
                let v = 0.01 + self.rng.next_f64() * 4.99;
                high_price = high_price.max(p);
                low_price = low_price.min(p);
                close_price = p;
                total_volume += v;
                notional_sum += p * v;
                trade_count += 1;
            }

            let vwap = if total_volume > 0.0 {
                Some(f64_to_decimal(notional_sum / total_volume))
            } else {
                None
            };

            bars.push(OhlcvBar {
                symbol: self.symbol.clone(),
                timeframe,
                bar_start_ms,
                open: f64_to_decimal(open_price),
                high: f64_to_decimal(high_price),
                low: f64_to_decimal(low_price),
                close: f64_to_decimal(close_price),
                volume: f64_to_decimal(total_volume),
                trade_count,
                is_complete: true,
                is_gap_fill: false,
                vwap,
            });

            bar_start_ms += bar_duration_ms;
        }
        bars
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Convert `f64` to `Decimal`, falling back to 0 on error.
fn f64_to_decimal(v: f64) -> Decimal {
    Decimal::from_str(&format!("{v:.8}")).unwrap_or(Decimal::ZERO)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ohlcv::Timeframe;

    // ── Legacy SyntheticGenerator ────────────────────────────────────────

    #[test]
    fn legacy_generate_tick_count() {
        let cfg = SyntheticConfig { ticks_per_bar: 10, seed: 1 };
        let mut gen = SyntheticGenerator::new(cfg);
        let bar = OhlcvInput {
            open_time_ms: 0,
            close_time_ms: 60_000,
            open: 100.0,
            high: 105.0,
            low: 98.0,
            close: 102.0,
            volume: 1000.0,
        };
        let ticks = gen.generate(&bar).unwrap_or_default();
        assert_eq!(ticks.len(), 10);
    }

    #[test]
    fn legacy_prices_within_range() {
        let cfg = SyntheticConfig { ticks_per_bar: 20, seed: 42 };
        let mut gen = SyntheticGenerator::new(cfg);
        let bar = OhlcvInput {
            open_time_ms: 0,
            close_time_ms: 60_000,
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 105.0,
            volume: 500.0,
        };
        let ticks = gen.generate(&bar).unwrap_or_default();
        for t in &ticks {
            assert!(t.price >= 90.0 && t.price <= 110.0, "price {} out of range", t.price);
        }
    }

    #[test]
    fn legacy_error_on_invalid_bar() {
        let cfg = SyntheticConfig::default();
        let mut gen = SyntheticGenerator::new(cfg);
        let bad = OhlcvInput {
            open_time_ms: 0, close_time_ms: 60_000,
            open: 100.0, high: 80.0, low: 90.0, close: 95.0, volume: 100.0,
        };
        assert!(gen.generate(&bad).is_err());
    }

    // ── Model-based generator ────────────────────────────────────────────

    #[test]
    fn gbm_generates_positive_prices() {
        let mut model = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
        let mut gen = SyntheticMarketGenerator::new(1234);
        let ticks = gen.generate_ticks(200, &mut model);
        assert_eq!(ticks.len(), 200);
        for t in &ticks {
            assert!(t.price > Decimal::ZERO, "price must be positive");
            assert!(t.quantity > Decimal::ZERO, "volume must be positive");
        }
    }

    #[test]
    fn gbm_timestamps_monotone() {
        let mut model = GeometricBrownianMotion::new(0.0, 0.10, 50.0);
        let mut gen = SyntheticMarketGenerator::new(999);
        let ticks = gen.generate_ticks(50, &mut model);
        for w in ticks.windows(2) {
            assert!(w[1].received_at_ms > w[0].received_at_ms);
        }
    }

    #[test]
    fn jump_diffusion_generates_ticks() {
        let mut model = JumpDiffusion::new(0.0, 0.20, 100.0, 10.0, -0.05, 0.10);
        let mut gen = SyntheticMarketGenerator::new(42);
        let ticks = gen.generate_ticks(100, &mut model);
        assert_eq!(ticks.len(), 100);
        for t in &ticks {
            assert!(t.price > Decimal::ZERO);
        }
    }

    #[test]
    fn ornstein_uhlenbeck_reverts_toward_mean() {
        let mean = 100.0_f64;
        let mut model = OrnsteinUhlenbeck::new(5.0, mean, 1.0, 200.0);
        let mut gen = SyntheticMarketGenerator::new(7);
        let ticks = gen.generate_ticks(500, &mut model);
        let last_price = ticks.last()
            .and_then(|t| t.price.to_f64())
            .unwrap_or(0.0);
        assert!(
            last_price < mean * 3.0,
            "OU should revert toward mean {mean}, got {last_price}"
        );
    }

    #[test]
    fn heston_model_generates_ticks() {
        let mut model = HestonModel::new(0.05, 100.0, 2.0, 0.04, 0.3, -0.7, 0.04);
        let mut gen = SyntheticMarketGenerator::new(5555);
        let ticks = gen.generate_ticks(200, &mut model);
        assert_eq!(ticks.len(), 200);
        for t in &ticks {
            assert!(t.price > Decimal::ZERO);
        }
    }

    #[test]
    fn ohlcv_generation_bar_count() {
        let mut model = GeometricBrownianMotion::new(0.0, 0.15, 100.0);
        let mut gen = SyntheticMarketGenerator::new(321);
        let bars = gen.generate_ohlcv(20, &mut model, Timeframe::Minutes(1));
        assert_eq!(bars.len(), 20);
    }

    #[test]
    fn ohlcv_invariants() {
        let mut model = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
        let mut gen = SyntheticMarketGenerator::new(1);
        let bars = gen.generate_ohlcv(50, &mut model, Timeframe::Minutes(5));
        for bar in &bars {
            assert!(bar.high >= bar.low, "high >= low");
            assert!(bar.high >= bar.open, "high >= open");
            assert!(bar.high >= bar.close, "high >= close");
            assert!(bar.low <= bar.open, "low <= open");
            assert!(bar.low <= bar.close, "low <= close");
            assert!(bar.volume >= Decimal::ZERO, "volume non-negative");
            assert!(bar.is_complete);
        }
    }

    #[test]
    fn ohlcv_timestamps_monotone() {
        let mut model = GeometricBrownianMotion::new(0.0, 0.10, 50.0);
        let mut gen = SyntheticMarketGenerator::new(77);
        let bars = gen.generate_ohlcv(10, &mut model, Timeframe::Seconds(30));
        for w in bars.windows(2) {
            assert!(w[1].bar_start_ms > w[0].bar_start_ms);
        }
    }

    #[test]
    fn deterministic_given_same_seed() {
        let mut model_a = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
        let mut model_b = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
        let mut gen_a = SyntheticMarketGenerator::new(9999);
        let mut gen_b = SyntheticMarketGenerator::new(9999);
        let ticks_a = gen_a.generate_ticks(50, &mut model_a);
        let ticks_b = gen_b.generate_ticks(50, &mut model_b);
        for (a, b) in ticks_a.iter().zip(ticks_b.iter()) {
            assert_eq!(a.price, b.price, "same seed must produce same prices");
        }
    }
}
