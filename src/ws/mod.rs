//! WebSocket connection management — auto-reconnect and backpressure.
//!
//! ## Responsibility
//! Manage the lifecycle of a WebSocket feed connection: connect, receive
//! messages, detect disconnections, apply exponential backoff reconnect,
//! and propagate backpressure when the downstream channel is full.
//!
//! ## Guarantees
//! - Non-panicking: all operations return Result
//! - Configurable: reconnect policy and buffer sizes are constructor params

use crate::error::StreamError;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

/// Statistics collected during WebSocket operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct WsStats {
    /// Total number of messages received (text + binary combined).
    pub total_messages_received: u64,
    /// Total bytes received across all messages.
    pub total_bytes_received: u64,
}

impl WsStats {
    /// Messages per second over the given elapsed window.
    ///
    /// Returns `0.0` if `elapsed_ms` is zero (avoids division by zero).
    pub fn message_rate(&self, elapsed_ms: u64) -> f64 {
        if elapsed_ms == 0 {
            return 0.0;
        }
        self.total_messages_received as f64 / (elapsed_ms as f64 / 1000.0)
    }

    /// Bytes per second over the given elapsed window.
    ///
    /// Returns `0.0` if `elapsed_ms` is zero.
    pub fn byte_rate(&self, elapsed_ms: u64) -> f64 {
        if elapsed_ms == 0 {
            return 0.0;
        }
        self.total_bytes_received as f64 / (elapsed_ms as f64 / 1000.0)
    }

    /// Average bytes per message: `total_bytes / total_messages`.
    ///
    /// Returns `None` if no messages have been received (avoids division by zero).
    pub fn avg_message_size(&self) -> Option<f64> {
        if self.total_messages_received == 0 {
            return None;
        }
        Some(self.total_bytes_received as f64 / self.total_messages_received as f64)
    }

    /// Total bytes received expressed as mebibytes (MiB): `total_bytes / 1_048_576.0`.
    pub fn total_data_mb(&self) -> f64 {
        self.total_bytes_received as f64 / 1_048_576.0
    }

    /// Returns `true` if the current message rate is below `min_rate` (msgs/s).
    ///
    /// Returns `true` when `elapsed_ms` is zero (no time elapsed → rate = 0).
    /// Useful for detecting stalled or silent feeds.
    pub fn is_idle(&self, elapsed_ms: u64, min_rate: f64) -> bool {
        self.message_rate(elapsed_ms) < min_rate
    }

    /// Returns `true` if at least one message has been received.
    pub fn has_traffic(&self) -> bool {
        self.total_messages_received > 0
    }

    /// Returns `true` if `total_messages_received >= threshold`.
    pub fn is_high_volume(&self, threshold: u64) -> bool {
        self.total_messages_received >= threshold
    }

    /// Average bytes per message received so far.
    ///
    /// Returns `None` if no messages have been received yet.
    pub fn bytes_per_message(&self) -> Option<f64> {
        if self.total_messages_received == 0 {
            return None;
        }
        Some(self.total_bytes_received as f64 / self.total_messages_received as f64)
    }


}

/// Reconnection policy for a WebSocket feed.
///
/// Controls exponential-backoff reconnect behaviour. Build with
/// [`ReconnectPolicy::new`] or use [`Default`] for sensible defaults.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Maximum number of reconnect attempts before giving up.
    pub max_attempts: u32,
    /// Initial backoff delay for the first reconnect attempt.
    pub initial_backoff: Duration,
    /// Maximum backoff delay (cap for exponential growth).
    pub max_backoff: Duration,
    /// Multiplier applied to the backoff on each successive attempt (must be >= 1.0).
    pub multiplier: f64,
    /// Jitter ratio in [0.0, 1.0]: the computed backoff is offset by up to
    /// `±ratio × backoff` using a deterministic per-attempt hash. 0.0 = no jitter.
    pub jitter: f64,
}

impl ReconnectPolicy {
    /// Build a reconnect policy with explicit parameters.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `multiplier < 1.0` (which would
    /// cause backoff to shrink over time) or if `max_attempts == 0`.
    pub fn new(
        max_attempts: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
        multiplier: f64,
    ) -> Result<Self, StreamError> {
        if multiplier < 1.0 {
            return Err(StreamError::ConfigError {
                reason: format!(
                    "reconnect multiplier must be >= 1.0, got {multiplier}"
                ),
            });
        }
        if max_attempts == 0 {
            return Err(StreamError::ConfigError {
                reason: "max_attempts must be > 0".into(),
            });
        }
        Ok(Self {
            max_attempts,
            initial_backoff,
            max_backoff,
            multiplier,
            jitter: 0.0,
        })
    }

    /// Set the maximum number of reconnect attempts.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `max_attempts` is zero.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Result<Self, StreamError> {
        if max_attempts == 0 {
            return Err(StreamError::ConfigError {
                reason: "max_attempts must be > 0".into(),
            });
        }
        self.max_attempts = max_attempts;
        Ok(self)
    }

    /// Set the exponential backoff multiplier.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `multiplier < 1.0` (which would
    /// cause the backoff to shrink on each attempt).
    pub fn with_multiplier(mut self, multiplier: f64) -> Result<Self, StreamError> {
        if multiplier < 1.0 {
            return Err(StreamError::ConfigError {
                reason: format!("reconnect multiplier must be >= 1.0, got {multiplier}"),
            });
        }
        self.multiplier = multiplier;
        Ok(self)
    }

    /// Set the initial backoff duration for the first reconnect attempt.
    pub fn with_initial_backoff(mut self, duration: Duration) -> Self {
        self.initial_backoff = duration;
        self
    }

    /// Set the maximum backoff duration (cap for exponential growth).
    pub fn with_max_backoff(mut self, duration: Duration) -> Self {
        self.max_backoff = duration;
        self
    }

    /// Apply deterministic per-attempt jitter to the computed backoff.
    ///
    /// `ratio` must be in `[0.0, 1.0]`. The effective backoff for attempt N
    /// will be spread uniformly over `[backoff*(1-ratio), backoff*(1+ratio)]`
    /// using a hash of the attempt index — no `rand` dependency needed.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `ratio` is outside `[0.0, 1.0]`.
    pub fn with_jitter(mut self, ratio: f64) -> Result<Self, StreamError> {
        if !(0.0..=1.0).contains(&ratio) {
            return Err(StreamError::ConfigError {
                reason: format!("jitter ratio must be in [0.0, 1.0], got {ratio}"),
            });
        }
        self.jitter = ratio;
        Ok(self)
    }

    /// Sum of all backoff delays across every reconnect attempt.
    ///
    /// Useful for estimating the worst-case time before a client gives up.
    /// The result is capped at `max_backoff * max_attempts` to avoid overflow.
    pub fn total_max_delay(&self) -> Duration {
        let total_ms: u64 = (0..self.max_attempts)
            .map(|a| self.backoff_for_attempt(a).as_millis() as u64)
            .fold(0u64, |acc, ms| acc.saturating_add(ms));
        Duration::from_millis(total_ms)
    }

    /// Maximum number of reconnect attempts before the client gives up.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Number of attempts remaining starting from `current_attempt` (0-indexed).
    ///
    /// Returns `0` if `current_attempt >= max_attempts`.
    pub fn total_attempts_remaining(&self, current_attempt: u32) -> u32 {
        self.max_attempts.saturating_sub(current_attempt)
    }

    /// Backoff delay that will be applied *after* `current_attempt` completes.
    ///
    /// Equivalent to `backoff_for_attempt(current_attempt + 1)`, capped at
    /// `max_backoff`. Saturates rather than wrapping when `current_attempt`
    /// is `u32::MAX`.
    pub fn delay_for_next(&self, current_attempt: u32) -> Duration {
        self.backoff_for_attempt(current_attempt.saturating_add(1))
    }

    /// Returns `true` if `attempts` has reached or exceeded `max_attempts`.
    ///
    /// After this returns `true` the reconnect loop should give up rather than
    /// scheduling another attempt.
    pub fn is_exhausted(&self, attempts: u32) -> bool {
        attempts >= self.max_attempts
    }

    /// Backoff duration for attempt N (0-indexed).
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.powi(attempt as i32);
        // Cap the f64 value *before* casting to u64. When `attempt` is large
        // (e.g. 63 with multiplier=2.0), `factor` becomes f64::INFINITY.
        // Casting f64::INFINITY as u64 is undefined behaviour in Rust — it
        // saturates to 0 on some targets and panics in debug builds. Clamping
        // to max_backoff in floating-point space first avoids the UB entirely.
        let max_ms = self.max_backoff.as_millis() as f64;
        let base_ms = (self.initial_backoff.as_millis() as f64 * factor).min(max_ms);
        let ms = if self.jitter > 0.0 {
            // Deterministic noise via Knuth multiplicative hash of the attempt index.
            let hash = (attempt as u64)
                .wrapping_mul(2654435769)
                .wrapping_add(1013904223);
            let noise = (hash & 0xFFFF) as f64 / 65535.0; // [0.0, 1.0]
            let delta = base_ms * self.jitter * (noise * 2.0 - 1.0); // ±jitter×base
            (base_ms + delta).clamp(0.0, max_ms)
        } else {
            base_ms
        };
        Duration::from_millis(ms as u64)
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            multiplier: 2.0,
            jitter: 0.0,
        }
    }
}

/// Configuration for a WebSocket feed connection.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// WebSocket URL to connect to (e.g. `"wss://stream.binance.com:9443/ws"`).
    pub url: String,
    /// Capacity of the downstream channel that receives incoming messages.
    pub channel_capacity: usize,
    /// Reconnect policy applied on disconnection.
    pub reconnect: ReconnectPolicy,
    /// Ping interval to keep the connection alive (default: 20 s).
    pub ping_interval: Duration,
}

impl ConnectionConfig {
    /// Build a connection configuration for `url` with the given downstream
    /// channel capacity.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `url` is empty or
    /// `channel_capacity` is zero.
    pub fn new(url: impl Into<String>, channel_capacity: usize) -> Result<Self, StreamError> {
        let url = url.into();
        if url.is_empty() {
            return Err(StreamError::ConfigError {
                reason: "WebSocket URL must not be empty".into(),
            });
        }
        if channel_capacity == 0 {
            return Err(StreamError::ConfigError {
                reason: "channel_capacity must be > 0".into(),
            });
        }
        Ok(Self {
            url,
            channel_capacity,
            reconnect: ReconnectPolicy::default(),
            ping_interval: Duration::from_secs(20),
        })
    }

    /// Override the default reconnect policy.
    pub fn with_reconnect(mut self, policy: ReconnectPolicy) -> Self {
        self.reconnect = policy;
        self
    }

    /// Override the keepalive ping interval (default: 20 s).
    pub fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Shortcut to set only the reconnect attempt limit without replacing the
    /// entire reconnect policy.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `n` is zero.
    pub fn with_reconnect_attempts(mut self, n: u32) -> Result<Self, StreamError> {
        self.reconnect = self.reconnect.with_max_attempts(n)?;
        Ok(self)
    }

    /// Override the downstream channel capacity.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if `capacity` is zero.
    pub fn with_channel_capacity(mut self, capacity: usize) -> Result<Self, StreamError> {
        if capacity == 0 {
            return Err(StreamError::ConfigError {
                reason: "channel_capacity must be > 0".into(),
            });
        }
        self.channel_capacity = capacity;
        Ok(self)
    }
}

/// Manages a single WebSocket feed: connect, receive, reconnect.
///
/// Call [`WsManager::run`] to enter the connection loop. Messages are forwarded
/// to the [`mpsc::Sender`] supplied to `run`; the loop retries on disconnection
/// according to the [`ReconnectPolicy`] in the [`ConnectionConfig`].
pub struct WsManager {
    config: ConnectionConfig,
    connect_attempts: u32,
    is_connected: bool,
    stats: WsStats,
}

impl WsManager {
    /// Create a new manager from a validated [`ConnectionConfig`].
    pub fn new(config: ConnectionConfig) -> Self {
        Self {
            config,
            connect_attempts: 0,
            is_connected: false,
            stats: WsStats::default(),
        }
    }

    /// Run the WebSocket connection loop, forwarding text messages to `message_tx`.
    ///
    /// The loop connects, reads frames until the socket closes or errors, then
    /// waits the configured backoff and reconnects. Returns when either:
    /// - `message_tx` is closed (receiver dropped), or
    /// - reconnect attempts are exhausted ([`StreamError::ReconnectExhausted`]).
    ///
    /// `outbound_rx` is an optional channel for sending messages **to** the
    /// server (e.g., subscription requests). When provided, any string received
    /// on this channel is forwarded to the WebSocket as a text frame.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ReconnectExhausted`] after all reconnect slots
    /// are consumed, or the underlying connection error if reconnects are
    /// exhausted immediately on the first attempt.
    pub async fn run(
        &mut self,
        message_tx: mpsc::Sender<String>,
        mut outbound_rx: Option<mpsc::Receiver<String>>,
    ) -> Result<(), StreamError> {
        loop {
            info!(url = %self.config.url, attempt = self.connect_attempts, "connecting");
            match self.try_connect(&message_tx, &mut outbound_rx).await {
                Ok(()) => {
                    // Clean close — receiver dropped or server sent Close frame.
                    self.is_connected = false;
                    debug!(url = %self.config.url, "connection closed cleanly");
                    if message_tx.is_closed() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    self.is_connected = false;
                    warn!(url = %self.config.url, error = %e, "connection error");
                }
            }

            if !self.can_reconnect() {
                return Err(StreamError::ReconnectExhausted {
                    url: self.config.url.clone(),
                    attempts: self.connect_attempts,
                });
            }
            let backoff = self.next_reconnect_backoff()?;
            info!(url = %self.config.url, backoff_ms = backoff.as_millis(), "reconnecting after backoff");
            tokio::time::sleep(backoff).await;
        }
    }

    /// Attempt a single connection, reading messages until close or error.
    async fn try_connect(
        &mut self,
        message_tx: &mpsc::Sender<String>,
        outbound_rx: &mut Option<mpsc::Receiver<String>>,
    ) -> Result<(), StreamError> {
        let (ws_stream, _response) =
            connect_async(&self.config.url)
                .await
                .map_err(|e| StreamError::ConnectionFailed {
                    url: self.config.url.clone(),
                    reason: e.to_string(),
                })?;

        self.is_connected = true;
        self.connect_attempts += 1;
        info!(url = %self.config.url, "connected");

        let (mut write, mut read) = ws_stream.split();
        let mut ping_interval = time::interval(self.config.ping_interval);
        // Skip the first tick so we don't ping immediately on connect.
        ping_interval.tick().await;

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.stats.total_messages_received += 1;
                            self.stats.total_bytes_received += text.len() as u64;
                            if message_tx.send(text.to_string()).await.is_err() {
                                // Receiver dropped — clean shutdown.
                                return Ok(());
                            }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            self.stats.total_messages_received += 1;
                            self.stats.total_bytes_received += bytes.len() as u64;
                            if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                                if message_tx.send(text).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                            // Control frames handled by tungstenite internally.
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            return Ok(());
                        }
                        Some(Err(e)) => {
                            return Err(StreamError::WebSocket(e.to_string()));
                        }
                    }
                }
                _ = ping_interval.tick() => {
                    debug!(url = %self.config.url, "sending keepalive ping");
                    if write.send(Message::Ping(vec![].into())).await.is_err() {
                        return Ok(());
                    }
                }
                outbound = recv_outbound(outbound_rx) => {
                    if let Some(text) = outbound {
                        let _ = write.send(Message::Text(text.into())).await;
                    }
                }
            }
        }
    }

    /// Simulate a connection (for testing without live WebSocket).
    /// Increments `connect_attempts` to reflect the initial connection slot.
    pub fn connect_simulated(&mut self) {
        self.connect_attempts += 1;
        self.is_connected = true;
    }

    /// Simulate a disconnection.
    pub fn disconnect_simulated(&mut self) {
        self.is_connected = false;
    }

    /// Whether the managed connection is currently in the connected state.
    pub fn is_connected(&self) -> bool {
        self.is_connected
    }

    /// Total connection attempts made so far (including the initial connect).
    pub fn connect_attempts(&self) -> u32 {
        self.connect_attempts
    }

    /// The configuration this manager was created with.
    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    /// Cumulative receive statistics for this manager.
    pub fn stats(&self) -> &WsStats {
        &self.stats
    }

    /// Check whether the next reconnect attempt is allowed.
    pub fn can_reconnect(&self) -> bool {
        self.connect_attempts < self.config.reconnect.max_attempts
    }

    /// Consume a reconnect slot and return the backoff duration to wait.
    pub fn next_reconnect_backoff(&mut self) -> Result<Duration, StreamError> {
        if !self.can_reconnect() {
            return Err(StreamError::ReconnectExhausted {
                url: self.config.url.clone(),
                attempts: self.connect_attempts,
            });
        }
        let backoff = self
            .config
            .reconnect
            .backoff_for_attempt(self.connect_attempts);
        self.connect_attempts += 1;
        Ok(backoff)
    }
}

/// Helper: receive from an optional mpsc channel, or never resolve if `None`.
///
/// Used in `tokio::select!` to make the outbound branch dormant when no
/// outbound channel was supplied, without allocating or spinning.
async fn recv_outbound(rx: &mut Option<mpsc::Receiver<String>>) -> Option<String> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ConnectionConfig {
        ConnectionConfig::new("wss://example.com/ws", 1024).unwrap()
    }

    #[test]
    fn test_reconnect_policy_default_values() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.max_attempts, 10);
        assert_eq!(p.multiplier, 2.0);
    }

    #[test]
    fn test_reconnect_policy_backoff_exponential() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0)
            .unwrap();
        assert_eq!(p.backoff_for_attempt(0), Duration::from_millis(100));
        assert_eq!(p.backoff_for_attempt(1), Duration::from_millis(200));
        assert_eq!(p.backoff_for_attempt(2), Duration::from_millis(400));
    }

    #[test]
    fn test_reconnect_policy_backoff_capped_at_max() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(1000), Duration::from_secs(5), 2.0)
            .unwrap();
        let backoff = p.backoff_for_attempt(10);
        assert!(backoff <= Duration::from_secs(5));
    }

    #[test]
    fn test_reconnect_policy_multiplier_below_1_rejected() {
        let result =
            ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 0.5);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_reconnect_policy_zero_attempts_rejected() {
        let result =
            ReconnectPolicy::new(0, Duration::from_millis(100), Duration::from_secs(30), 2.0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_connection_config_empty_url_rejected() {
        let result = ConnectionConfig::new("", 1024);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_connection_config_zero_capacity_rejected() {
        let result = ConnectionConfig::new("wss://example.com", 0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_connection_config_with_reconnect() {
        let policy =
            ReconnectPolicy::new(3, Duration::from_millis(200), Duration::from_secs(10), 2.0)
                .unwrap();
        let config = default_config().with_reconnect(policy);
        assert_eq!(config.reconnect.max_attempts, 3);
    }

    #[test]
    fn test_connection_config_with_ping_interval() {
        let config = default_config().with_ping_interval(Duration::from_secs(30));
        assert_eq!(config.ping_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_ws_manager_initial_state() {
        let mgr = WsManager::new(default_config());
        assert!(!mgr.is_connected());
        assert_eq!(mgr.connect_attempts(), 0);
    }

    #[test]
    fn test_ws_manager_connect_simulated() {
        let mut mgr = WsManager::new(default_config());
        mgr.connect_simulated();
        assert!(mgr.is_connected());
        assert_eq!(mgr.connect_attempts(), 1);
    }

    #[test]
    fn test_ws_manager_disconnect_simulated() {
        let mut mgr = WsManager::new(default_config());
        mgr.connect_simulated();
        mgr.disconnect_simulated();
        assert!(!mgr.is_connected());
    }

    #[test]
    fn test_ws_manager_can_reconnect_within_limit() {
        let mut mgr = WsManager::new(
            default_config().with_reconnect(
                ReconnectPolicy::new(3, Duration::from_millis(10), Duration::from_secs(1), 2.0)
                    .unwrap(),
            ),
        );
        assert!(mgr.can_reconnect());
        mgr.next_reconnect_backoff().unwrap();
        mgr.next_reconnect_backoff().unwrap();
        mgr.next_reconnect_backoff().unwrap();
        assert!(!mgr.can_reconnect());
    }

    #[test]
    fn test_ws_manager_reconnect_exhausted_error() {
        let mut mgr = WsManager::new(
            default_config().with_reconnect(
                ReconnectPolicy::new(1, Duration::from_millis(10), Duration::from_secs(1), 2.0)
                    .unwrap(),
            ),
        );
        mgr.next_reconnect_backoff().unwrap();
        let result = mgr.next_reconnect_backoff();
        assert!(matches!(
            result,
            Err(StreamError::ReconnectExhausted { .. })
        ));
    }

    #[test]
    fn test_ws_manager_backoff_increases() {
        let mut mgr = WsManager::new(
            default_config().with_reconnect(
                ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(30), 2.0)
                    .unwrap(),
            ),
        );
        let b0 = mgr.next_reconnect_backoff().unwrap();
        let b1 = mgr.next_reconnect_backoff().unwrap();
        assert!(b1 >= b0);
    }

    #[test]
    fn test_ws_manager_config_accessor() {
        let mgr = WsManager::new(default_config());
        assert_eq!(mgr.config().url, "wss://example.com/ws");
        assert_eq!(mgr.config().channel_capacity, 1024);
    }

    /// Verify that `recv_outbound` with `None` never resolves (returns pending).
    /// We test this by racing it against a resolved future and confirming the
    /// resolved future always wins.
    #[tokio::test]
    async fn test_recv_outbound_none_is_always_pending() {
        let mut rx: Option<mpsc::Receiver<String>> = None;
        // Race recv_outbound(None) against an immediately-ready future.
        tokio::select! {
            _ = recv_outbound(&mut rx) => {
                panic!("recv_outbound(None) should never resolve");
            }
            _ = std::future::ready(()) => {
                // Expected: the ready() future wins.
            }
        }
    }

    /// Verify that `recv_outbound` with `Some(rx)` resolves when a message arrives.
    #[tokio::test]
    async fn test_recv_outbound_some_resolves_with_message() {
        let (tx, mut channel_rx) = mpsc::channel::<String>(1);
        tx.send("subscribe".into()).await.unwrap();
        let mut rx: Option<mpsc::Receiver<String>> = Some(channel_rx);
        let msg = recv_outbound(&mut rx).await;
        assert_eq!(msg.as_deref(), Some("subscribe"));
        // Re-borrow to confirm the channel holds the receiver
        let _ = rx;
    }

    #[test]
    fn test_ws_stats_initial_zero() {
        let mgr = WsManager::new(default_config());
        let s = mgr.stats();
        assert_eq!(s.total_messages_received, 0);
        assert_eq!(s.total_bytes_received, 0);
    }

    #[test]
    fn test_ws_stats_default() {
        let s = WsStats::default();
        assert_eq!(s.total_messages_received, 0);
        assert_eq!(s.total_bytes_received, 0);
    }

    #[test]
    fn test_reconnect_policy_with_jitter_valid() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0)
            .unwrap()
            .with_jitter(0.5)
            .unwrap();
        assert_eq!(p.jitter, 0.5);
    }

    #[test]
    fn test_reconnect_policy_with_jitter_zero_is_deterministic() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0)
            .unwrap()
            .with_jitter(0.0)
            .unwrap();
        // Zero jitter: backoff must be identical to un-jittered value.
        let b0 = p.backoff_for_attempt(0);
        let b1 = p.backoff_for_attempt(0);
        assert_eq!(b0, b1);
        assert_eq!(b0, Duration::from_millis(100));
    }

    #[test]
    fn test_reconnect_policy_with_jitter_invalid_ratio() {
        let result =
            ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0)
                .unwrap()
                .with_jitter(1.5);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_reconnect_policy_with_jitter_negative_ratio() {
        let result =
            ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0)
                .unwrap()
                .with_jitter(-0.1);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_reconnect_policy_with_jitter_stays_within_bounds() {
        let p = ReconnectPolicy::new(20, Duration::from_millis(100), Duration::from_secs(30), 2.0)
            .unwrap()
            .with_jitter(1.0)
            .unwrap();
        // With ratio=1.0 the backoff can range [0, 2×base] but must not exceed max_backoff.
        for attempt in 0..20 {
            let b = p.backoff_for_attempt(attempt);
            assert!(b <= Duration::from_secs(30), "attempt {attempt} exceeded max_backoff");
        }
    }

    #[test]
    fn test_reconnect_policy_with_max_attempts_valid() {
        let p = ReconnectPolicy::default().with_max_attempts(5).unwrap();
        assert_eq!(p.max_attempts, 5);
    }

    #[test]
    fn test_reconnect_policy_with_max_attempts_zero_rejected() {
        let result = ReconnectPolicy::default().with_max_attempts(0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_connection_config_with_reconnect_attempts_valid() {
        let config = default_config().with_reconnect_attempts(3).unwrap();
        assert_eq!(config.reconnect.max_attempts, 3);
    }

    #[test]
    fn test_connection_config_with_reconnect_attempts_zero_rejected() {
        let result = default_config().with_reconnect_attempts(0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_reconnect_policy_total_max_delay_sum_of_backoffs() {
        let p = ReconnectPolicy::new(3, Duration::from_millis(100), Duration::from_secs(30), 2.0)
            .unwrap();
        // attempts 0,1,2 → 100ms, 200ms, 400ms = 700ms
        assert_eq!(p.total_max_delay(), Duration::from_millis(700));
    }

    #[test]
    fn test_reconnect_policy_total_max_delay_capped_by_max_backoff() {
        // With small max_backoff, all delays are capped
        let p = ReconnectPolicy::new(5, Duration::from_millis(1000), Duration::from_millis(500), 2.0)
            .unwrap();
        // All 5 attempts capped at 500ms → total = 2500ms
        assert_eq!(p.total_max_delay(), Duration::from_millis(2500));
    }

    #[test]
    fn test_connection_config_with_channel_capacity_valid() {
        let config = default_config().with_channel_capacity(512).unwrap();
        assert_eq!(config.channel_capacity, 512);
    }

    #[test]
    fn test_connection_config_with_channel_capacity_zero_rejected() {
        let result = default_config().with_channel_capacity(0);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_reconnect_policy_with_jitter_varies_across_attempts() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(1000), Duration::from_secs(30), 1.0)
            .unwrap()
            .with_jitter(0.5)
            .unwrap();
        // With constant multiplier=1.0 all base backoffs are 1000ms; jitter should
        // produce different values for different attempt indices.
        let values: Vec<Duration> = (0..10).map(|a| p.backoff_for_attempt(a)).collect();
        let unique: std::collections::HashSet<u64> =
            values.iter().map(|d| d.as_millis() as u64).collect();
        assert!(unique.len() > 1, "jitter should produce variation across attempts");
    }

    // ── ReconnectPolicy::with_initial_backoff / with_max_backoff ──────────────

    #[test]
    fn test_with_initial_backoff_sets_value() {
        let p = ReconnectPolicy::default()
            .with_initial_backoff(Duration::from_secs(2));
        assert_eq!(p.initial_backoff, Duration::from_secs(2));
    }

    #[test]
    fn test_with_max_backoff_sets_value() {
        let p = ReconnectPolicy::default()
            .with_max_backoff(Duration::from_secs(60));
        assert_eq!(p.max_backoff, Duration::from_secs(60));
    }

    #[test]
    fn test_with_initial_backoff_affects_first_attempt() {
        let p = ReconnectPolicy::default()
            .with_initial_backoff(Duration::from_millis(200));
        assert_eq!(p.backoff_for_attempt(0), Duration::from_millis(200));
    }

    // ── ReconnectPolicy::with_multiplier ──────────────────────────────────────

    #[test]
    fn test_with_multiplier_valid() {
        let p = ReconnectPolicy::default().with_multiplier(3.0).unwrap();
        assert_eq!(p.multiplier, 3.0);
    }

    #[test]
    fn test_with_multiplier_below_one_rejected() {
        let result = ReconnectPolicy::default().with_multiplier(0.9);
        assert!(matches!(result, Err(StreamError::ConfigError { .. })));
    }

    #[test]
    fn test_with_multiplier_exactly_one_accepted() {
        let p = ReconnectPolicy::default().with_multiplier(1.0).unwrap();
        assert_eq!(p.multiplier, 1.0);
    }

    // ── WsStats::message_rate / byte_rate ─────────────────────────────────────

    #[test]
    fn test_message_rate_zero_elapsed_returns_zero() {
        let stats = WsStats {
            total_messages_received: 100,
            total_bytes_received: 50_000,
        };
        assert_eq!(stats.message_rate(0), 0.0);
        assert_eq!(stats.byte_rate(0), 0.0);
    }

    #[test]
    fn test_message_rate_100_messages_in_1s() {
        let stats = WsStats {
            total_messages_received: 100,
            total_bytes_received: 0,
        };
        let rate = stats.message_rate(1_000); // 1 second = 1000ms
        assert!((rate - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_byte_rate_1mb_in_1s() {
        let stats = WsStats {
            total_messages_received: 0,
            total_bytes_received: 1_000_000,
        };
        let rate = stats.byte_rate(1_000); // 1 second
        assert!((rate - 1_000_000.0).abs() < 1.0);
    }

    // ── WsStats::avg_message_size ─────────────────────────────────────────────

    #[test]
    fn test_avg_message_size_none_when_no_messages() {
        let stats = WsStats::default();
        assert!(stats.avg_message_size().is_none());
    }

    #[test]
    fn test_avg_message_size_basic() {
        let stats = WsStats {
            total_messages_received: 10,
            total_bytes_received: 1_000,
        };
        let avg = stats.avg_message_size().unwrap();
        assert!((avg - 100.0).abs() < 1e-9);
    }

    // ── WsStats::total_data_mb ────────────────────────────────────────────────

    #[test]
    fn test_total_data_mb_zero_bytes() {
        let stats = WsStats::default();
        assert!((stats.total_data_mb() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_total_data_mb_one_mib() {
        let stats = WsStats {
            total_messages_received: 1,
            total_bytes_received: 1_048_576,
        };
        assert!((stats.total_data_mb() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_max_attempts_getter_matches_field() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.max_attempts(), p.max_attempts);
    }

    #[test]
    fn test_max_attempts_getter_after_new() {
        let p = ReconnectPolicy::new(
            7,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_secs(30),
            2.0,
        )
        .unwrap();
        assert_eq!(p.max_attempts(), 7);
    }

    // ── WsStats::is_idle ──────────────────────────────────────────────────────

    #[test]
    fn test_is_idle_below_min_rate() {
        let stats = WsStats {
            total_messages_received: 1,
            total_bytes_received: 0,
        };
        // 1 msg / 10s = 0.1 msg/s; min_rate = 1.0 → idle
        assert!(stats.is_idle(10_000, 1.0));
    }

    #[test]
    fn test_is_idle_above_min_rate() {
        let stats = WsStats {
            total_messages_received: 100,
            total_bytes_received: 0,
        };
        // 100 msg / 1s = 100 msg/s; min_rate = 1.0 → not idle
        assert!(!stats.is_idle(1_000, 1.0));
    }

    #[test]
    fn test_is_idle_zero_messages_always_idle() {
        let stats = WsStats::default();
        assert!(stats.is_idle(1_000, 0.001));
    }

    #[test]
    fn test_total_attempts_remaining_full() {
        let p = ReconnectPolicy::default(); // max_attempts = 10
        assert_eq!(p.total_attempts_remaining(0), 10);
    }

    #[test]
    fn test_total_attempts_remaining_partial() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.total_attempts_remaining(3), 7);
    }

    #[test]
    fn test_total_attempts_remaining_exhausted() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.total_attempts_remaining(10), 0);
        assert_eq!(p.total_attempts_remaining(99), 0);
    }

    // ── WsStats::has_traffic ──────────────────────────────────────────────────

    #[test]
    fn test_has_traffic_false_when_no_messages() {
        let stats = WsStats::default();
        assert!(!stats.has_traffic());
    }

    #[test]
    fn test_has_traffic_true_after_one_message() {
        let stats = WsStats {
            total_messages_received: 1,
            total_bytes_received: 0,
        };
        assert!(stats.has_traffic());
    }

    #[test]
    fn test_has_traffic_true_with_many_messages() {
        let stats = WsStats {
            total_messages_received: 1_000,
            total_bytes_received: 50_000,
        };
        assert!(stats.has_traffic());
    }

    // ── WsStats::is_high_volume ───────────────────────────────────────────────

    #[test]
    fn test_is_high_volume_true_at_threshold() {
        let stats = WsStats { total_messages_received: 1_000, total_bytes_received: 0 };
        assert!(stats.is_high_volume(1_000));
    }

    #[test]
    fn test_is_high_volume_false_below_threshold() {
        let stats = WsStats { total_messages_received: 500, total_bytes_received: 0 };
        assert!(!stats.is_high_volume(1_000));
    }

    #[test]
    fn test_is_high_volume_true_above_threshold() {
        let stats = WsStats { total_messages_received: 2_000, total_bytes_received: 0 };
        assert!(stats.is_high_volume(1_000));
    }

    // ── WsStats::bytes_per_message ────────────────────────────────────────────

    #[test]
    fn test_bytes_per_message_none_when_no_messages() {
        let stats = WsStats { total_messages_received: 0, total_bytes_received: 0 };
        assert!(stats.bytes_per_message().is_none());
    }

    #[test]
    fn test_bytes_per_message_correct_value() {
        let stats = WsStats { total_messages_received: 4, total_bytes_received: 400 };
        assert_eq!(stats.bytes_per_message(), Some(100.0));
    }

    #[test]
    fn test_bytes_per_message_fractional() {
        let stats = WsStats { total_messages_received: 3, total_bytes_received: 10 };
        let bpm = stats.bytes_per_message().unwrap();
        assert!((bpm - 10.0 / 3.0).abs() < 1e-10);
    }

    // --- delay_for_next ---

    #[test]
    fn test_delay_for_next_is_backoff_for_attempt_plus_one() {
        let policy = ReconnectPolicy::new(
            10,
            Duration::from_millis(100),
            Duration::from_secs(60),
            2.0,
        )
        .unwrap();
        assert_eq!(
            policy.delay_for_next(0),
            policy.backoff_for_attempt(1)
        );
        assert_eq!(
            policy.delay_for_next(3),
            policy.backoff_for_attempt(4)
        );
    }

    #[test]
    fn test_delay_for_next_saturates_at_max_backoff() {
        let policy = ReconnectPolicy::new(
            10,
            Duration::from_millis(100),
            Duration::from_secs(1),
            2.0,
        )
        .unwrap();
        // After many attempts the delay is capped at max_backoff
        assert!(policy.delay_for_next(100) <= Duration::from_secs(1));
    }

    // ── WsStats::message_rate ─────────────────────────────────────────────────

    #[test]
    fn test_message_rate_zero_when_elapsed_is_zero() {
        let stats = WsStats { total_messages_received: 1_000, total_bytes_received: 0 };
        assert_eq!(stats.message_rate(0), 0.0);
    }

    #[test]
    fn test_message_rate_correct_value() {
        let stats = WsStats { total_messages_received: 100, total_bytes_received: 0 };
        // 100 messages in 10_000ms = 10 msg/s
        assert!((stats.message_rate(10_000) - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_message_rate_zero_messages() {
        let stats = WsStats { total_messages_received: 0, total_bytes_received: 0 };
        assert_eq!(stats.message_rate(5_000), 0.0);
    }

    // --- is_exhausted ---

    #[test]
    fn test_is_exhausted_true_at_max_attempts() {
        let policy = ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(10), 2.0).unwrap();
        assert!(policy.is_exhausted(5));
    }

    #[test]
    fn test_is_exhausted_true_beyond_max_attempts() {
        let policy = ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(10), 2.0).unwrap();
        assert!(policy.is_exhausted(10));
    }

    #[test]
    fn test_is_exhausted_false_below_max_attempts() {
        let policy = ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(10), 2.0).unwrap();
        assert!(!policy.is_exhausted(4));
    }
}
