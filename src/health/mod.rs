//! Feed health monitoring — staleness detection and status tracking.
//!
//! ## Responsibility
//! Track the timestamp of the most recent tick per feed. Emit StaleFeed
//! errors when a feed has not produced data within the configured threshold.
//!
//! ## Guarantees
//! - Thread-safe: HealthMonitor uses DashMap for concurrent updates
//! - Non-panicking: all operations return Result or Option

use crate::error::StreamError;
use dashmap::DashMap;
use std::sync::Arc;

/// Health status of a feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HealthStatus {
    /// Feed is active and within staleness threshold.
    Healthy,
    /// Feed has not produced data within the staleness threshold.
    Stale,
    /// Feed is newly registered, no data received yet.
    Unknown,
}

/// Per-feed health state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedHealth {
    pub feed_id: String,
    pub status: HealthStatus,
    pub last_tick_ms: Option<u64>,
    pub stale_threshold_ms: u64,
    pub tick_count: u64,
}

impl FeedHealth {
    /// Elapsed ms since last tick.
    pub fn elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        self.last_tick_ms.map(|t| now_ms.saturating_sub(t))
    }
}

/// Central health monitor for all active feeds.
pub struct HealthMonitor {
    feeds: Arc<DashMap<String, FeedHealth>>,
    default_stale_threshold_ms: u64,
}

impl HealthMonitor {
    pub fn new(default_stale_threshold_ms: u64) -> Self {
        Self {
            feeds: Arc::new(DashMap::new()),
            default_stale_threshold_ms,
        }
    }

    /// Register a feed with optional custom staleness threshold.
    pub fn register(&self, feed_id: impl Into<String>, stale_threshold_ms: Option<u64>) {
        let id = feed_id.into();
        let threshold = stale_threshold_ms.unwrap_or(self.default_stale_threshold_ms);
        self.feeds.insert(id.clone(), FeedHealth {
            feed_id: id,
            status: HealthStatus::Unknown,
            last_tick_ms: None,
            stale_threshold_ms: threshold,
            tick_count: 0,
        });
    }

    /// Record a tick heartbeat for a feed.
    pub fn heartbeat(&self, feed_id: &str, ts_ms: u64) -> Result<(), StreamError> {
        let mut entry = self.feeds.get_mut(feed_id).ok_or_else(|| StreamError::StaleFeed {
            feed_id: feed_id.to_string(),
            elapsed_ms: 0,
            threshold_ms: 0,
        })?;
        entry.last_tick_ms = Some(ts_ms);
        entry.tick_count += 1;
        entry.status = HealthStatus::Healthy;
        Ok(())
    }

    /// Check all feeds for staleness at the given timestamp.
    /// Returns a list of errors for stale feeds.
    pub fn check_all(&self, now_ms: u64) -> Vec<StreamError> {
        let mut errors = Vec::new();
        for mut entry in self.feeds.iter_mut() {
            let elapsed = entry.last_tick_ms.map(|t| now_ms.saturating_sub(t));
            if let Some(elapsed) = elapsed {
                if elapsed > entry.stale_threshold_ms {
                    entry.status = HealthStatus::Stale;
                    errors.push(StreamError::StaleFeed {
                        feed_id: entry.feed_id.clone(),
                        elapsed_ms: elapsed,
                        threshold_ms: entry.stale_threshold_ms,
                    });
                }
            }
        }
        errors
    }

    /// Get health state for a specific feed.
    pub fn get(&self, feed_id: &str) -> Option<FeedHealth> {
        self.feeds.get(feed_id).map(|e| e.clone())
    }

    /// All registered feeds.
    pub fn all_feeds(&self) -> Vec<FeedHealth> {
        self.feeds.iter().map(|e| e.clone()).collect()
    }

    /// Number of registered feeds.
    pub fn feed_count(&self) -> usize { self.feeds.len() }

    /// Count of feeds by status.
    pub fn healthy_count(&self) -> usize {
        self.feeds.iter().filter(|e| e.status == HealthStatus::Healthy).count()
    }

    pub fn stale_count(&self) -> usize {
        self.feeds.iter().filter(|e| e.status == HealthStatus::Stale).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> HealthMonitor { HealthMonitor::new(5_000) }

    #[test]
    fn test_register_creates_unknown_feed() {
        let m = monitor();
        m.register("BTC-USD", None);
        let h = m.get("BTC-USD").unwrap();
        assert_eq!(h.status, HealthStatus::Unknown);
        assert!(h.last_tick_ms.is_none());
    }

    #[test]
    fn test_heartbeat_marks_feed_healthy() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let h = m.get("BTC-USD").unwrap();
        assert_eq!(h.status, HealthStatus::Healthy);
        assert_eq!(h.last_tick_ms, Some(1_000_000));
    }

    #[test]
    fn test_heartbeat_increments_tick_count() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1000).unwrap();
        m.heartbeat("BTC-USD", 2000).unwrap();
        m.heartbeat("BTC-USD", 3000).unwrap();
        assert_eq!(m.get("BTC-USD").unwrap().tick_count, 3);
    }

    #[test]
    fn test_heartbeat_unknown_feed_returns_error() {
        let m = monitor();
        let result = m.heartbeat("ghost", 1000);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_all_healthy_feed_no_errors() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let errors = m.check_all(1_003_000); // 3s elapsed, threshold 5s
        assert!(errors.is_empty());
    }

    #[test]
    fn test_check_all_stale_feed_returns_error() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let errors = m.check_all(1_010_000); // 10s elapsed, threshold 5s
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], StreamError::StaleFeed { feed_id, .. } if feed_id == "BTC-USD"));
    }

    #[test]
    fn test_check_all_marks_stale_in_state() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000);
        assert_eq!(m.get("BTC-USD").unwrap().status, HealthStatus::Stale);
    }

    #[test]
    fn test_check_all_unknown_feed_not_counted_as_stale() {
        let m = monitor();
        m.register("BTC-USD", None);
        // No heartbeat yet
        let errors = m.check_all(9_999_999);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_custom_threshold_per_feed() {
        let m = monitor();
        m.register("BTC-USD", Some(1_000)); // 1 second threshold
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let errors = m.check_all(1_002_000); // 2s elapsed, threshold 1s
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_feed_count() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        assert_eq!(m.feed_count(), 2);
    }

    #[test]
    fn test_healthy_count_and_stale_count() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.heartbeat("ETH-USD", 1_000_000).unwrap();
        m.check_all(1_010_000); // stales both
        assert_eq!(m.stale_count(), 2);
        assert_eq!(m.healthy_count(), 0);
    }

    #[test]
    fn test_feed_health_elapsed_ms() {
        let h = FeedHealth {
            feed_id: "BTC-USD".into(),
            status: HealthStatus::Healthy,
            last_tick_ms: Some(1_000_000),
            stale_threshold_ms: 5_000,
            tick_count: 1,
        };
        assert_eq!(h.elapsed_ms(1_003_000), Some(3_000));
    }

    #[test]
    fn test_feed_health_elapsed_ms_none_when_no_last_tick() {
        let h = FeedHealth {
            feed_id: "X".into(),
            status: HealthStatus::Unknown,
            last_tick_ms: None,
            stale_threshold_ms: 5_000,
            tick_count: 0,
        };
        assert!(h.elapsed_ms(9_999_999).is_none());
    }

    #[test]
    fn test_all_feeds_returns_all() {
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        let feeds = m.all_feeds();
        assert_eq!(feeds.len(), 2);
    }
}
