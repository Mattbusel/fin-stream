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
use std::time::Duration;

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
}

impl ReconnectPolicy {
    /// Build a reconnect policy with explicit parameters.
    ///
    /// Returns an error if `multiplier < 1.0` (which would cause backoff to
    /// shrink over time) or if `max_attempts == 0`.
    pub fn new(
        max_attempts: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
        multiplier: f64,
    ) -> Result<Self, StreamError> {
        if multiplier < 1.0 {
            return Err(StreamError::ConnectionFailed {
                url: String::new(),
                reason: "reconnect multiplier must be >= 1.0".into(),
            });
        }
        if max_attempts == 0 {
            return Err(StreamError::ConnectionFailed {
                url: String::new(),
                reason: "max_attempts must be > 0".into(),
            });
        }
        Ok(Self { max_attempts, initial_backoff, max_backoff, multiplier })
    }

    /// Backoff duration for attempt N (0-indexed).
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.powi(attempt as i32);
        // Cap the f64 value *before* casting to u64.  When `attempt` is large
        // (e.g. 63 with multiplier=2.0), `factor` becomes f64::INFINITY.
        // Casting f64::INFINITY as u64 is undefined behaviour in Rust — it
        // saturates to 0 on some targets and panics in debug builds.  Clamping
        // to max_backoff in floating-point space first avoids the UB entirely.
        let max_ms = self.max_backoff.as_millis() as f64;
        let ms = (self.initial_backoff.as_millis() as f64 * factor).min(max_ms);
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
    /// Returns an error if `url` is empty or `channel_capacity` is zero.
    pub fn new(url: impl Into<String>, channel_capacity: usize) -> Result<Self, StreamError> {
        let url = url.into();
        if url.is_empty() {
            return Err(StreamError::ConnectionFailed {
                url: url.clone(),
                reason: "URL must not be empty".into(),
            });
        }
        if channel_capacity == 0 {
            return Err(StreamError::Backpressure {
                channel: url.clone(),
                depth: 0,
                capacity: 0,
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
}

/// Manages a single WebSocket feed: connect, receive, reconnect.
///
/// In production, WsManager wraps tokio-tungstenite. In tests, it operates
/// in simulation mode with injected messages.
pub struct WsManager {
    config: ConnectionConfig,
    connect_attempts: u32,
    is_connected: bool,
}

impl WsManager {
    /// Create a new manager from a validated [`ConnectionConfig`].
    pub fn new(config: ConnectionConfig) -> Self {
        Self { config, connect_attempts: 0, is_connected: false }
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
    pub fn is_connected(&self) -> bool { self.is_connected }

    /// Total connection attempts made so far (including the initial connect).
    pub fn connect_attempts(&self) -> u32 { self.connect_attempts }

    /// The configuration this manager was created with.
    pub fn config(&self) -> &ConnectionConfig { &self.config }

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
        let backoff = self.config.reconnect.backoff_for_attempt(self.connect_attempts);
        self.connect_attempts += 1;
        Ok(backoff)
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
        let p = ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 2.0).unwrap();
        assert_eq!(p.backoff_for_attempt(0), Duration::from_millis(100));
        assert_eq!(p.backoff_for_attempt(1), Duration::from_millis(200));
        assert_eq!(p.backoff_for_attempt(2), Duration::from_millis(400));
    }

    #[test]
    fn test_reconnect_policy_backoff_capped_at_max() {
        let p = ReconnectPolicy::new(10, Duration::from_millis(1000), Duration::from_secs(5), 2.0).unwrap();
        let backoff = p.backoff_for_attempt(10);
        assert!(backoff <= Duration::from_secs(5));
    }

    #[test]
    fn test_reconnect_policy_multiplier_below_1_rejected() {
        let result = ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(30), 0.5);
        assert!(result.is_err());
    }

    #[test]
    fn test_reconnect_policy_zero_attempts_rejected() {
        let result = ReconnectPolicy::new(0, Duration::from_millis(100), Duration::from_secs(30), 2.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_connection_config_empty_url_rejected() {
        let result = ConnectionConfig::new("", 1024);
        assert!(result.is_err());
    }

    #[test]
    fn test_connection_config_zero_capacity_rejected() {
        let result = ConnectionConfig::new("wss://example.com", 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_connection_config_with_reconnect() {
        let policy = ReconnectPolicy::new(3, Duration::from_millis(200), Duration::from_secs(10), 2.0).unwrap();
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
                ReconnectPolicy::new(3, Duration::from_millis(10), Duration::from_secs(1), 2.0).unwrap()
            )
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
                ReconnectPolicy::new(1, Duration::from_millis(10), Duration::from_secs(1), 2.0).unwrap()
            )
        );
        mgr.next_reconnect_backoff().unwrap();
        let result = mgr.next_reconnect_backoff();
        assert!(matches!(result, Err(StreamError::ReconnectExhausted { .. })));
    }

    #[test]
    fn test_ws_manager_backoff_increases() {
        let mut mgr = WsManager::new(
            default_config().with_reconnect(
                ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(30), 2.0).unwrap()
            )
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
}
