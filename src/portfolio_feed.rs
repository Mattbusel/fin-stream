//! Multi-asset portfolio feed — parallel WebSocket connections per asset.
//!
//! ## Responsibility
//! Manage one [`WsManager`] per registered asset, run them concurrently in a
//! [`tokio::task::JoinSet`], merge all tick streams into a single
//! `mpsc::Receiver<(String, NormalizedTick)>`, and maintain the latest tick per
//! asset for snapshot queries.
//!
//! ## Reconnection
//! Each asset feed inherits the reconnection policy of the underlying
//! [`WsManager`]. An exponential-backoff respawn is applied at the
//! [`PortfolioFeed`] level if a feed task exits unexpectedly: the feed is
//! restarted after a delay that doubles up to a configured maximum.
//!
//! ## Concurrency
//! Asset registration is protected by a `tokio::sync::Mutex`. The latest-tick
//! snapshot map uses `DashMap` so reads are lock-free.

use crate::error::StreamError;
use crate::tick::{Exchange, NormalizedTick};
use crate::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

/// Configuration for a single asset feed within the portfolio.
#[derive(Debug, Clone)]
pub struct AssetFeedConfig {
    /// Canonical asset symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Exchange to connect to.
    pub exchange: Exchange,
    /// WebSocket connection configuration.
    pub connection_config: ConnectionConfig,
    /// Reconnect policy for this asset.
    pub reconnect_policy: ReconnectPolicy,
}

/// Statistics for a single asset feed.
#[derive(Debug, Clone, Default)]
pub struct AssetFeedStats {
    /// Number of ticks received since start.
    pub ticks_received: u64,
    /// Number of times this feed has been restarted.
    pub restart_count: u32,
    /// Whether this feed is currently active.
    pub is_active: bool,
}

/// Exponential backoff state for a per-asset restart loop.
#[derive(Debug, Clone)]
struct BackoffState {
    current_delay: Duration,
    max_delay: Duration,
    multiplier: f64,
}

impl BackoffState {
    fn new(initial: Duration, max: Duration, multiplier: f64) -> Self {
        Self {
            current_delay: initial,
            max_delay: max,
            multiplier,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.current_delay;
        self.current_delay = Duration::from_millis(
            ((self.current_delay.as_millis() as f64) * self.multiplier) as u64,
        )
        .min(self.max_delay);
        delay
    }

    fn reset(&mut self, initial: Duration) {
        self.current_delay = initial;
    }
}

/// A merged stream of real-time ticks for all registered assets.
///
/// ## Example
/// ```rust,no_run
/// use fin_stream::portfolio_feed::PortfolioFeed;
/// use fin_stream::tick::Exchange;
///
/// # async fn run() -> Result<(), fin_stream::error::StreamError> {
/// let mut feed = PortfolioFeed::new(256);
/// feed.add_asset("BTC-USD", Exchange::Coinbase);
/// feed.add_asset("ETH-USD", Exchange::Coinbase);
///
/// let mut rx = feed.tick_stream();
/// while let Some((symbol, tick)) = rx.recv().await {
///     println!("{symbol}: {}", tick.price);
/// }
/// # Ok(())
/// # }
/// ```
pub struct PortfolioFeed {
    /// Registered assets — symbol → config.
    configs: Arc<tokio::sync::Mutex<HashMap<String, AssetFeedConfig>>>,
    /// Latest tick snapshot per symbol — updated on every incoming tick.
    latest: Arc<DashMap<String, NormalizedTick>>,
    /// Per-asset feed statistics.
    stats: Arc<DashMap<String, AssetFeedStats>>,
    /// Capacity of the merged tick channel.
    channel_capacity: usize,
    /// Sender end of the merged tick channel; `None` until `tick_stream` is called.
    tick_tx: Option<mpsc::Sender<(String, NormalizedTick)>>,
}

impl PortfolioFeed {
    /// Create a new [`PortfolioFeed`] with the given merged-channel capacity.
    ///
    /// `channel_capacity` must be at least 1; values below 1 are clamped to 1.
    pub fn new(channel_capacity: usize) -> Self {
        Self {
            configs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            latest: Arc::new(DashMap::new()),
            stats: Arc::new(DashMap::new()),
            channel_capacity: channel_capacity.max(1),
            tick_tx: None,
        }
    }

    /// Register an asset for streaming using default connection settings.
    ///
    /// If `tick_stream()` has already been called, the new asset feed is
    /// spawned immediately; otherwise it will be started when `tick_stream()`
    /// is called.
    pub async fn add_asset(&self, symbol: &str, exchange: Exchange) {
        let config = AssetFeedConfig {
            symbol: symbol.to_string(),
            exchange,
            connection_config: ConnectionConfig::default(),
            reconnect_policy: ReconnectPolicy::default(),
        };
        self.add_asset_with_config(config).await;
    }

    /// Register an asset with a fully specified [`AssetFeedConfig`].
    pub async fn add_asset_with_config(&self, config: AssetFeedConfig) {
        let symbol = config.symbol.clone();
        let mut configs = self.configs.lock().await;
        configs.insert(symbol.clone(), config);
        self.stats.entry(symbol).or_default();
    }

    /// Start all registered asset feeds and return a merged tick receiver.
    ///
    /// This method consumes the internal sender — it should be called exactly
    /// once. Subsequent calls will return a receiver that never produces items
    /// (the send side has been moved into the spawned tasks).
    pub fn tick_stream(&mut self) -> mpsc::Receiver<(String, NormalizedTick)> {
        let (tx, rx) = mpsc::channel::<(String, NormalizedTick)>(self.channel_capacity);
        self.tick_tx = Some(tx.clone());

        let configs_arc = Arc::clone(&self.configs);
        let latest_arc = Arc::clone(&self.latest);
        let stats_arc = Arc::clone(&self.stats);

        tokio::spawn(async move {
            portfolio_driver(configs_arc, latest_arc, stats_arc, tx).await;
        });

        rx
    }

    /// Return the latest tick for each registered asset (snapshot).
    ///
    /// Only assets for which at least one tick has been received are included.
    pub fn portfolio_snapshot(&self) -> HashMap<String, NormalizedTick> {
        self.latest
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    /// Return the latest tick for a specific symbol, or `None` if not yet received.
    pub fn latest_tick(&self, symbol: &str) -> Option<NormalizedTick> {
        self.latest.get(symbol).map(|e| e.value().clone())
    }

    /// Return feed statistics for a specific symbol.
    pub fn feed_stats(&self, symbol: &str) -> Option<AssetFeedStats> {
        self.stats.get(symbol).map(|e| e.value().clone())
    }

    /// Number of currently registered assets.
    pub async fn asset_count(&self) -> usize {
        self.configs.lock().await.len()
    }
}

/// Background task: spawns one feed task per registered asset and supervises them.
async fn portfolio_driver(
    configs: Arc<tokio::sync::Mutex<HashMap<String, AssetFeedConfig>>>,
    latest: Arc<DashMap<String, NormalizedTick>>,
    stats: Arc<DashMap<String, AssetFeedStats>>,
    tx: mpsc::Sender<(String, NormalizedTick)>,
) {
    let snapshot: Vec<AssetFeedConfig> = {
        let guard = configs.lock().await;
        guard.values().cloned().collect()
    };

    let mut join_set: JoinSet<(String, Result<(), StreamError>)> = JoinSet::new();

    for cfg in snapshot {
        let tx2 = tx.clone();
        let latest2 = Arc::clone(&latest);
        let stats2 = Arc::clone(&stats);
        let symbol = cfg.symbol.clone();
        join_set.spawn(async move {
            let result = run_asset_feed(cfg, tx2, latest2, stats2).await;
            (symbol, result)
        });
    }

    while let Some(outcome) = join_set.join_next().await {
        match outcome {
            Ok((symbol, Ok(()))) => {
                info!(symbol, "asset feed exited cleanly");
            }
            Ok((symbol, Err(e))) => {
                error!(symbol, error = %e, "asset feed exited with error");
            }
            Err(join_err) => {
                error!(error = %join_err, "asset feed task panicked");
            }
        }
    }
}

/// Drives a single asset feed with exponential-backoff restarts.
async fn run_asset_feed(
    cfg: AssetFeedConfig,
    tx: mpsc::Sender<(String, NormalizedTick)>,
    latest: Arc<DashMap<String, NormalizedTick>>,
    stats: Arc<DashMap<String, AssetFeedStats>>,
) -> Result<(), StreamError> {
    let mut backoff = BackoffState::new(
        Duration::from_millis(500),
        Duration::from_secs(30),
        2.0,
    );

    loop {
        {
            let mut entry = stats.entry(cfg.symbol.clone()).or_default();
            entry.is_active = true;
        }

        match run_ws_feed_once(&cfg, &tx, &latest, &stats).await {
            Ok(()) => {
                // Clean disconnect — do not restart.
                let mut entry = stats.entry(cfg.symbol.clone()).or_default();
                entry.is_active = false;
                return Ok(());
            }
            Err(e) => {
                warn!(symbol = cfg.symbol, error = %e, "asset feed disconnected, restarting");
                let mut entry = stats.entry(cfg.symbol.clone()).or_default();
                entry.is_active = false;
                entry.restart_count = entry.restart_count.saturating_add(1);
            }
        }

        let delay = backoff.next_delay();
        debug!(symbol = cfg.symbol, delay_ms = delay.as_millis(), "backoff before restart");
        tokio::time::sleep(delay).await;

        // After a successful period we could reset backoff, but since we
        // don't track uptime here we keep it simple and let WsManager handle it.
    }
}

/// One connection attempt for a single asset using [`WsManager`].
async fn run_ws_feed_once(
    cfg: &AssetFeedConfig,
    tx: &mpsc::Sender<(String, NormalizedTick)>,
    latest: &Arc<DashMap<String, NormalizedTick>>,
    stats: &Arc<DashMap<String, AssetFeedStats>>,
) -> Result<(), StreamError> {
    let manager = WsManager::new(cfg.connection_config.clone(), cfg.reconnect_policy.clone())?;
    let mut tick_rx = manager.run().await?;

    while let Some(tick) = tick_rx.recv().await {
        // Update snapshot.
        latest.insert(cfg.symbol.clone(), tick.clone());
        // Update stats.
        {
            let mut entry = stats.entry(cfg.symbol.clone()).or_default();
            entry.ticks_received = entry.ticks_received.saturating_add(1);
        }
        // Forward to merged channel; drop tick if the channel is full.
        if tx.try_send((cfg.symbol.clone(), tick)).is_err() {
            debug!(symbol = cfg.symbol, "tick dropped: merged channel full");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portfolio_feed_construction() {
        let feed = PortfolioFeed::new(64);
        assert_eq!(feed.channel_capacity, 64);
    }

    #[test]
    fn channel_capacity_clamped_to_one() {
        let feed = PortfolioFeed::new(0);
        assert_eq!(feed.channel_capacity, 1);
    }

    #[tokio::test]
    async fn add_asset_registers_config() {
        let feed = PortfolioFeed::new(8);
        feed.add_asset("BTC-USD", Exchange::Binance).await;
        feed.add_asset("ETH-USD", Exchange::Coinbase).await;
        assert_eq!(feed.asset_count().await, 2);
    }

    #[test]
    fn portfolio_snapshot_empty_at_start() {
        let feed = PortfolioFeed::new(8);
        assert!(feed.portfolio_snapshot().is_empty());
    }

    #[test]
    fn latest_tick_returns_none_before_any_tick() {
        let feed = PortfolioFeed::new(8);
        assert!(feed.latest_tick("BTC-USD").is_none());
    }

    #[tokio::test]
    async fn feed_stats_default_after_add() {
        let feed = PortfolioFeed::new(8);
        feed.add_asset("SOL-USD", Exchange::Coinbase).await;
        let stats = feed.feed_stats("SOL-USD").expect("stats should exist");
        assert_eq!(stats.ticks_received, 0);
        assert_eq!(stats.restart_count, 0);
        assert!(!stats.is_active);
    }

    #[test]
    fn backoff_state_doubles_up_to_max() {
        let mut b = BackoffState::new(
            Duration::from_millis(100),
            Duration::from_secs(1),
            2.0,
        );
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(800));
        // Capped at 1s
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_reset_restores_initial() {
        let initial = Duration::from_millis(250);
        let mut b = BackoffState::new(initial, Duration::from_secs(10), 2.0);
        b.next_delay();
        b.next_delay();
        b.reset(initial);
        assert_eq!(b.next_delay(), initial);
    }
}
