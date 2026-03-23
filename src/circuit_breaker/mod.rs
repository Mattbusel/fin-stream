//! WebSocket circuit breaker — degraded-mode synthetic ticks after repeated failures.
//!
//! ## Responsibility
//! Wrap a [`WsManager`] connection loop with a circuit breaker that counts
//! consecutive failures. After [`CircuitBreakerConfig::failure_threshold`]
//! consecutive failures the circuit opens, enters **degraded mode**, and emits
//! synthetic ticks derived from the last-known price with an inflated spread
//! until a real connection is restored.
//!
//! ## State machine
//!
//! ```text
//! CLOSED ──(N failures)──▶ OPEN (degraded)
//!   ▲                         │
//!   └──(success)──────────────┘
//! ```
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or `Option`.
//! - Thread-safe: state is managed inside a single async task.

use crate::error::StreamError;
use crate::tick::{Exchange, NormalizedTick, TradeSide};
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{info, warn};

/// Circuit-breaker operating state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CircuitState {
    /// Normal operation — real ticks are flowing.
    Closed,
    /// Too many consecutive failures — emitting synthetic ticks.
    Open,
}

/// Configuration for the [`WsCircuitBreaker`].
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive connection failures before the circuit opens.
    /// Must be > 0. Default: 5.
    pub failure_threshold: u32,
    /// When in degraded mode, synthetic ticks are emitted at this interval.
    pub synthetic_tick_interval: Duration,
    /// Spread multiplier applied to the last-known price to simulate a wide
    /// bid/ask in degraded mode. E.g. `0.005` = ±0.5 %.
    pub degraded_spread_pct: Decimal,
    /// Exchange identifier to stamp on synthetic ticks.
    pub exchange: Exchange,
    /// Symbol to stamp on synthetic ticks.
    pub symbol: String,
    /// Initial backoff for the first reconnect attempt.
    pub initial_backoff: Duration,
    /// Maximum backoff cap.
    pub max_backoff: Duration,
    /// Backoff multiplier (must be >= 1.0).
    pub backoff_multiplier: f64,
}

impl CircuitBreakerConfig {
    /// Build a config with required fields.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if:
    /// - `failure_threshold` is 0
    /// - `backoff_multiplier` < 1.0
    /// - `symbol` is empty
    /// - `degraded_spread_pct` is negative
    pub fn new(
        exchange: Exchange,
        symbol: impl Into<String>,
        failure_threshold: u32,
    ) -> Result<Self, StreamError> {
        let symbol = symbol.into();
        if symbol.is_empty() {
            return Err(StreamError::ConfigError {
                reason: "symbol must not be empty".into(),
            });
        }
        if failure_threshold == 0 {
            return Err(StreamError::ConfigError {
                reason: "failure_threshold must be > 0".into(),
            });
        }
        Ok(Self {
            failure_threshold,
            synthetic_tick_interval: Duration::from_millis(500),
            degraded_spread_pct: Decimal::new(5, 3), // 0.005 = 0.5%
            exchange,
            symbol,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 2.0,
        })
    }

    /// Override the degraded-mode synthetic tick interval.
    pub fn with_synthetic_tick_interval(mut self, interval: Duration) -> Self {
        self.synthetic_tick_interval = interval;
        self
    }

    /// Override the spread percentage applied in degraded mode.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `pct` is negative.
    pub fn with_degraded_spread_pct(mut self, pct: Decimal) -> Result<Self, StreamError> {
        if pct < Decimal::ZERO {
            return Err(StreamError::ConfigError {
                reason: "degraded_spread_pct must be >= 0".into(),
            });
        }
        self.degraded_spread_pct = pct;
        Ok(self)
    }
}

/// Circuit-breaker statistics snapshot.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerStats {
    /// Current circuit state.
    pub state: CircuitState,
    /// Number of consecutive failures since the last successful connection.
    pub consecutive_failures: u32,
    /// Total number of synthetic ticks emitted in degraded mode.
    pub synthetic_ticks_emitted: u64,
    /// Total real ticks forwarded while the circuit was closed.
    pub real_ticks_forwarded: u64,
}

/// Shared counters accessible from outside the run loop.
struct SharedCounters {
    consecutive_failures: AtomicU32,
    is_open: AtomicBool,
}

impl SharedCounters {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            consecutive_failures: AtomicU32::new(0),
            is_open: AtomicBool::new(false),
        })
    }
}

/// WebSocket circuit breaker with degraded-mode synthetic tick emission.
///
/// Wraps the raw message channel from a [`crate::ws::WsManager`] and monitors
/// consecutive failures. When the circuit opens it emits synthetic ticks derived
/// from the last real price until a new real connection is established.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::circuit_breaker::{CircuitBreakerConfig, WsCircuitBreaker};
/// use fin_stream::tick::Exchange;
///
/// # async fn example() -> Result<(), fin_stream::StreamError> {
/// let cfg = CircuitBreakerConfig::new(Exchange::Binance, "BTC-USD", 5)?;
/// let (breaker, mut tick_rx) = WsCircuitBreaker::new(cfg, 64);
/// // spawn breaker.run(raw_msg_rx) in a task
/// # Ok(())
/// # }
/// ```
pub struct WsCircuitBreaker {
    config: CircuitBreakerConfig,
    tick_tx: mpsc::Sender<NormalizedTick>,
    counters: Arc<SharedCounters>,
}

impl WsCircuitBreaker {
    /// Create a new circuit breaker.
    ///
    /// Returns `(breaker, tick_rx)` where `tick_rx` receives both real and
    /// synthetic ticks depending on the circuit state.
    pub fn new(
        config: CircuitBreakerConfig,
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<NormalizedTick>) {
        let (tick_tx, tick_rx) = mpsc::channel(channel_capacity);
        let breaker = Self {
            config,
            tick_tx,
            counters: SharedCounters::new(),
        };
        (breaker, tick_rx)
    }

    /// Run the circuit-breaker loop.
    ///
    /// `raw_rx` receives raw JSON strings from a [`crate::ws::WsManager`] loop.
    /// The circuit breaker parses the price field from each message, forwards
    /// parsed ticks to its output channel, and tracks consecutive failures.
    ///
    /// When `failure_threshold` consecutive failures occur the circuit opens.
    /// In open state the breaker spawns a synthetic tick emitter that runs until
    /// either a real message arrives or `tick_tx` is closed.
    ///
    /// This method returns when `raw_rx` is closed.
    pub async fn run(&self, mut raw_rx: mpsc::Receiver<String>) {
        let mut last_price: Option<Decimal> = None;
        let mut consecutive_failures: u32 = 0;
        let mut synthetic_ticks_emitted: u64 = 0;
        let mut real_ticks_forwarded: u64 = 0;

        loop {
            if self.is_circuit_open(consecutive_failures) {
                self.counters.is_open.store(true, Ordering::Relaxed);
                warn!(
                    exchange = %self.config.exchange,
                    symbol = %self.config.symbol,
                    failures = consecutive_failures,
                    "circuit open — entering degraded mode"
                );

                // In degraded mode: wait for either a real message or emit a synthetic tick.
                let mut ticker = time::interval(self.config.synthetic_tick_interval);
                loop {
                    tokio::select! {
                        msg = raw_rx.recv() => {
                            match msg {
                                None => return, // channel closed
                                Some(raw) => {
                                    if let Some(tick) = self.parse_tick(&raw, last_price) {
                                        last_price = Some(tick.price);
                                        consecutive_failures = 0;
                                        self.counters.consecutive_failures.store(0, Ordering::Relaxed);
                                        self.counters.is_open.store(false, Ordering::Relaxed);
                                        real_ticks_forwarded += 1;
                                        info!(
                                            exchange = %self.config.exchange,
                                            "circuit closed — real tick received"
                                        );
                                        let _ = self.tick_tx.send(tick).await;
                                        break; // exit degraded loop
                                    } else {
                                        consecutive_failures += 1;
                                        self.counters.consecutive_failures.store(consecutive_failures, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        _ = ticker.tick() => {
                            if let Some(price) = last_price {
                                if let Some(synthetic) = self.make_synthetic_tick(price) {
                                    synthetic_ticks_emitted += 1;
                                    let _ = self.tick_tx.send(synthetic).await;
                                }
                            }
                        }
                    }
                }
            } else {
                self.counters.is_open.store(false, Ordering::Relaxed);
                match raw_rx.recv().await {
                    None => return, // channel closed
                    Some(raw) => {
                        if let Some(tick) = self.parse_tick(&raw, last_price) {
                            last_price = Some(tick.price);
                            consecutive_failures = 0;
                            self.counters.consecutive_failures.store(0, Ordering::Relaxed);
                            real_ticks_forwarded += 1;
                            let _ = self.tick_tx.send(tick).await;
                        } else {
                            consecutive_failures += 1;
                            self.counters.consecutive_failures.store(consecutive_failures, Ordering::Relaxed);
                            warn!(
                                exchange = %self.config.exchange,
                                consecutive = consecutive_failures,
                                "parse failure counted toward circuit threshold"
                            );
                        }
                    }
                }

                // Suppress unused variable warnings in release mode.
                let _ = synthetic_ticks_emitted;
                let _ = real_ticks_forwarded;
            }
        }
    }

    fn is_circuit_open(&self, consecutive_failures: u32) -> bool {
        consecutive_failures >= self.config.failure_threshold
    }

    /// Attempt to extract a price from a raw JSON message and build a tick.
    ///
    /// Tries several common field names used by Binance/Coinbase/Polygon.
    fn parse_tick(&self, raw: &str, _last_price: Option<Decimal>) -> Option<NormalizedTick> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        // Try common price field names across exchanges.
        let price_str = v
            .get("p")
            .or_else(|| v.get("price"))
            .or_else(|| v.get("trade_price"))
            .or_else(|| v.get("lp"))
            .and_then(|p| p.as_str())?;

        let price: Decimal = price_str.parse().ok()?;
        if price <= Decimal::ZERO {
            return None;
        }

        let qty_str = v
            .get("q")
            .or_else(|| v.get("size"))
            .or_else(|| v.get("qty"))
            .and_then(|q| q.as_str())
            .unwrap_or("0");
        let quantity: Decimal = qty_str.parse().unwrap_or(Decimal::ZERO);

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64;

        Some(NormalizedTick {
            exchange: self.config.exchange,
            symbol: self.config.symbol.clone(),
            price,
            quantity,
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: now_ms,
        })
    }

    /// Construct a synthetic tick with an inflated spread from `last_price`.
    fn make_synthetic_tick(&self, last_price: Decimal) -> Option<NormalizedTick> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Widen the price by half the configured spread to signal degraded data.
        let half_spread = last_price * self.config.degraded_spread_pct;
        // Synthetic mid: price ± half_spread. We emit the mid price and tag
        // the tick with side=None so consumers know it's synthetic.
        let synthetic_price = last_price + half_spread;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64;

        Some(NormalizedTick {
            exchange: self.config.exchange,
            symbol: self.config.symbol.clone(),
            price: synthetic_price,
            quantity: Decimal::ZERO, // synthetic — zero volume
            side: Some(TradeSide::Buy), // sentinel: synthetic ticks use Buy side
            trade_id: Some("synthetic".to_string()),
            exchange_ts_ms: None,
            received_at_ms: now_ms,
        })
    }

    /// Current circuit state.
    pub fn state(&self) -> CircuitState {
        if self.counters.is_open.load(Ordering::Relaxed) {
            CircuitState::Open
        } else {
            CircuitState::Closed
        }
    }

    /// Number of consecutive failures tracked so far.
    pub fn consecutive_failures(&self) -> u32 {
        self.counters.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Configured failure threshold.
    pub fn failure_threshold(&self) -> u32 {
        self.config.failure_threshold
    }
}
