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
//! interchangeably with [`generate_ticks`] and [`generate_ohlcv`].
//!
//! ## Randomness
//!
//! The generator uses a lightweight xorshift64 PRNG seeded at construction
//! time.  No `unsafe` code is required.  The PRNG is not cryptographically
//! secure — it is used only for statistical simulation.

use crate::ohlcv::{OhlcvBar, Timeframe};
use crate::tick::{Exchange, NormalizedTick, TradeSide};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::str::FromStr;

// ────────────────────────────────────────────────────────────────────────────
// PRNG
// ────────────────────────────────────────────────────────────────────────────

/// Xorshift64 pseudo-random number generator.
///
/// Produces a uniform stream of `u64` values from a non-zero seed.
/// The state must never be zero.
#[derive(Debug, Clone)]
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// Create from a seed.  A seed of 0 is replaced with 1.
    fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 1 } else { seed } }
    }

    /// Return the next raw `u64`.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Return a uniform float in (0, 1).
    fn next_f64(&mut self) -> f64 {
        // Use 53 bits of mantissa for a `[0, 1)` value.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Approximate standard-normal sample via Box–Muller transform.
    fn next_normal(&mut self) -> f64 {
        // Box–Muller: requires two independent uniform samples.
        let u1 = self.next_f64().max(1e-15); // avoid log(0)
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Poisson sample with rate λ (for small λ, Knuth's algorithm).
    fn next_poisson(&mut self, lambda: f64) -> u32 {
        if lambda <= 0.0 {
            return 0;
        }
        // For λ > 30, fall back to Normal approximation to avoid underflow.
        if lambda > 30.0 {
            let n = (lambda + self.next_normal() * lambda.sqrt()).round();
            return n.max(0.0) as u32;
        }
        let l = (-lambda).exp();
        let mut k = 0u32;
        let mut p = 1.0_f64;
        loop {
            p *= self.next_f64();
            if p <= l {
                break;
            }
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
/// Implementors are driven by a shared PRNG and advance their internal state
/// one step at a time.
pub trait PriceModel: Send + Sync {
    /// Advance the model by one time step `dt` and return the new price.
    ///
    /// `dt` is expressed in years (e.g. 1 minute = 1.0 / (252.0 * 390.0)).
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64;

    /// Current price of the model (before the next step).
    fn current_price(&self) -> f64;
}

// ────────────────────────────────────────────────────────────────────────────
// Geometric Brownian Motion
// ────────────────────────────────────────────────────────────────────────────

/// Geometric Brownian Motion: `dS = μ S dt + σ S dW`.
///
/// The standard model for equity prices under risk-neutral measure (Black–Scholes).
///
/// # Parameters
///
/// * `drift` (`μ`) — annualised drift (e.g. 0.05 for 5% annual return).
/// * `volatility` (`σ`) — annualised volatility (e.g. 0.20 for 20% vol).
/// * `initial_price` — starting price.
#[derive(Debug, Clone)]
pub struct GeometricBrownianMotion {
    /// Annualised drift (μ).
    pub drift: f64,
    /// Annualised volatility (σ).
    pub volatility: f64,
    /// Current price.
    price: f64,
}

impl GeometricBrownianMotion {
    /// Construct a new GBM model.
    ///
    /// `initial_price` must be positive.
    pub fn new(drift: f64, volatility: f64, initial_price: f64) -> Self {
        assert!(initial_price > 0.0, "initial_price must be positive");
        assert!(volatility >= 0.0, "volatility must be non-negative");
        Self { drift, volatility, price: initial_price }
    }
}

impl PriceModel for GeometricBrownianMotion {
    fn step(&mut self, rng: &mut Xorshift64, dt: f64) -> f64 {
        let z = rng.next_normal();
        // Exact solution: S_{t+dt} = S_t * exp((μ - σ²/2) dt + σ √dt Z)
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
/// where `N` is a Poisson process with rate `λ` and each jump size
/// `J ~ LogNormal(jump_mean, jump_std)` (so `k̄ = e^{μ_J + σ_J²/2} − 1`).
///
/// # Parameters
///
/// * `gbm` — underlying GBM component.
/// * `jump_rate` (`λ`) — expected number of jumps per year (e.g. 5.0).
/// * `jump_mean` — mean of the jump log-size (e.g. −0.05 for mean-5% drops).
/// * `jump_std` — std of the jump log-size (e.g. 0.10).
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
        // GBM component.
        self.gbm.step(rng, dt);

        // Jump component: draw number of jumps in interval dt.
        let n_jumps = rng.next_poisson(self.jump_rate * dt);
        for _ in 0..n_jumps {
            let log_j = self.jump_mean + self.jump_std * rng.next_normal();
            self.gbm.price *= log_j.exp();
        }

        // Ensure price stays positive.
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
/// Useful for modelling spread, basis, and short-rate processes.  The OU
/// process is not guaranteed to stay positive — use `initial_price > 0` and
/// moderate parameters for realistic price-level simulation.
///
/// # Parameters
///
/// * `mean_reversion_speed` (`θ`) — speed of pull toward `long_term_mean`
///   (e.g. 2.0 for rapid reversion, 0.1 for slow).
/// * `long_term_mean` (`μ`) — equilibrium price level.
/// * `volatility` (`σ`) — diffusion coefficient.
/// * `initial_price` — starting price (before the first step).
#[derive(Debug, Clone)]
pub struct OrnsteinUhlenbeck {
    /// Mean-reversion speed (θ).
    pub mean_reversion_speed: f64,
    /// Long-term mean price (μ).
    pub long_term_mean: f64,
    /// Volatility (σ).
    pub volatility: f64,
    /// Current price level.
    price: f64,
}

impl OrnsteinUhlenbeck {
    /// Construct a new OU process.
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
        // Exact OU step (Euler–Maruyama approximation for simplicity).
        let z = rng.next_normal();
        let drift = self.mean_reversion_speed * (self.long_term_mean - self.price);
        self.price += drift * dt + self.volatility * dt.sqrt() * z;
        // Clamp to a small positive number to avoid negative prices in simulation.
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
/// dS   =  μ S dt + √v S dW_S
/// dv   =  κ(θ − v) dt + ξ √v dW_v
/// corr(dW_S, dW_v)  =  ρ dt
/// ```
///
/// Parameters:
///
/// * `drift` (`μ`) — annualised price drift.
/// * `initial_price` — starting price.
/// * `kappa` (`κ`) — mean-reversion speed of variance.
/// * `theta` (`θ`) — long-run variance level.
/// * `xi` (`ξ`) — volatility of variance ("vol of vol").
/// * `rho` (`ρ`) — correlation between price and variance Brownians (typically −0.7).
/// * `initial_var` — starting variance level.
#[derive(Debug, Clone)]
pub struct HestonModel {
    /// Annualised price drift (μ).
    pub drift: f64,
    /// Current price.
    price: f64,
    /// Mean-reversion speed of variance (κ).
    pub kappa: f64,
    /// Long-run variance (θ).
    pub theta: f64,
    /// Volatility of variance / vol-of-vol (ξ).
    pub xi: f64,
    /// Correlation between price and variance Brownians (ρ).
    pub rho: f64,
    /// Current variance level.
    variance: f64,
}

impl HestonModel {
    /// Construct a new Heston model.
    ///
    /// Enforces the Feller condition `2·κ·θ > ξ²` so that variance stays
    /// strictly positive (logs a warning if violated — does not panic).
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
                kappa,
                theta,
                xi,
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

        // Correlated Brownians: W_S = z1, W_v = ρ·z1 + √(1-ρ²)·z2.
        let w_v = self.rho * z1 + (1.0 - self.rho * self.rho).max(0.0).sqrt() * z2;

        // Variance step (Euler–Maruyama, clamped to ≥0 for realism).
        let v_sqrt = self.variance.max(0.0).sqrt();
        let dv = self.kappa * (self.theta - self.variance) * dt + self.xi * v_sqrt * dt.sqrt() * w_v;
        self.variance = (self.variance + dv).max(0.0);

        // Price step.
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
/// let model = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
/// let mut gen = SyntheticMarketGenerator::new(42);
/// let ticks = gen.generate_ticks(500, &mut model.clone());
/// let bars  = gen.generate_ohlcv(20, &mut model.clone(),
///     fin_stream::ohlcv::Timeframe::Minutes(1));
///
/// println!("Generated {} ticks, {} bars", ticks.len(), bars.len());
/// ```
#[derive(Debug)]
pub struct SyntheticMarketGenerator {
    rng: Xorshift64,
    /// Symbol used in generated ticks.
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

    /// Configure the symbol name used in generated ticks.
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = symbol.into();
        self
    }

    /// Configure the exchange tag used in generated ticks.
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
            let price = model.step(&mut self.rng, dt);
            let price = price.max(1e-6);
            let volume = 0.01 + self.rng.next_f64() * 4.99;
            let side = if self.rng.next_u64() & 1 == 0 { TradeSide::Buy } else { TradeSide::Sell };

            let tick = NormalizedTick {
                exchange: self.exchange,
                symbol: self.symbol.clone(),
                price: f64_to_decimal(price),
                quantity: f64_to_decimal(volume),
                side: Some(side),
                trade_id: Some(format!("syn-{i}")),
                exchange_ts_ms: None,
                received_at_ms: self.start_ts_ms + i as u64 * self.tick_interval_ms,
            };
            ticks.push(tick);
        }
        ticks
    }

    // ── OHLCV bar generation ──────────────────────────────────────────────

    /// Generate `n_bars` synthetic [`OhlcvBar`]s from a price model.
    ///
    /// Each bar is constructed by stepping the model `ticks_per_bar` times
    /// (where `ticks_per_bar` is derived from the bar duration and
    /// `tick_interval_ms`).  The bar's O/H/L/C are the first, max, min, and
    /// last prices within the sub-steps; volume is the sum of sub-step volumes.
    ///
    /// If `ticks_per_bar` rounds to 0, it is clamped to 1.
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

        for _bar_idx in 0..n_bars {
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

            let vwap = if total_volume > 0.0 { Some(f64_to_decimal(notional_sum / total_volume)) } else { None };

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
        // Start far above mean; over many steps the price should come back.
        let mean = 100.0_f64;
        let mut model = OrnsteinUhlenbeck::new(5.0, mean, 1.0, 200.0);
        let mut gen = SyntheticMarketGenerator::new(7);
        let ticks = gen.generate_ticks(500, &mut model);
        let last_price = ticks.last().map(|t| t.price.to_f64().unwrap_or(0.0)).unwrap_or(0.0);
        // After 500 steps at θ=5, price should be within 3× mean of equilibrium.
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
            assert!(bar.is_complete, "bars should be marked complete");
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
    fn xorshift_non_zero() {
        let mut rng = Xorshift64::new(0); // 0 should be replaced with 1
        for _ in 0..100 {
            // Should never return 0 from a non-degenerate xorshift.
            let v = rng.next_u64();
            assert_ne!(v, 0);
        }
    }

    #[test]
    fn xorshift_f64_in_unit_interval() {
        let mut rng = Xorshift64::new(12345);
        for _ in 0..1000 {
            let v = rng.next_f64();
            assert!(v >= 0.0 && v < 1.0, "f64 must be in [0,1), got {v}");
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
