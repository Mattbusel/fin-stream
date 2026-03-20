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

/// Health status of a single feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HealthStatus {
    /// Feed is active and within staleness threshold.
    Healthy,
    /// Feed has not produced data within the staleness threshold.
    Stale,
    /// Feed is newly registered and no data has been received yet.
    Unknown,
}

/// Per-feed health state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedHealth {
    /// Identifier of this feed (e.g. `"BTC-USD"`).
    pub feed_id: String,
    /// Current health status of this feed.
    pub status: HealthStatus,
    /// Timestamp (ms since Unix epoch) of the most recent tick, if any.
    pub last_tick_ms: Option<u64>,
    /// Staleness threshold in milliseconds for this feed.
    pub stale_threshold_ms: u64,
    /// Total number of heartbeats received since registration.
    pub tick_count: u64,
    /// Number of consecutive stale checks. Resets to 0 on heartbeat.
    pub consecutive_stale: u32,
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
    /// Number of consecutive stale checks before the circuit opens. 0 = disabled.
    circuit_breaker_threshold: u32,
}

impl HealthMonitor {
    /// Create a monitor with a default staleness threshold (milliseconds).
    ///
    /// Individual feeds can override this with a custom threshold via
    /// [`HealthMonitor::register`].
    pub fn new(default_stale_threshold_ms: u64) -> Self {
        Self {
            feeds: Arc::new(DashMap::new()),
            default_stale_threshold_ms,
            circuit_breaker_threshold: 3,
        }
    }

    /// Configure how many consecutive stale checks open the circuit (default: 3).
    /// Set to 0 to disable circuit-breaking.
    pub fn with_circuit_breaker_threshold(mut self, threshold: u32) -> Self {
        self.circuit_breaker_threshold = threshold;
        self
    }

    /// Returns true if the feed's circuit is open (too many consecutive stale checks).
    /// An open circuit means callers should stop routing work to this feed.
    pub fn is_circuit_open(&self, feed_id: &str) -> bool {
        if self.circuit_breaker_threshold == 0 {
            return false;
        }
        self.feeds
            .get(feed_id)
            .map(|e| e.consecutive_stale >= self.circuit_breaker_threshold)
            .unwrap_or(false)
    }

    /// Register a feed with optional custom staleness threshold.
    pub fn register(&self, feed_id: impl Into<String>, stale_threshold_ms: Option<u64>) {
        let id = feed_id.into();
        let threshold = stale_threshold_ms.unwrap_or(self.default_stale_threshold_ms);
        self.feeds.insert(
            id.clone(),
            FeedHealth {
                feed_id: id,
                status: HealthStatus::Unknown,
                last_tick_ms: None,
                stale_threshold_ms: threshold,
                tick_count: 0,
                consecutive_stale: 0,
            },
        );
    }

    /// Remove a previously registered feed from the monitor.
    ///
    /// Returns the last known [`FeedHealth`] for the feed, or `None` if it
    /// was not registered.
    pub fn deregister(&self, feed_id: &str) -> Option<FeedHealth> {
        self.feeds.remove(feed_id).map(|(_, v)| v)
    }

    /// Record a tick heartbeat for a feed.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::UnknownFeed`] if `feed_id` has not been
    /// registered via [`register`](Self::register).
    pub fn heartbeat(&self, feed_id: &str, ts_ms: u64) -> Result<(), StreamError> {
        let mut entry = self
            .feeds
            .get_mut(feed_id)
            .ok_or_else(|| StreamError::UnknownFeed {
                feed_id: feed_id.to_string(),
            })?;
        entry.last_tick_ms = Some(ts_ms);
        entry.tick_count += 1;
        entry.status = HealthStatus::Healthy;
        entry.consecutive_stale = 0;
        Ok(())
    }

    /// Check all feeds for staleness at the given timestamp.
    ///
    /// Returns a list of `(feed_id, error)` pairs for stale feeds so callers
    /// can route errors to the appropriate handler without re-parsing the feed
    /// identifier out of the error message.
    pub fn check_all(&self, now_ms: u64) -> Vec<(String, StreamError)> {
        let mut errors = Vec::new();
        for mut entry in self.feeds.iter_mut() {
            let elapsed = entry.last_tick_ms.map(|t| now_ms.saturating_sub(t));
            if let Some(elapsed) = elapsed {
                if elapsed > entry.stale_threshold_ms {
                    entry.status = HealthStatus::Stale;
                    entry.consecutive_stale += 1;
                    let feed_id = entry.feed_id.clone();
                    errors.push((
                        feed_id.clone(),
                        StreamError::StaleFeed {
                            feed_id,
                            elapsed_ms: elapsed,
                            threshold_ms: entry.stale_threshold_ms,
                        },
                    ));
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

    /// Total number of registered feeds.
    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    /// Check staleness for a single feed at `now_ms`.
    ///
    /// Returns `Some(StreamError::StaleFeed { .. })` if the feed is stale,
    /// `None` if it is healthy or has not yet received any ticks, or
    /// `Err(StreamError::UnknownFeed)` if `feed_id` is not registered.
    pub fn check_one(
        &self,
        feed_id: &str,
        now_ms: u64,
    ) -> Result<Option<StreamError>, StreamError> {
        let mut entry = self
            .feeds
            .get_mut(feed_id)
            .ok_or_else(|| StreamError::UnknownFeed {
                feed_id: feed_id.to_string(),
            })?;
        let elapsed = match entry.last_tick_ms {
            Some(t) => now_ms.saturating_sub(t),
            None => return Ok(None),
        };
        if elapsed > entry.stale_threshold_ms {
            entry.status = HealthStatus::Stale;
            entry.consecutive_stale += 1;
            Ok(Some(StreamError::StaleFeed {
                feed_id: entry.feed_id.clone(),
                elapsed_ms: elapsed,
                threshold_ms: entry.stale_threshold_ms,
            }))
        } else {
            Ok(None)
        }
    }

    /// Reset a feed's stale counter and status without deregistering it.
    ///
    /// Clears `consecutive_stale`, `tick_count`, `last_tick_ms`, and sets
    /// the status back to `Unknown`. Useful when a feed reconnects and should
    /// be treated as fresh.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::UnknownFeed`] if `feed_id` is not registered.
    pub fn reset_feed(&self, feed_id: &str) -> Result<(), StreamError> {
        let mut entry = self
            .feeds
            .get_mut(feed_id)
            .ok_or_else(|| StreamError::UnknownFeed {
                feed_id: feed_id.to_string(),
            })?;
        entry.status = HealthStatus::Unknown;
        entry.last_tick_ms = None;
        entry.tick_count = 0;
        entry.consecutive_stale = 0;
        Ok(())
    }

    /// Sorted list of all registered feed identifiers.
    pub fn feed_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.feeds.iter().map(|e| e.feed_id.clone()).collect();
        ids.sort();
        ids
    }

    /// Number of feeds currently in the [`HealthStatus::Healthy`] state.
    pub fn healthy_count(&self) -> usize {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Healthy)
            .count()
    }

    /// Number of feeds currently in the [`HealthStatus::Stale`] state.
    pub fn stale_count(&self) -> usize {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Stale)
            .count()
    }

    /// The oldest `last_tick_ms` across all registered feeds, or `None` if no
    /// feed has received any tick yet.
    pub fn oldest_tick_ms(&self) -> Option<u64> {
        self.feeds
            .iter()
            .filter_map(|e| e.last_tick_ms)
            .min()
    }

    /// The most recent `last_tick_ms` across all registered feeds, or `None`
    /// if no feed has received any tick yet.
    pub fn newest_tick_ms(&self) -> Option<u64> {
        self.feeds
            .iter()
            .filter_map(|e| e.last_tick_ms)
            .max()
    }

    /// Feed skew: `newest_tick_ms - oldest_tick_ms` across all registered feeds.
    ///
    /// Returns `None` if fewer than two feeds have received ticks. A large lag
    /// indicates that some feeds are receiving data faster than others — useful
    /// for detecting slow feeds or feed-specific latency spikes.
    pub fn lag_ms(&self) -> Option<u64> {
        let newest = self.newest_tick_ms()?;
        let oldest = self.oldest_tick_ms()?;
        Some(newest.saturating_sub(oldest))
    }

    /// Feed identifiers whose status is not [`HealthStatus::Healthy`].
    ///
    /// Returns a sorted list of IDs that are `Stale` or `Unknown`. Complement
    /// to [`healthy_count`](Self::healthy_count); avoids caller iteration when
    /// only the unhealthy feed names are needed.
    pub fn unhealthy_feeds(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .feeds
            .iter()
            .filter(|e| e.status != HealthStatus::Healthy)
            .map(|e| e.feed_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Reset all registered feeds to `Unknown` status, clearing last-tick timestamps.
    ///
    /// Useful at session boundaries (e.g. daily market open) to start fresh staleness
    /// tracking without re-registering feeds. Tick counts are preserved.
    pub fn reset_all(&self) {
        for mut entry in self.feeds.iter_mut() {
            entry.status = HealthStatus::Unknown;
            entry.last_tick_ms = None;
            entry.consecutive_stale = 0;
        }
    }

    /// Returns `true` if every registered feed is in [`HealthStatus::Healthy`] state.
    ///
    /// Vacuously `true` when no feeds are registered. Use [`feed_count`](Self::feed_count)
    /// to distinguish "all healthy" from "no feeds registered".
    pub fn all_healthy(&self) -> bool {
        self.feeds.iter().all(|e| e.status == HealthStatus::Healthy)
    }

    /// Feed identifiers whose status is exactly [`HealthStatus::Stale`].
    ///
    /// Unlike [`unhealthy_feeds`](Self::unhealthy_feeds), feeds with
    /// [`HealthStatus::Unknown`] are excluded. Returns a sorted list.
    pub fn stale_feed_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Stale)
            .map(|e| e.feed_id.clone())
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> HealthMonitor {
        HealthMonitor::new(5_000)
    }

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
    fn test_heartbeat_unknown_feed_returns_unknown_feed_error() {
        let m = monitor();
        let result = m.heartbeat("ghost", 1000);
        assert!(matches!(result, Err(StreamError::UnknownFeed { .. })));
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
        assert_eq!(errors[0].0, "BTC-USD");
        assert!(matches!(&errors[0].1, StreamError::StaleFeed { feed_id, .. } if feed_id == "BTC-USD"));
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
            consecutive_stale: 0,
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
            consecutive_stale: 0,
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

    #[test]
    fn test_circuit_not_open_before_threshold() {
        let m = monitor(); // threshold = 3
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000); // 1 stale
        m.check_all(1_020_000); // 2 stale
        assert!(!m.is_circuit_open("BTC-USD"));
    }

    #[test]
    fn test_circuit_opens_at_threshold() {
        let m = monitor(); // threshold = 3
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000); // 1
        m.check_all(1_020_000); // 2
        m.check_all(1_030_000); // 3 — circuit opens
        assert!(m.is_circuit_open("BTC-USD"));
    }

    #[test]
    fn test_circuit_resets_on_heartbeat() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000);
        m.check_all(1_020_000);
        m.check_all(1_030_000);
        assert!(m.is_circuit_open("BTC-USD"));
        m.heartbeat("BTC-USD", 1_040_000).unwrap();
        assert!(!m.is_circuit_open("BTC-USD"));
    }

    #[test]
    fn test_circuit_disabled_when_threshold_zero() {
        let m = HealthMonitor::new(5_000).with_circuit_breaker_threshold(0);
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        for i in 0..10 {
            m.check_all(1_010_000 + i * 10_000);
        }
        assert!(!m.is_circuit_open("BTC-USD"));
    }

    #[test]
    fn test_circuit_open_returns_false_for_unknown_feed() {
        let m = monitor();
        assert!(!m.is_circuit_open("ghost"));
    }

    #[test]
    fn test_deregister_removes_feed() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let removed = m.deregister("BTC-USD");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().feed_id, "BTC-USD");
        assert!(m.get("BTC-USD").is_none());
        assert_eq!(m.feed_count(), 0);
    }

    #[test]
    fn test_deregister_unknown_feed_returns_none() {
        let m = monitor();
        assert!(m.deregister("ghost").is_none());
    }

    #[test]
    fn test_feed_ids_returns_sorted_ids() {
        let m = monitor();
        m.register("ETH-USD", None);
        m.register("BTC-USD", None);
        m.register("SOL-USD", None);
        let ids = m.feed_ids();
        assert_eq!(ids, vec!["BTC-USD", "ETH-USD", "SOL-USD"]);
    }

    #[test]
    fn test_feed_ids_empty_when_no_feeds() {
        let m = monitor();
        assert!(m.feed_ids().is_empty());
    }

    #[test]
    fn test_feed_ids_updates_after_deregister() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        m.deregister("BTC-USD");
        assert_eq!(m.feed_ids(), vec!["ETH-USD"]);
    }

    #[test]
    fn test_reset_feed_clears_state() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000); // make it stale
        assert_eq!(m.get("BTC-USD").unwrap().status, HealthStatus::Stale);
        m.reset_feed("BTC-USD").unwrap();
        let h = m.get("BTC-USD").unwrap();
        assert_eq!(h.status, HealthStatus::Unknown);
        assert_eq!(h.tick_count, 0);
        assert_eq!(h.consecutive_stale, 0);
        assert!(h.last_tick_ms.is_none());
    }

    #[test]
    fn test_reset_feed_unknown_returns_error() {
        let m = monitor();
        assert!(matches!(
            m.reset_feed("ghost"),
            Err(StreamError::UnknownFeed { .. })
        ));
    }

    #[test]
    fn test_check_one_healthy_feed_returns_none() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        assert!(m.check_one("BTC-USD", 1_003_000).unwrap().is_none());
    }

    #[test]
    fn test_check_one_stale_feed_returns_some_error() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let result = m.check_one("BTC-USD", 1_010_000).unwrap();
        assert!(matches!(result, Some(StreamError::StaleFeed { .. })));
    }

    #[test]
    fn test_check_one_unknown_feed_returns_err() {
        let m = monitor();
        assert!(matches!(
            m.check_one("ghost", 0),
            Err(StreamError::UnknownFeed { .. })
        ));
    }

    #[test]
    fn test_heartbeat_after_deregister_returns_unknown_feed_error() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.deregister("BTC-USD");
        let result = m.heartbeat("BTC-USD", 1_000_000);
        assert!(matches!(result, Err(StreamError::UnknownFeed { .. })));
    }

    #[test]
    fn test_unhealthy_feeds_returns_non_healthy_sorted() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        m.register("SOL-USD", None);
        // Make BTC healthy, leave ETH and SOL as Unknown
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        let unhealthy = m.unhealthy_feeds();
        // ETH and SOL are Unknown (not Healthy)
        assert!(unhealthy.contains(&"ETH-USD".to_string()));
        assert!(unhealthy.contains(&"SOL-USD".to_string()));
        assert!(!unhealthy.contains(&"BTC-USD".to_string()));
        // Verify sorted
        assert_eq!(unhealthy[0], "ETH-USD");
        assert_eq!(unhealthy[1], "SOL-USD");
    }

    #[test]
    fn test_oldest_tick_ms_returns_minimum() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        m.heartbeat("B", 2_000_000).unwrap();
        assert_eq!(m.oldest_tick_ms(), Some(1_000_000));
    }

    #[test]
    fn test_newest_tick_ms_returns_maximum() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        m.heartbeat("B", 2_000_000).unwrap();
        assert_eq!(m.newest_tick_ms(), Some(2_000_000));
    }

    #[test]
    fn test_oldest_newest_tick_ms_none_when_no_ticks() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        assert!(m.oldest_tick_ms().is_none());
        assert!(m.newest_tick_ms().is_none());
    }

    #[test]
    fn test_unhealthy_feeds_empty_when_all_healthy() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        assert!(m.unhealthy_feeds().is_empty());
    }

    #[test]
    fn test_unhealthy_feeds_includes_stale_feeds() {
        let m = HealthMonitor::new(1_000); // 1s threshold
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_002_000); // 2s elapsed → stale
        let unhealthy = m.unhealthy_feeds();
        assert!(unhealthy.contains(&"BTC-USD".to_string()));
    }

    // ── all_healthy ───────────────────────────────────────────────────────────

    #[test]
    fn test_all_healthy_vacuously_true_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!(m.all_healthy());
    }

    #[test]
    fn test_all_healthy_false_when_unknown() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        // No heartbeat → Unknown → not Healthy
        assert!(!m.all_healthy());
    }

    #[test]
    fn test_all_healthy_true_after_heartbeats() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.heartbeat("ETH-USD", 1_000_000).unwrap();
        assert!(m.all_healthy());
    }

    #[test]
    fn test_all_healthy_false_when_one_stale() {
        let m = HealthMonitor::new(1_000);
        m.register("BTC-USD", None);
        m.register("ETH-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.heartbeat("ETH-USD", 1_000_000).unwrap();
        m.check_all(1_002_000); // 2s elapsed → stale
        assert!(!m.all_healthy());
    }

    // ── stale_feed_ids ────────────────────────────────────────────────────────

    #[test]
    fn test_stale_feed_ids_empty_when_none_stale() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        assert!(m.stale_feed_ids().is_empty());
    }

    #[test]
    fn test_stale_feed_ids_excludes_unknown() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None); // Unknown — never received heartbeat
        assert!(m.stale_feed_ids().is_empty()); // Unknown is not Stale
    }

    #[test]
    fn test_stale_feed_ids_returns_only_stale() {
        let m = HealthMonitor::new(1_000);
        m.register("BTC-USD", None);
        m.register("ETH-USD", Some(10_000)); // 10s threshold
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.heartbeat("ETH-USD", 1_000_000).unwrap();
        m.check_all(1_002_000); // BTC stale (2s > 1s), ETH healthy (2s < 10s)
        let stale = m.stale_feed_ids();
        assert_eq!(stale, vec!["BTC-USD".to_string()]);
    }

    // ── HealthMonitor::lag_ms ─────────────────────────────────────────────────

    #[test]
    fn test_lag_ms_none_when_no_ticks() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        assert!(m.lag_ms().is_none());
    }

    #[test]
    fn test_lag_ms_zero_when_one_feed_or_equal_timestamps() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        m.heartbeat("B", 1_000_000).unwrap();
        assert_eq!(m.lag_ms(), Some(0));
    }

    #[test]
    fn test_lag_ms_returns_spread() {
        let m = HealthMonitor::new(5_000);
        m.register("fast", None);
        m.register("slow", None);
        m.heartbeat("fast", 2_000_000).unwrap();
        m.heartbeat("slow", 1_000_000).unwrap();
        assert_eq!(m.lag_ms(), Some(1_000_000));
    }
}
