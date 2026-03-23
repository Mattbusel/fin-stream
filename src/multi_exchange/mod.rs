//! Multi-exchange aggregation — NBBO-style consolidated best bid/ask.
//!
//! ## Responsibility
//! Merge N per-exchange [`NormalizedTick`] streams into a single consolidated
//! best-bid/best-ask (NBBO) view, track per-exchange latency divergence, and
//! detect arbitrage opportunities when the price spread between exchanges
//! exceeds a configurable threshold.
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or `Option`.
//! - Thread-safe: [`MultiExchangeAggregator`] is `Send + Sync` via `DashMap`.
//! - Uses `rust_decimal::Decimal` for all price arithmetic — never `f64`.

use crate::error::StreamError;
use crate::tick::{Exchange, NormalizedTick};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// The best bid and ask across all registered exchanges — analogous to the
/// US National Best Bid and Offer (NBBO).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Nbbo {
    /// Symbol this NBBO applies to.
    pub symbol: String,
    /// Best (highest) bid price across all exchanges, and the exchange quoting it.
    pub best_bid: Option<(Decimal, Exchange)>,
    /// Best (lowest) ask price across all exchanges, and the exchange quoting it.
    pub best_ask: Option<(Decimal, Exchange)>,
    /// Millisecond timestamp (Unix epoch) when this NBBO was computed.
    pub computed_at_ms: u64,
}

impl Nbbo {
    /// Spread between best ask and best bid, or `None` if either side is absent.
    pub fn spread(&self) -> Option<Decimal> {
        match (&self.best_bid, &self.best_ask) {
            (Some((bid, _)), Some((ask, _))) => Some(*ask - *bid),
            _ => None,
        }
    }

    /// Mid-price `(bid + ask) / 2`, or `None` if either side is absent.
    pub fn mid_price(&self) -> Option<Decimal> {
        match (&self.best_bid, &self.best_ask) {
            (Some((bid, _)), Some((ask, _))) => {
                Some((*bid + *ask) / Decimal::from(2))
            }
            _ => None,
        }
    }

    /// Returns `true` if both bid and ask are present.
    pub fn is_complete(&self) -> bool {
        self.best_bid.is_some() && self.best_ask.is_some()
    }
}

/// An arbitrage opportunity detected between two exchanges.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrageOpportunity {
    /// Symbol where the opportunity was detected.
    pub symbol: String,
    /// Exchange offering the lower (buy) price.
    pub buy_exchange: Exchange,
    /// Exchange offering the higher (sell) price.
    pub sell_exchange: Exchange,
    /// Price to buy at.
    pub buy_price: Decimal,
    /// Price to sell at.
    pub sell_price: Decimal,
    /// Absolute price difference (`sell_price - buy_price`).
    pub spread: Decimal,
    /// Millisecond timestamp when the opportunity was detected.
    pub detected_at_ms: u64,
}

impl ArbitrageOpportunity {
    /// Percentage profit: `spread / buy_price * 100`.
    pub fn profit_pct(&self) -> Decimal {
        if self.buy_price.is_zero() {
            return Decimal::ZERO;
        }
        self.spread / self.buy_price * Decimal::from(100)
    }
}

/// Per-exchange state stored inside the aggregator.
#[derive(Debug, Clone)]
struct ExchangeState {
    /// Most recent tick from this exchange.
    last_tick: Option<NormalizedTick>,
    /// Millisecond timestamp of the most recently received tick.
    last_received_ms: u64,
    /// Running total of ticks received from this exchange.
    tick_count: u64,
}

impl ExchangeState {
    fn new() -> Self {
        Self {
            last_tick: None,
            last_received_ms: 0,
            tick_count: 0,
        }
    }
}

/// Latency statistics for a single exchange.
#[derive(Debug, Clone, Copy)]
pub struct ExchangeLatencyStats {
    /// Exchange this stat belongs to.
    pub exchange: Exchange,
    /// Millisecond timestamp of the most recent tick from this exchange.
    pub last_received_ms: u64,
    /// Total ticks received.
    pub tick_count: u64,
    /// Age of the most recent tick relative to `now_ms`.
    pub age_ms: u64,
}

/// Configuration for [`MultiExchangeAggregator`].
#[derive(Debug, Clone)]
pub struct AggregatorConfig {
    /// Minimum absolute price difference (in the same currency unit as the
    /// tick price) to classify as an arbitrage opportunity.
    pub arb_threshold: Decimal,
    /// Symbol to aggregate (e.g. `"BTC-USD"`).
    pub symbol: String,
}

impl AggregatorConfig {
    /// Create a new config.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `symbol` is empty or
    /// `arb_threshold` is negative.
    pub fn new(symbol: impl Into<String>, arb_threshold: Decimal) -> Result<Self, StreamError> {
        let symbol = symbol.into();
        if symbol.is_empty() {
            return Err(StreamError::ConfigError {
                reason: "symbol must not be empty".into(),
            });
        }
        if arb_threshold < Decimal::ZERO {
            return Err(StreamError::ConfigError {
                reason: "arb_threshold must be >= 0".into(),
            });
        }
        Ok(Self {
            arb_threshold,
            symbol,
        })
    }
}

/// NBBO-style multi-exchange aggregator.
///
/// Accepts ticks from multiple exchanges via [`MultiExchangeAggregator::ingest`]
/// and emits consolidated [`Nbbo`] updates and [`ArbitrageOpportunity`] alerts.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::multi_exchange::{AggregatorConfig, MultiExchangeAggregator};
/// use rust_decimal_macros::dec;
///
/// # async fn example() -> Result<(), fin_stream::StreamError> {
/// let cfg = AggregatorConfig::new("BTC-USD", dec!(10))?;
/// let (agg, mut nbbo_rx, mut arb_rx) = MultiExchangeAggregator::new(cfg, 64);
/// # Ok(())
/// # }
/// ```
pub struct MultiExchangeAggregator {
    config: AggregatorConfig,
    state: Arc<DashMap<Exchange, ExchangeState>>,
    nbbo_tx: mpsc::Sender<Nbbo>,
    arb_tx: mpsc::Sender<ArbitrageOpportunity>,
}

impl MultiExchangeAggregator {
    /// Create a new aggregator.
    ///
    /// Returns the aggregator plus two receivers:
    /// - `nbbo_rx`: receives a new [`Nbbo`] snapshot every time a tick changes
    ///   the consolidated best bid/ask.
    /// - `arb_rx`: receives an [`ArbitrageOpportunity`] whenever the price
    ///   discrepancy between any two exchanges exceeds `config.arb_threshold`.
    pub fn new(
        config: AggregatorConfig,
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Nbbo>, mpsc::Receiver<ArbitrageOpportunity>) {
        let (nbbo_tx, nbbo_rx) = mpsc::channel(channel_capacity);
        let (arb_tx, arb_rx) = mpsc::channel(channel_capacity);
        let agg = Self {
            config,
            state: Arc::new(DashMap::new()),
            nbbo_tx,
            arb_tx,
        };
        (agg, nbbo_rx, arb_rx)
    }

    /// Ingest a tick from any exchange.
    ///
    /// Updates internal exchange state, recomputes the NBBO, emits a new
    /// [`Nbbo`] snapshot, and checks for arbitrage opportunities.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if the tick's symbol does not match
    /// the configured symbol. Channel send errors are logged as warnings and
    /// do not propagate (the consumer may have dropped the receiver).
    pub async fn ingest(&self, tick: NormalizedTick) -> Result<(), StreamError> {
        if tick.symbol != self.config.symbol {
            return Err(StreamError::ConfigError {
                reason: format!(
                    "aggregator configured for '{}', received tick for '{}'",
                    self.config.symbol, tick.symbol
                ),
            });
        }

        // Update per-exchange state.
        let received_ms = tick.received_at_ms;
        {
            let mut entry = self
                .state
                .entry(tick.exchange)
                .or_insert_with(ExchangeState::new);
            entry.tick_count += 1;
            entry.last_received_ms = received_ms;
            entry.last_tick = Some(tick.clone());
        }

        debug!(
            exchange = %tick.exchange,
            price = %tick.price,
            symbol = %tick.symbol,
            "tick ingested"
        );

        // Recompute NBBO from all current last-ticks.
        let nbbo = self.compute_nbbo(received_ms);
        if let Err(e) = self.nbbo_tx.send(nbbo.clone()).await {
            warn!("nbbo channel closed: {e}");
        }

        // Check for arbitrage.
        self.check_arbitrage(&nbbo, received_ms).await;

        Ok(())
    }

    /// Compute the current NBBO from all exchange states.
    fn compute_nbbo(&self, now_ms: u64) -> Nbbo {
        let mut best_bid: Option<(Decimal, Exchange)> = None;
        let mut best_ask: Option<(Decimal, Exchange)> = None;

        for entry in self.state.iter() {
            let exchange = *entry.key();
            if let Some(tick) = &entry.value().last_tick {
                // Treat the last known trade price as both a synthetic bid and
                // ask (spread around price). In a real NBBO you would use a
                // live order book; here we use the trade price as a proxy.
                let price = tick.price;

                match best_bid {
                    None => best_bid = Some((price, exchange)),
                    Some((current, _)) if price > current => {
                        best_bid = Some((price, exchange))
                    }
                    _ => {}
                }

                match best_ask {
                    None => best_ask = Some((price, exchange)),
                    Some((current, _)) if price < current => {
                        best_ask = Some((price, exchange))
                    }
                    _ => {}
                }
            }
        }

        Nbbo {
            symbol: self.config.symbol.clone(),
            best_bid,
            best_ask,
            computed_at_ms: now_ms,
        }
    }

    /// Check all exchange-pair combinations for arbitrage.
    async fn check_arbitrage(&self, _nbbo: &Nbbo, now_ms: u64) {
        // Collect current prices per exchange.
        let prices: Vec<(Exchange, Decimal)> = self
            .state
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .last_tick
                    .as_ref()
                    .map(|t| (*entry.key(), t.price))
            })
            .collect();

        // O(N^2) over exchanges (N is small: typically 2–5).
        for i in 0..prices.len() {
            for j in (i + 1)..prices.len() {
                let (ex_a, price_a) = prices[i];
                let (ex_b, price_b) = prices[j];

                let (buy_ex, buy_price, sell_ex, sell_price) = if price_a <= price_b {
                    (ex_a, price_a, ex_b, price_b)
                } else {
                    (ex_b, price_b, ex_a, price_a)
                };

                let spread = sell_price - buy_price;
                if spread >= self.config.arb_threshold {
                    let opp = ArbitrageOpportunity {
                        symbol: self.config.symbol.clone(),
                        buy_exchange: buy_ex,
                        sell_exchange: sell_ex,
                        buy_price,
                        sell_price,
                        spread,
                        detected_at_ms: now_ms,
                    };
                    info!(
                        symbol = %opp.symbol,
                        buy_at = %opp.buy_exchange,
                        sell_at = %opp.sell_exchange,
                        spread = %opp.spread,
                        "arbitrage opportunity detected"
                    );
                    if let Err(e) = self.arb_tx.send(opp).await {
                        warn!("arb channel closed: {e}");
                    }
                }
            }
        }
    }

    /// Snapshot of per-exchange latency statistics at `now_ms`.
    pub fn latency_stats(&self, now_ms: u64) -> Vec<ExchangeLatencyStats> {
        self.state
            .iter()
            .map(|entry| ExchangeLatencyStats {
                exchange: *entry.key(),
                last_received_ms: entry.value().last_received_ms,
                tick_count: entry.value().tick_count,
                age_ms: now_ms.saturating_sub(entry.value().last_received_ms),
            })
            .collect()
    }

    /// Maximum latency divergence (ms) between the fastest and slowest exchange.
    ///
    /// Returns `None` if fewer than two exchanges have been seen.
    pub fn max_latency_divergence_ms(&self, now_ms: u64) -> Option<u64> {
        let stats = self.latency_stats(now_ms);
        if stats.len() < 2 {
            return None;
        }
        let min_age = stats.iter().map(|s| s.age_ms).min().unwrap_or(0);
        let max_age = stats.iter().map(|s| s.age_ms).max().unwrap_or(0);
        Some(max_age.saturating_sub(min_age))
    }

    /// Number of distinct exchanges that have contributed at least one tick.
    pub fn exchange_count(&self) -> usize {
        self.state.len()
    }

    /// Most recently computed NBBO, or `None` if no ticks have been received.
    pub fn current_nbbo(&self) -> Option<Nbbo> {
        let now_ms = now_ms();
        let nbbo = self.compute_nbbo(now_ms);
        if nbbo.best_bid.is_none() && nbbo.best_ask.is_none() {
            None
        } else {
            Some(nbbo)
        }
    }

    /// Configured symbol.
    pub fn symbol(&self) -> &str {
        &self.config.symbol
    }

    /// Configured arbitrage threshold.
    pub fn arb_threshold(&self) -> Decimal {
        self.config.arb_threshold
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}
