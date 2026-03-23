//! Per-symbol circuit breakers for halting trading on price or volume extremes.
//!
//! ## Overview
//!
//! | Type | Responsibility |
//! |------|----------------|
//! | [`SymbolCircuitBreaker`] | State machine per symbol: `Normal` → `Halted` → `Recovering` |
//! | [`CircuitBreakerHub`] | Manages one breaker per symbol via [`DashMap`] |
//! | [`HaltConfig`] | Configures price-move and volume-surge thresholds and halt duration |
//!
//! ## State Machine
//!
//! ```text
//! Normal ──(price spike | volume surge)──▶ Halted { until, reason }
//!   ▲                                           │
//!   └──(halt expired, next tick)────────────────┘  (via Recovering)
//! ```

use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::tick::NormalizedTick;

// ──────────────────────────────────────────────────────────────────────────────
// HaltConfig
// ──────────────────────────────────────────────────────────────────────────────

/// Configuration for a [`SymbolCircuitBreaker`].
#[derive(Debug, Clone)]
pub struct HaltConfig {
    /// Maximum permissible absolute price move as a fraction of the reference
    /// price (e.g. `0.05` = 5 %). Moves exceeding this trigger a halt.
    pub price_move_pct: f64,
    /// Volume surge factor relative to the rolling average. A value of `3.0`
    /// means the current tick's volume must be at least 3× the average to
    /// trigger a halt.
    pub volume_surge_factor: f64,
    /// Number of ticks in the rolling window used to compute the reference
    /// price and average volume.
    pub window_ticks: usize,
    /// How long the circuit breaker remains halted after triggering.
    pub halt_duration: Duration,
}

impl HaltConfig {
    /// Construct a [`HaltConfig`] with sensible defaults.
    ///
    /// * `price_move_pct` — e.g. `0.05` for 5 %
    /// * `volume_surge_factor` — e.g. `5.0` for 5×
    /// * `window_ticks` — e.g. `20`
    /// * `halt_duration` — e.g. `Duration::from_secs(60)`
    pub fn new(
        price_move_pct: f64,
        volume_surge_factor: f64,
        window_ticks: usize,
        halt_duration: Duration,
    ) -> Self {
        Self {
            price_move_pct,
            volume_surge_factor,
            window_ticks,
            halt_duration,
        }
    }
}

impl Default for HaltConfig {
    fn default() -> Self {
        Self {
            price_move_pct: 0.05,
            volume_surge_factor: 5.0,
            window_ticks: 20,
            halt_duration: Duration::from_secs(60),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// HaltReason
// ──────────────────────────────────────────────────────────────────────────────

/// The reason a circuit breaker was tripped.
#[derive(Debug, Clone, PartialEq)]
pub enum HaltReason {
    /// The absolute price move exceeded `price_move_pct`.
    PriceSpike {
        /// Observed move as a fraction of the reference price.
        move_pct: f64,
    },
    /// Trading volume exceeded `volume_surge_factor × average_volume`.
    VolumeSurge {
        /// Observed surge factor (current / average).
        factor: f64,
    },
    /// Both a price spike and a volume surge were detected simultaneously.
    Composite,
}

// ──────────────────────────────────────────────────────────────────────────────
// BreakerState
// ──────────────────────────────────────────────────────────────────────────────

/// Operating state of a [`SymbolCircuitBreaker`].
#[derive(Debug, Clone)]
pub enum BreakerState {
    /// Normal operation — all ticks are forwarded.
    Normal,
    /// Halted: ticks are rejected until `until` has elapsed.
    Halted {
        /// When the halt expires.
        until: Instant,
        /// What caused the halt.
        reason: HaltReason,
    },
    /// Recovering: the halt has expired; the next tick transitions to `Normal`.
    Recovering,
}

// ──────────────────────────────────────────────────────────────────────────────
// CircuitDecision
// ──────────────────────────────────────────────────────────────────────────────

/// Decision returned by [`SymbolCircuitBreaker::process_tick`].
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitDecision {
    /// Allow the tick through.
    Allow,
    /// Halt: this tick triggered the circuit breaker.
    Halt(HaltReason),
    /// Recover: the halt has expired; returning to `Normal`.
    Recover,
}

// ──────────────────────────────────────────────────────────────────────────────
// SymbolCircuitBreaker
// ──────────────────────────────────────────────────────────────────────────────

/// Per-symbol circuit breaker.
///
/// Maintains a rolling window of recent prices and volumes. When a new tick's
/// price move or volume surge exceeds the configured thresholds the breaker
/// transitions to `Halted` for `halt_duration`.
pub struct SymbolCircuitBreaker {
    config: HaltConfig,
    state: BreakerState,
    /// Rolling window of recent prices (used for reference price).
    price_window: VecDeque<f64>,
    /// Rolling window of recent volumes (used for average volume computation).
    volume_window: VecDeque<f64>,
    /// Total number of halts triggered.
    halt_count: u64,
    /// Longest observed halt duration in milliseconds.
    longest_halt_ms: u64,
    /// Timestamp when the most recent halt began.
    halt_started: Option<Instant>,
}

impl SymbolCircuitBreaker {
    /// Create a new breaker with the given configuration.
    pub fn new(config: HaltConfig) -> Self {
        Self {
            config,
            state: BreakerState::Normal,
            price_window: VecDeque::new(),
            volume_window: VecDeque::new(),
            halt_count: 0,
            longest_halt_ms: 0,
            halt_started: None,
        }
    }

    /// Process an incoming tick and return the circuit decision.
    pub fn process_tick(&mut self, tick: &NormalizedTick) -> CircuitDecision {
        let price: f64 = tick.price.try_into().unwrap_or(0.0);
        let volume: f64 = tick.quantity.try_into().unwrap_or(0.0);

        // State machine transitions.
        match &self.state.clone() {
            BreakerState::Halted { until, .. } => {
                let now = Instant::now();
                if now >= *until {
                    // Record halt duration.
                    if let Some(started) = self.halt_started.take() {
                        let ms = now.duration_since(started).as_millis() as u64;
                        if ms > self.longest_halt_ms {
                            self.longest_halt_ms = ms;
                        }
                    }
                    self.state = BreakerState::Recovering;
                    return CircuitDecision::Recover;
                }
                // Still halted — update window but reject tick.
                self.push_window(price, volume);
                return CircuitDecision::Halt(
                    if let BreakerState::Halted { reason, .. } = &self.state {
                        reason.clone()
                    } else {
                        HaltReason::Composite
                    },
                );
            }
            BreakerState::Recovering => {
                self.state = BreakerState::Normal;
                // Fall through to Normal processing below.
            }
            BreakerState::Normal => {}
        }

        // Normal state: check thresholds.
        let reason_opt = self.check_thresholds(price, volume);
        self.push_window(price, volume);

        if let Some(reason) = reason_opt {
            let until = Instant::now() + self.config.halt_duration;
            self.halt_started = Some(Instant::now());
            self.state = BreakerState::Halted { until, reason: reason.clone() };
            self.halt_count += 1;
            CircuitDecision::Halt(reason)
        } else {
            CircuitDecision::Allow
        }
    }

    fn push_window(&mut self, price: f64, volume: f64) {
        let cap = self.config.window_ticks.max(1);
        if self.price_window.len() == cap {
            self.price_window.pop_front();
        }
        self.price_window.push_back(price);
        if self.volume_window.len() == cap {
            self.volume_window.pop_front();
        }
        self.volume_window.push_back(volume);
    }

    fn check_thresholds(&self, price: f64, volume: f64) -> Option<HaltReason> {
        if self.price_window.is_empty() {
            return None;
        }

        // Reference price: average of the rolling window.
        let ref_price: f64 = self.price_window.iter().sum::<f64>() / self.price_window.len() as f64;
        let price_move = if ref_price > 0.0 {
            (price - ref_price).abs() / ref_price
        } else {
            0.0
        };

        let price_triggered = price_move > self.config.price_move_pct;

        // Average volume over the window.
        let avg_vol: f64 = if self.volume_window.is_empty() {
            0.0
        } else {
            self.volume_window.iter().sum::<f64>() / self.volume_window.len() as f64
        };
        let vol_factor = if avg_vol > 0.0 { volume / avg_vol } else { 0.0 };
        let volume_triggered = vol_factor > self.config.volume_surge_factor;

        match (price_triggered, volume_triggered) {
            (true, true) => Some(HaltReason::Composite),
            (true, false) => Some(HaltReason::PriceSpike { move_pct: price_move }),
            (false, true) => Some(HaltReason::VolumeSurge { factor: vol_factor }),
            (false, false) => None,
        }
    }

    /// Current breaker state.
    pub fn state(&self) -> &BreakerState {
        &self.state
    }

    /// Whether the breaker is currently halted.
    pub fn is_halted(&self) -> bool {
        matches!(self.state, BreakerState::Halted { .. })
    }

    /// Total number of halts triggered since creation.
    pub fn halt_count(&self) -> u64 {
        self.halt_count
    }

    /// Longest observed halt duration in milliseconds.
    pub fn longest_halt_ms(&self) -> u64 {
        self.longest_halt_ms
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// CircuitBreakerHub
// ──────────────────────────────────────────────────────────────────────────────

/// Manages one [`SymbolCircuitBreaker`] per symbol.
///
/// Thread-safe: uses a [`DashMap`] internally.
pub struct CircuitBreakerHub {
    breakers: DashMap<String, SymbolCircuitBreaker>,
    config: HaltConfig,
}

impl CircuitBreakerHub {
    /// Create a new hub. All breakers created by this hub use `config`.
    pub fn new(config: HaltConfig) -> Self {
        Self {
            breakers: DashMap::new(),
            config,
        }
    }

    /// Process a tick through the appropriate symbol's circuit breaker,
    /// creating a new breaker for the symbol if one does not yet exist.
    pub fn process_tick(&self, tick: &NormalizedTick) -> CircuitDecision {
        let mut entry = self
            .breakers
            .entry(tick.symbol.clone())
            .or_insert_with(|| SymbolCircuitBreaker::new(self.config.clone()));
        entry.process_tick(tick)
    }

    /// Get a snapshot of circuit-breaker statistics across all symbols.
    pub fn stats(&self) -> CircuitStats {
        let mut total_halts = 0_u64;
        let mut current_halted_symbols = Vec::new();
        let mut longest_halt_ms = 0_u64;

        for entry in self.breakers.iter() {
            let sym = entry.key().clone();
            let breaker = entry.value();
            total_halts += breaker.halt_count();
            if breaker.is_halted() {
                current_halted_symbols.push(sym);
            }
            if breaker.longest_halt_ms() > longest_halt_ms {
                longest_halt_ms = breaker.longest_halt_ms();
            }
        }

        CircuitStats {
            total_halts,
            current_halted_symbols,
            longest_halt_ms,
        }
    }

    /// Number of symbols tracked by this hub.
    pub fn symbol_count(&self) -> usize {
        self.breakers.len()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// CircuitStats
// ──────────────────────────────────────────────────────────────────────────────

/// Aggregated statistics from all breakers in a [`CircuitBreakerHub`].
#[derive(Debug, Clone)]
pub struct CircuitStats {
    /// Total halt events across all symbols.
    pub total_halts: u64,
    /// Symbols currently in the `Halted` state.
    pub current_halted_symbols: Vec<String>,
    /// Longest single halt duration observed across all symbols (milliseconds).
    pub longest_halt_ms: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Shared hub
// ──────────────────────────────────────────────────────────────────────────────

/// Thread-safe shared [`CircuitBreakerHub`] wrapped in an `Arc`.
pub type SharedCircuitBreakerHub = Arc<CircuitBreakerHub>;

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use crate::tick::{Exchange, NormalizedTick};

    fn make_tick(symbol: &str, price: f64, qty: f64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price: Decimal::from_str(&price.to_string()).unwrap_or(Decimal::ONE),
            quantity: Decimal::from_str(&qty.to_string()).unwrap_or(Decimal::ONE),
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    fn config_tight() -> HaltConfig {
        HaltConfig {
            price_move_pct: 0.05,
            volume_surge_factor: 3.0,
            window_ticks: 5,
            halt_duration: Duration::from_millis(10), // short for tests
        }
    }

    fn seed_breaker(breaker: &mut SymbolCircuitBreaker, price: f64, qty: f64, n: usize) {
        for _ in 0..n {
            let t = make_tick("X", price, qty);
            breaker.process_tick(&t);
        }
    }

    // --- HaltConfig ---

    #[test]
    fn halt_config_default_values() {
        let cfg = HaltConfig::default();
        assert_eq!(cfg.price_move_pct, 0.05);
        assert_eq!(cfg.volume_surge_factor, 5.0);
        assert_eq!(cfg.window_ticks, 20);
    }

    // --- SymbolCircuitBreaker ---

    #[test]
    fn normal_tick_allowed() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        let t = make_tick("BTC", 100.0, 1.0);
        assert_eq!(cb.process_tick(&t), CircuitDecision::Allow);
    }

    #[test]
    fn price_spike_triggers_halt() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let spike = make_tick("BTC", 200.0, 1.0); // +100 % >> 5 %
        let decision = cb.process_tick(&spike);
        assert!(
            matches!(decision, CircuitDecision::Halt(HaltReason::PriceSpike { .. })),
            "expected PriceSpike, got {:?}",
            decision
        );
    }

    #[test]
    fn volume_surge_triggers_halt() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let surge = make_tick("BTC", 100.0, 50.0); // 50× average
        let decision = cb.process_tick(&surge);
        assert!(
            matches!(decision, CircuitDecision::Halt(HaltReason::VolumeSurge { .. })),
            "expected VolumeSurge, got {:?}",
            decision
        );
    }

    #[test]
    fn composite_halt_when_both_triggered() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let both = make_tick("BTC", 200.0, 50.0); // price spike + volume surge
        let decision = cb.process_tick(&both);
        assert!(
            matches!(decision, CircuitDecision::Halt(HaltReason::Composite)),
            "expected Composite, got {:?}",
            decision
        );
    }

    #[test]
    fn halted_state_rejects_subsequent_ticks() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let spike = make_tick("BTC", 200.0, 1.0);
        cb.process_tick(&spike); // triggers halt
        let next = make_tick("BTC", 200.0, 1.0);
        let decision = cb.process_tick(&next);
        assert!(matches!(decision, CircuitDecision::Halt(_)));
    }

    #[test]
    fn halt_count_increments() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let spike = make_tick("BTC", 200.0, 1.0);
        cb.process_tick(&spike);
        assert_eq!(cb.halt_count(), 1);
    }

    #[test]
    fn breaker_is_halted_after_spike() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let spike = make_tick("BTC", 200.0, 1.0);
        cb.process_tick(&spike);
        assert!(cb.is_halted());
    }

    #[test]
    fn recovery_after_halt_expires() {
        let cfg = HaltConfig {
            halt_duration: Duration::from_millis(1), // expires instantly
            ..config_tight()
        };
        let mut cb = SymbolCircuitBreaker::new(cfg);
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        let spike = make_tick("BTC", 200.0, 1.0);
        cb.process_tick(&spike); // halt
        std::thread::sleep(Duration::from_millis(5));
        let t = make_tick("BTC", 200.0, 1.0);
        let decision = cb.process_tick(&t);
        assert_eq!(decision, CircuitDecision::Recover);
    }

    #[test]
    fn normal_price_no_halt() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        seed_breaker(&mut cb, 100.0, 1.0, 5);
        // Price move < threshold
        let normal = make_tick("BTC", 102.0, 1.0); // +2 % < 5 %
        assert_eq!(cb.process_tick(&normal), CircuitDecision::Allow);
    }

    #[test]
    fn first_tick_never_halts() {
        let mut cb = SymbolCircuitBreaker::new(config_tight());
        // No window yet → no halt even for extreme price
        let t = make_tick("BTC", 999999.0, 9999.0);
        assert_eq!(cb.process_tick(&t), CircuitDecision::Allow);
    }

    // --- CircuitBreakerHub ---

    #[test]
    fn hub_creates_breaker_per_symbol() {
        let hub = CircuitBreakerHub::new(config_tight());
        let t1 = make_tick("BTC", 100.0, 1.0);
        let t2 = make_tick("ETH", 50.0, 2.0);
        hub.process_tick(&t1);
        hub.process_tick(&t2);
        assert_eq!(hub.symbol_count(), 2);
    }

    #[test]
    fn hub_stats_no_halts_initially() {
        let hub = CircuitBreakerHub::new(config_tight());
        let t = make_tick("BTC", 100.0, 1.0);
        hub.process_tick(&t);
        let stats = hub.stats();
        assert_eq!(stats.total_halts, 0);
        assert!(stats.current_halted_symbols.is_empty());
    }

    #[test]
    fn hub_stats_reflects_halt() {
        let hub = CircuitBreakerHub::new(config_tight());
        // Seed BTC then spike it.
        for _ in 0..5 {
            hub.process_tick(&make_tick("BTC", 100.0, 1.0));
        }
        hub.process_tick(&make_tick("BTC", 200.0, 1.0));
        let stats = hub.stats();
        assert_eq!(stats.total_halts, 1);
        assert!(stats.current_halted_symbols.contains(&"BTC".to_string()));
    }

    #[test]
    fn hub_independent_symbols_dont_interfere() {
        let hub = CircuitBreakerHub::new(config_tight());
        for _ in 0..5 {
            hub.process_tick(&make_tick("BTC", 100.0, 1.0));
            hub.process_tick(&make_tick("ETH", 50.0, 1.0));
        }
        // Spike only BTC
        hub.process_tick(&make_tick("BTC", 200.0, 1.0));
        let stats = hub.stats();
        assert!(stats.current_halted_symbols.contains(&"BTC".to_string()));
        assert!(!stats.current_halted_symbols.contains(&"ETH".to_string()));
    }
}
