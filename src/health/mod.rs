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

    /// Returns `true` if this feed's status is [`HealthStatus::Healthy`].
    pub fn is_healthy(&self) -> bool {
        self.status == HealthStatus::Healthy
    }

    /// Returns `true` if this feed's status is [`HealthStatus::Stale`].
    pub fn is_stale(&self) -> bool {
        self.status == HealthStatus::Stale
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

    /// Register multiple feeds in one call.
    ///
    /// Each feed in `ids` is registered with the same optional staleness
    /// threshold. Equivalent to calling [`register`](Self::register) for
    /// each ID individually.
    pub fn register_many(&self, ids: &[&str], stale_threshold_ms: Option<u64>) {
        for id in ids {
            self.register(*id, stale_threshold_ms);
        }
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

    /// Fraction of registered feeds that are currently stale, in `[0.0, 1.0]`.
    ///
    /// Returns `0.0` when no feeds are registered.
    pub fn stale_ratio(&self) -> f64 {
        let total = self.feeds.len();
        if total == 0 {
            return 0.0;
        }
        self.stale_count() as f64 / total as f64
    }

    /// Number of feeds currently in the [`HealthStatus::Stale`] state.
    pub fn stale_count(&self) -> usize {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Stale)
            .count()
    }

    /// Clone of all feeds currently in the [`HealthStatus::Stale`] state.
    ///
    /// Useful for bulk alerting: iterate the returned vec to log or notify on
    /// every stale feed in one call rather than checking each feed individually.
    pub fn stale_feeds(&self) -> Vec<FeedHealth> {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Stale)
            .map(|e| e.clone())
            .collect()
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

    /// Sum of tick counts across all registered feeds.
    ///
    /// Useful for throughput monitoring: the total number of heartbeats seen
    /// since the monitor was created (counts are not reset by
    /// [`reset_all`](Self::reset_all)).
    pub fn total_tick_count(&self) -> u64 {
        self.feeds.iter().map(|e| e.tick_count).sum()
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

    /// The feed that has gone the longest without receiving a tick.
    ///
    /// Among all registered feeds, returns the one with the oldest (smallest)
    /// `last_tick_ms`. Feeds that have never received a tick (`last_tick_ms ==
    /// `None`) are considered more stale than any feed that has. Returns `None`
    /// if no feeds are registered.
    pub fn most_stale_feed(&self) -> Option<FeedHealth> {
        self.feeds.iter().fold(None, |acc: Option<FeedHealth>, entry| {
            let feed = entry.clone();
            match acc {
                None => Some(feed),
                Some(current) => {
                    let more_stale = match (feed.last_tick_ms, current.last_tick_ms) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(a), Some(b)) => a < b,
                    };
                    if more_stale { Some(feed) } else { Some(current) }
                }
            }
        })
    }

    /// Fraction of known (non-unknown) feeds that are stale.
    ///
    /// Excludes feeds in `Unknown` state from the denominator.
    /// Returns `0.0` when no non-unknown feeds are registered.
    pub fn stale_ratio_excluding_unknown(&self) -> f64 {
        let known: Vec<_> = self.feeds.iter()
            .filter(|e| e.status != HealthStatus::Unknown)
            .collect();
        if known.is_empty() { return 0.0; }
        let stale = known.iter().filter(|e| e.status == HealthStatus::Stale).count();
        stale as f64 / known.len() as f64
    }

    /// Feed identifiers that are currently in [`HealthStatus::Healthy`] state.
    ///
    /// Returns a sorted list of IDs. Complement of
    /// [`unhealthy_feeds`](Self::unhealthy_feeds).
    pub fn healthy_feeds(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Healthy)
            .map(|e| e.feed_id.clone())
            .collect();
        ids.sort();
        ids
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

    /// Count of feeds in each health state: `(healthy, stale, unknown)`.
    ///
    /// Equivalent to calling [`healthy_count`](Self::healthy_count),
    /// [`stale_count`](Self::stale_count), and [`unknown_count`](Self::unknown_count)
    /// in one pass.
    pub fn status_summary(&self) -> (usize, usize, usize) {
        let (mut healthy, mut stale, mut unknown) = (0, 0, 0);
        for e in self.feeds.iter() {
            match e.status {
                HealthStatus::Healthy => healthy += 1,
                HealthStatus::Stale => stale += 1,
                HealthStatus::Unknown => unknown += 1,
            }
        }
        (healthy, stale, unknown)
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

    /// Count of feeds currently in `Stale` status.
    pub fn total_stale_count(&self) -> usize {
        self.feeds.iter().filter(|e| e.status == HealthStatus::Stale).count()
    }

    /// Average age in milliseconds across all feeds that have received at least one tick.
    ///
    /// Returns `None` if no feed has ever received a tick.
    pub fn avg_feed_age_ms(&self, now_ms: u64) -> Option<f64> {
        let ages: Vec<u64> = self
            .feeds
            .iter()
            .filter_map(|e| e.last_tick_ms)
            .map(|t| now_ms.saturating_sub(t))
            .collect();
        if ages.is_empty() {
            return None;
        }
        Some(ages.iter().sum::<u64>() as f64 / ages.len() as f64)
    }

    /// Number of feeds whose status is [`HealthStatus::Unknown`].
    ///
    /// Feeds start in `Unknown` state before the first heartbeat arrives.
    /// A non-zero count indicates feeds that have never been heard from.
    pub fn unknown_count(&self) -> usize {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Unknown)
            .count()
    }

    /// Average tick count per registered feed.
    ///
    /// Returns `0.0` if no feeds are registered.
    pub fn avg_tick_count(&self) -> f64 {
        let count = self.feeds.len();
        if count == 0 {
            return 0.0;
        }
        self.total_tick_count() as f64 / count as f64
    }

    /// Maximum `consecutive_stale` count across all registered feeds.
    ///
    /// Returns `0` if no feeds are registered or none have been stale yet.
    pub fn max_consecutive_stale(&self) -> u32 {
        self.feeds
            .iter()
            .map(|e| e.consecutive_stale)
            .max()
            .unwrap_or(0)
    }

    /// Number of feeds currently in the [`HealthStatus::Unknown`] state.
    pub fn unknown_feed_count(&self) -> usize {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Unknown)
            .count()
    }

    /// All feeds whose current status exactly matches `status`.
    ///
    /// Returns a `Vec` of cloned [`FeedHealth`] entries. Order is unspecified.
    /// Use [`stale_feeds`](Self::stale_feeds) as a shorthand for the common
    /// `Stale` case.
    pub fn feeds_by_status(&self, status: HealthStatus) -> Vec<FeedHealth> {
        self.feeds
            .iter()
            .filter(|e| e.status == status)
            .map(|e| e.clone())
            .collect()
    }

    /// The stale feed with the oldest `last_tick_ms`, or `None` if no feeds are stale.
    ///
    /// "Oldest" means the feed that has gone the longest without a heartbeat
    /// — i.e., the one with the smallest `last_tick_ms`. Feeds with
    /// `last_tick_ms == None` are placed last (they have never ticked).
    pub fn oldest_stale_feed(&self) -> Option<FeedHealth> {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Stale)
            .min_by_key(|e| e.last_tick_ms.unwrap_or(u64::MAX))
            .map(|e| e.clone())
    }

    /// Fraction of registered feeds that are currently `Healthy`: `healthy / total`.
    ///
    /// Returns `0.0` when no feeds are registered.
    pub fn healthy_ratio(&self) -> f64 {
        let total = self.feeds.len();
        if total == 0 {
            return 0.0;
        }
        self.healthy_count() as f64 / total as f64
    }

    /// The feed with the highest lifetime tick count, or `None` if no feeds
    /// are registered.
    ///
    /// "Most reliable" is defined as the feed that has processed the greatest
    /// number of heartbeats since registration or last reset.
    pub fn most_reliable_feed(&self) -> Option<FeedHealth> {
        self.feeds.iter()
            .max_by_key(|e| e.tick_count)
            .map(|e| e.clone())
    }

    /// All feeds that have never received a heartbeat (`last_tick_ms` is
    /// `None`).
    ///
    /// Useful for detecting feeds that were registered but have not yet
    /// started streaming data.
    pub fn feeds_never_seen(&self) -> Vec<FeedHealth> {
        self.feeds.iter()
            .filter(|e| e.last_tick_ms.is_none())
            .map(|e| e.clone())
            .collect()
    }

    /// Returns `true` if at least one registered feed currently has
    /// [`HealthStatus::Stale`] status.
    pub fn is_any_feed_stale(&self) -> bool {
        self.feeds.iter().any(|e| e.status == HealthStatus::Stale)
    }

    /// Returns `true` if every registered feed has received at least one
    /// heartbeat (`last_tick_ms` is `Some`).
    ///
    /// Returns `true` vacuously when no feeds are registered.
    pub fn all_feeds_seen(&self) -> bool {
        self.feeds.iter().all(|e| e.last_tick_ms.is_some())
    }

    /// Returns the `tick_count` for `feed_id`, or `None` if the feed is not
    /// registered.
    pub fn tick_count_for(&self, feed_id: &str) -> Option<u64> {
        self.feeds.iter()
            .find(|e| e.feed_id == feed_id)
            .map(|e| e.tick_count)
    }

    /// Average tick count across all registered feeds.
    ///
    /// Returns `0.0` when no feeds are registered.
    pub fn average_tick_count(&self) -> f64 {
        let total = self.feeds.len();
        if total == 0 {
            return 0.0;
        }
        self.total_tick_count() as f64 / total as f64
    }

    /// Count of feeds whose `tick_count` exceeds `threshold`.
    pub fn feeds_above_tick_count(&self, threshold: u64) -> usize {
        self.feeds.iter().filter(|e| e.tick_count > threshold).count()
    }

    /// Age in milliseconds of the feed with the oldest `last_tick_ms`
    /// (the most stale one) relative to `now_ms`.
    ///
    /// Returns `None` if no feed has ever received a tick.
    pub fn oldest_feed_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.feeds
            .iter()
            .filter_map(|e| e.last_tick_ms)
            .map(|t| now_ms.saturating_sub(t))
            .max()
    }

    /// Returns `true` if at least one feed currently has
    /// [`HealthStatus::Unknown`] status.
    pub fn has_any_unknown(&self) -> bool {
        self.feeds.iter().any(|e| e.status == HealthStatus::Unknown)
    }

    /// Returns `true` if at least one feed is unhealthy but not all feeds
    /// are unhealthy.
    ///
    /// A fully-healthy or fully-down monitor both return `false`.
    /// Returns `false` when no feeds are registered.
    pub fn is_degraded(&self) -> bool {
        let total = self.feeds.len();
        if total == 0 {
            return false;
        }
        let healthy = self.healthy_count();
        healthy > 0 && healthy < total
    }

    /// Number of feeds that are not in the [`HealthStatus::Healthy`] state.
    pub fn unhealthy_count(&self) -> usize {
        self.feeds.len().saturating_sub(self.healthy_count())
    }

    /// Returns `true` if a feed with the given ID is registered.
    pub fn feed_exists(&self, feed_id: &str) -> bool {
        self.feeds.iter().any(|e| e.feed_id == feed_id)
    }

    /// Returns `true` if any registered feed has [`HealthStatus::Unknown`] status.
    pub fn any_unknown(&self) -> bool {
        self.feeds.iter().any(|e| e.status == HealthStatus::Unknown)
    }

    /// Count of feeds in [`HealthStatus::Stale`] state (degraded but not unknown).
    ///
    /// Degraded = stale. Excludes healthy and unknown feeds.
    pub fn degraded_count(&self) -> usize {
        self.feeds.iter().filter(|e| e.status == HealthStatus::Stale).count()
    }

    /// Milliseconds since the last heartbeat for `feed_id` at `now_ms`.
    ///
    /// Returns `None` if the feed is not registered or has never received a tick.
    pub fn time_since_last_heartbeat(&self, feed_id: &str, now_ms: u64) -> Option<u64> {
        self.feeds
            .iter()
            .find(|e| e.feed_id == feed_id)?
            .last_tick_ms
            .map(|t| now_ms.saturating_sub(t))
    }

    /// IDs of all feeds currently in [`HealthStatus::Healthy`] state.
    pub fn healthy_feed_ids(&self) -> Vec<String> {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Healthy)
            .map(|e| e.feed_id.clone())
            .collect()
    }

    /// Age in milliseconds of the most recently-ticked healthy feed at `now_ms`.
    ///
    /// Returns `None` if no healthy feeds have received a tick.
    pub fn min_healthy_age_ms(&self, now_ms: u64) -> Option<u64> {
        self.feeds
            .iter()
            .filter(|e| e.status == HealthStatus::Healthy)
            .filter_map(|e| e.last_tick_ms)
            .map(|t| now_ms.saturating_sub(t))
            .min()
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

    // ── HealthMonitor::total_tick_count ───────────────────────────────────────

    #[test]
    fn test_total_tick_count_zero_initially() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        assert_eq!(m.total_tick_count(), 0);
    }

    #[test]
    fn test_total_tick_count_sums_across_feeds() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        m.heartbeat("A", 1_001_000).unwrap();
        m.heartbeat("B", 1_000_000).unwrap();
        assert_eq!(m.total_tick_count(), 3);
    }

    // ── HealthMonitor::healthy_feeds ──────────────────────────────────────────

    #[test]
    fn test_healthy_feeds_empty_initially() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None); // Unknown, not Healthy
        assert!(m.healthy_feeds().is_empty());
    }

    #[test]
    fn test_healthy_feeds_after_heartbeat() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        m.check_all(1_001_000); // A healthy (1ms < 5s), B unknown
        let healthy = m.healthy_feeds();
        assert_eq!(healthy, vec!["A".to_string()]);
    }

    #[test]
    fn test_healthy_feeds_sorted() {
        let m = HealthMonitor::new(5_000);
        m.register("C", None);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("C", 1_000_000).unwrap();
        m.heartbeat("A", 1_000_000).unwrap();
        m.heartbeat("B", 1_000_000).unwrap();
        m.check_all(1_001_000);
        let healthy = m.healthy_feeds();
        assert_eq!(healthy, vec!["A".to_string(), "B".to_string(), "C".to_string()]);
    }

    // ── HealthMonitor::unknown_count / FeedHealth::is_healthy ─────────────────

    #[test]
    fn test_unknown_count_all_new_feeds() {
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        assert_eq!(m.unknown_count(), 2);
    }

    #[test]
    fn test_unknown_count_decreases_after_heartbeat() {
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        assert_eq!(m.unknown_count(), 1); // B is still Unknown
    }

    #[test]
    fn test_unknown_count_zero_when_all_healthy() {
        let m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        assert_eq!(m.unknown_count(), 0);
    }

    #[test]
    fn test_feed_health_is_healthy_true_after_heartbeat() {
        let m = monitor();
        m.register("X", None);
        m.heartbeat("X", 1_000).unwrap();
        let fh = m.get("X").unwrap();
        assert!(fh.is_healthy());
    }

    #[test]
    fn test_feed_health_is_healthy_false_when_unknown() {
        let m = monitor();
        m.register("X", None);
        let fh = m.get("X").unwrap();
        assert!(!fh.is_healthy());
    }

    // ── HealthMonitor::most_stale_feed ────────────────────────────────────────

    #[test]
    fn test_most_stale_feed_none_when_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!(m.most_stale_feed().is_none());
    }

    #[test]
    fn test_most_stale_feed_returns_unticked_feed_first() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000_000).unwrap();
        // B never received a tick → most stale
        let stale = m.most_stale_feed().unwrap();
        assert_eq!(stale.feed_id, "B");
    }

    #[test]
    fn test_most_stale_feed_returns_oldest_last_tick() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 5_000).unwrap();
        // A has older last_tick_ms → most stale
        let stale = m.most_stale_feed().unwrap();
        assert_eq!(stale.feed_id, "A");
    }

    #[test]
    fn test_stale_feeds_empty_when_all_healthy() {
        let m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        assert!(m.stale_feeds().is_empty());
    }

    #[test]
    fn test_stale_feeds_returns_all_stale() {
        // stale_timeout = 5_000 ms; heartbeat A at 1_000, then check at 10_000
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 9_500).unwrap();
        m.check_all(10_000);
        let stale = m.stale_feeds();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].feed_id, "A");
    }

    // ── HealthMonitor::register_many ──────────────────────────────────────────

    #[test]
    fn test_register_many_creates_all_feeds() {
        let m = monitor();
        m.register_many(&["BTC-USD", "ETH-USD", "SOL-USD"], None);
        assert_eq!(m.feed_count(), 3);
    }

    #[test]
    fn test_register_many_custom_threshold_applies() {
        let m = monitor();
        m.register_many(&["A", "B"], Some(1_000));
        m.heartbeat("A", 1_000_000).unwrap();
        let errors = m.check_all(1_002_000); // 2s > 1s → A stale
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_register_many_empty_slice_is_noop() {
        let m = monitor();
        m.register_many(&[], None);
        assert_eq!(m.feed_count(), 0);
    }

    // ── FeedHealth::is_stale ──────────────────────────────────────────────────

    #[test]
    fn test_feed_health_is_stale_true_when_stale() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        m.check_all(1_010_000); // 10s > 5s → stale
        assert!(m.get("BTC-USD").unwrap().is_stale());
    }

    #[test]
    fn test_feed_health_is_stale_false_when_healthy() {
        let m = monitor();
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000_000).unwrap();
        assert!(!m.get("BTC-USD").unwrap().is_stale());
    }

    // ── HealthMonitor::avg_tick_count ─────────────────────────────────────────

    #[test]
    fn test_avg_tick_count_zero_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!((m.avg_tick_count() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_avg_tick_count_with_equal_ticks() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        assert!((m.avg_tick_count() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_avg_tick_count_with_different_ticks() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("A", 2_000).unwrap();
        m.heartbeat("A", 3_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        // A=3, B=1 → avg=2.0
        assert!((m.avg_tick_count() - 2.0).abs() < 1e-9);
    }

    // ── HealthMonitor::max_consecutive_stale ──────────────────────────────────

    #[test]
    fn test_max_consecutive_stale_zero_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.max_consecutive_stale(), 0);
    }

    #[test]
    fn test_max_consecutive_stale_picks_highest() {
        let m = HealthMonitor::new(1_000);
        m.register("A", None);
        m.register("B", None);
        // Give each feed a heartbeat, then let them go stale
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        // 3s later → both exceed 1s threshold
        m.check_all(4_000);
        m.check_all(5_000);
        // Both feeds are stale; consecutive_stale should be at least 2
        assert!(m.max_consecutive_stale() >= 2);
    }

    #[test]
    fn test_max_consecutive_stale_zero_after_heartbeat() {
        let m = HealthMonitor::new(1_000);
        m.register("A", None);
        m.check_all(10_000);
        m.heartbeat("A", 10_001).unwrap();
        assert_eq!(m.max_consecutive_stale(), 0);
    }

    #[test]
    fn test_status_summary_all_unknown() {
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        let (healthy, stale, unknown) = m.status_summary();
        assert_eq!(healthy, 0);
        assert_eq!(stale, 0);
        assert_eq!(unknown, 2);
    }

    #[test]
    fn test_status_summary_mixed() {
        let m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.register("C", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 5_000).unwrap();
        // A is stale (tick at 1_000, check at 10_000 > 5_000 threshold), B is healthy, C unknown
        m.check_all(10_000);
        let (healthy, stale, unknown) = m.status_summary();
        assert_eq!(stale, 1);
        assert_eq!(unknown, 1);
        assert_eq!(healthy, 1);
    }

    // --- feeds_by_status ---

    #[test]
    fn test_feeds_by_status_returns_only_matching_status() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        // A: old heartbeat → stale; B: recent heartbeat → healthy
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 9_000).unwrap();
        m.check_all(10_000); // A elapsed=9000>5000 → Stale; B elapsed=1000<5000 → Healthy
        let stale = m.feeds_by_status(HealthStatus::Stale);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].feed_id, "A");
    }

    #[test]
    fn test_feeds_by_status_empty_when_none_match() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        m.check_all(2_000);
        // A is healthy; no unknown feeds remain
        let unknown = m.feeds_by_status(HealthStatus::Unknown);
        assert!(unknown.is_empty());
    }

    #[test]
    fn test_feeds_by_status_all_start_as_unknown() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        let unknown = m.feeds_by_status(HealthStatus::Unknown);
        assert_eq!(unknown.len(), 2);
    }

    // ── HealthMonitor::unknown_feed_count ─────────────────────────────────────

    #[test]
    fn test_unknown_feed_count_all_unknown_at_start() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        assert_eq!(m.unknown_feed_count(), 2);
    }

    #[test]
    fn test_unknown_feed_count_decreases_after_heartbeat() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        assert_eq!(m.unknown_feed_count(), 1);
    }

    #[test]
    fn test_unknown_feed_count_zero_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.unknown_feed_count(), 0);
    }

    // ── HealthMonitor::all_healthy ────────────────────────────────────────────

    #[test]
    fn test_all_healthy_vacuously_true_with_no_feeds() {
        // all_healthy is vacuously true when no feeds are registered
        let m = HealthMonitor::new(5_000);
        assert!(m.all_healthy());
    }

    #[test]
    fn test_all_healthy_true_when_all_feeds_healthy() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        assert!(m.all_healthy());
    }

    #[test]
    fn test_all_healthy_false_when_one_feed_unknown() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        // B still unknown
        assert!(!m.all_healthy());
    }

    // --- oldest_stale_feed / healthy_ratio ---

    #[test]
    fn test_oldest_stale_feed_returns_feed_with_smallest_last_tick() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap(); // older tick
        m.heartbeat("B", 3_000).unwrap(); // newer tick
        m.check_all(10_000); // both stale (>5s threshold)
        let oldest = m.oldest_stale_feed().unwrap();
        assert_eq!(oldest.feed_id, "A");
    }

    #[test]
    fn test_oldest_stale_feed_none_when_no_stale_feeds() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 9_000).unwrap();
        m.check_all(10_000); // 1s elapsed < 5s threshold → healthy
        assert!(m.oldest_stale_feed().is_none());
    }

    #[test]
    fn test_healthy_ratio_zero_when_no_feeds() {
        let m = monitor();
        assert_eq!(m.healthy_ratio(), 0.0);
    }

    #[test]
    fn test_healthy_ratio_one_when_all_healthy() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 9_000).unwrap();
        m.heartbeat("B", 9_500).unwrap();
        m.check_all(10_000); // both healthy
        assert!((m.healthy_ratio() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_healthy_ratio_half_when_one_of_two_healthy() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap(); // stale
        m.heartbeat("B", 9_500).unwrap(); // healthy
        m.check_all(10_000);
        assert!((m.healthy_ratio() - 0.5).abs() < 1e-10);
    }

    // --- most_reliable_feed / feeds_never_seen ---

    #[test]
    fn test_most_reliable_feed_returns_highest_tick_count() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_100).unwrap();
        m.heartbeat("B", 1_200).unwrap();
        // B has 2 ticks, A has 1
        let best = m.most_reliable_feed().unwrap();
        assert_eq!(best.feed_id, "B");
    }

    #[test]
    fn test_most_reliable_feed_none_when_no_feeds() {
        let m = monitor();
        assert!(m.most_reliable_feed().is_none());
    }

    #[test]
    fn test_feeds_never_seen_returns_feeds_with_no_heartbeat() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        // B has no heartbeat
        let never_seen = m.feeds_never_seen();
        assert_eq!(never_seen.len(), 1);
        assert_eq!(never_seen[0].feed_id, "B");
    }

    #[test]
    fn test_feeds_never_seen_empty_when_all_have_heartbeat() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        assert!(m.feeds_never_seen().is_empty());
    }

    // --- is_any_feed_stale / all_feeds_seen ---

    #[test]
    fn test_is_any_feed_stale_true_when_stale_feed_exists() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        m.check_all(10_000); // elapsed > threshold → stale
        assert!(m.is_any_feed_stale());
    }

    #[test]
    fn test_is_any_feed_stale_false_when_all_healthy() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 9_500).unwrap();
        m.check_all(10_000); // 500ms elapsed → healthy
        assert!(!m.is_any_feed_stale());
    }

    #[test]
    fn test_is_any_feed_stale_false_when_no_feeds() {
        let m = monitor();
        assert!(!m.is_any_feed_stale());
    }

    #[test]
    fn test_all_feeds_seen_true_when_all_have_heartbeat() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 2_000).unwrap();
        assert!(m.all_feeds_seen());
    }

    #[test]
    fn test_all_feeds_seen_false_when_one_never_seen() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        assert!(!m.all_feeds_seen());
    }

    #[test]
    fn test_all_feeds_seen_true_vacuously_when_no_feeds() {
        let m = monitor();
        assert!(m.all_feeds_seen());
    }

    // --- tick_count_for / average_tick_count ---

    #[test]
    fn test_tick_count_for_returns_correct_count() {
        let mut m = monitor();
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("A", 2_000).unwrap();
        assert_eq!(m.tick_count_for("A"), Some(2));
    }

    #[test]
    fn test_tick_count_for_none_when_not_registered() {
        let m = monitor();
        assert!(m.tick_count_for("nonexistent").is_none());
    }

    #[test]
    fn test_tick_count_for_zero_when_no_heartbeats() {
        let mut m = monitor();
        m.register("A", None);
        assert_eq!(m.tick_count_for("A"), Some(0));
    }

    #[test]
    fn test_average_tick_count_zero_when_no_feeds() {
        let m = monitor();
        assert_eq!(m.average_tick_count(), 0.0);
    }

    #[test]
    fn test_average_tick_count_correct_value() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("A", 2_000).unwrap(); // A: 2 ticks
        m.heartbeat("B", 1_000).unwrap(); // B: 1 tick
        // avg = (2 + 1) / 2 = 1.5
        assert!((m.average_tick_count() - 1.5).abs() < 1e-10);
    }

    // --- HealthMonitor::feeds_above_tick_count ---
    #[test]
    fn test_feeds_above_tick_count_correct() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.register("C", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("A", 2_000).unwrap();
        m.heartbeat("A", 3_000).unwrap(); // A: 3 ticks
        m.heartbeat("B", 1_000).unwrap(); // B: 1 tick
        // threshold=1: A(3) and B(1) → only A > 1
        // Wait: "above" means > threshold
        assert_eq!(m.feeds_above_tick_count(1), 1);
        assert_eq!(m.feeds_above_tick_count(0), 2);
        assert_eq!(m.feeds_above_tick_count(5), 0);
    }

    #[test]
    fn test_feeds_above_tick_count_zero_when_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.feeds_above_tick_count(0), 0);
    }

    // --- HealthMonitor::oldest_feed_age_ms ---
    #[test]
    fn test_oldest_feed_age_ms_returns_max_age() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 5_000).unwrap();
        m.heartbeat("B", 8_000).unwrap();
        // A is older (ts=5000), B more recent (ts=8000)
        // At now=10_000: A age=5000, B age=2000 → oldest = 5000
        assert_eq!(m.oldest_feed_age_ms(10_000), Some(5_000));
    }

    #[test]
    fn test_oldest_feed_age_ms_none_when_no_ticks() {
        let mut m = monitor();
        m.register("A", None);
        assert!(m.oldest_feed_age_ms(10_000).is_none());
    }

    // --- HealthMonitor::total_stale_count ---
    #[test]
    fn test_total_stale_count_zero_when_all_healthy() {
        let mut m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.heartbeat("A", 9_500).unwrap();
        let _ = m.check_all(10_000);
        assert_eq!(m.total_stale_count(), 0);
    }

    #[test]
    fn test_total_stale_count_correct_when_stale() {
        let mut m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        // at t=10000, both stale (9s elapsed > 5s threshold)
        let _ = m.check_all(10_000);
        assert_eq!(m.total_stale_count(), 2);
    }

    // --- HealthMonitor::avg_feed_age_ms ---
    #[test]
    fn test_avg_feed_age_ms_none_when_no_ticks() {
        let mut m = monitor();
        m.register("A", None);
        assert!(m.avg_feed_age_ms(10_000).is_none());
    }

    #[test]
    fn test_avg_feed_age_ms_correct_average() {
        let mut m = monitor();
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 5_000).unwrap(); // age at t=10000: 5000
        m.heartbeat("B", 8_000).unwrap(); // age at t=10000: 2000
        // avg = (5000 + 2000) / 2 = 3500
        let avg = m.avg_feed_age_ms(10_000).unwrap();
        assert!((avg - 3500.0).abs() < 1e-10, "got {avg}");
    }

    // ── HealthMonitor::stale_ratio ────────────────────────────────────────────

    #[test]
    fn test_stale_ratio_zero_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.stale_ratio(), 0.0);
    }

    #[test]
    fn test_stale_ratio_one_when_all_stale() {
        let m = HealthMonitor::new(1_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 1_000).unwrap();
        m.check_all(5_000);
        assert!((m.stale_ratio() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_stale_ratio_half_when_one_of_two_stale() {
        let m = HealthMonitor::new(1_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        m.heartbeat("B", 9_500).unwrap();
        m.check_all(10_000);
        assert!((m.stale_ratio() - 0.5).abs() < 1e-10);
    }

    // ── HealthMonitor::has_any_unknown ────────────────────────────────────────

    #[test]
    fn test_has_any_unknown_true_for_fresh_feed() {
        let m = HealthMonitor::new(5_000);
        m.register("feed", None);
        // Newly registered feed starts as Unknown
        assert!(m.has_any_unknown());
    }

    #[test]
    fn test_has_any_unknown_false_after_heartbeat() {
        let m = HealthMonitor::new(5_000);
        m.register("feed", None);
        m.heartbeat("feed", 1_000).unwrap();
        assert!(!m.has_any_unknown());
    }

    #[test]
    fn test_has_any_unknown_false_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!(!m.has_any_unknown());
    }

    // ── HealthMonitor::is_degraded ────────────────────────────────────────────

    #[test]
    fn test_is_degraded_true_when_some_unhealthy() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 9_500).unwrap(); // recent → healthy
        m.heartbeat("B", 1_000).unwrap(); // old → stale
        m.check_all(10_000);
        // A: healthy, B: stale → is_degraded
        assert!(m.is_degraded());
    }

    #[test]
    fn test_is_degraded_false_when_all_healthy() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        assert!(!m.is_degraded());
    }

    #[test]
    fn test_is_degraded_false_with_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!(!m.is_degraded());
    }

    // ── HealthMonitor::unhealthy_count / feed_exists ────────────────────────

    #[test]
    fn test_unhealthy_count_all_unknown() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        // Both unknown → unhealthy_count = 2
        assert_eq!(m.unhealthy_count(), 2);
    }

    #[test]
    fn test_unhealthy_count_one_healthy() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap();
        // A is healthy (just received heartbeat), B still unknown
        assert_eq!(m.unhealthy_count(), 1);
    }

    #[test]
    fn test_unhealthy_count_zero_when_empty() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.unhealthy_count(), 0);
    }

    #[test]
    fn test_feed_exists_true_after_register() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        assert!(m.feed_exists("BTC-USD"));
    }

    #[test]
    fn test_feed_exists_false_for_unknown_feed() {
        let m = HealthMonitor::new(5_000);
        assert!(!m.feed_exists("ETH-USD"));
    }

    // ── HealthMonitor::most_stale_feed ─────────────────────────────────────

    #[test]
    fn test_most_stale_feed_returns_oldest_feed() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 1_000).unwrap(); // older tick
        m.heartbeat("B", 9_000).unwrap(); // newer tick
        // "A" has the oldest tick → it is the most stale
        let stale = m.most_stale_feed().unwrap();
        assert_eq!(stale.feed_id, "A");
    }

    #[test]
    fn test_most_stale_feed_some_with_single_feed() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.heartbeat("A", 1_000).unwrap();
        assert!(m.most_stale_feed().is_some());
    }

    #[test]
    fn test_most_stale_feed_none_when_empty() {
        let m = HealthMonitor::new(5_000);
        assert!(m.most_stale_feed().is_none());
    }

    // ── HealthMonitor::stale_ratio_excluding_unknown ────────────────────────

    #[test]
    fn test_stale_ratio_excl_unknown_zero_when_empty() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.stale_ratio_excluding_unknown(), 0.0);
    }

    #[test]
    fn test_stale_ratio_excl_unknown_zero_when_all_unknown() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        // Both unknown → excluded → ratio = 0.0
        assert_eq!(m.stale_ratio_excluding_unknown(), 0.0);
    }

    #[test]
    fn test_stale_ratio_excl_unknown_half_when_one_stale() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 9_500).unwrap(); // recent → healthy
        m.heartbeat("B", 1_000).unwrap(); // old → stale
        m.check_all(10_000);
        // A=Healthy, B=Stale → 1 stale of 2 known = 0.5
        assert!((m.stale_ratio_excluding_unknown() - 0.5).abs() < 1e-10);
    }

    // ── HealthMonitor::any_unknown ────────────────────────────────────────────

    #[test]
    fn test_any_unknown_false_when_empty() {
        let m = HealthMonitor::new(5_000);
        assert!(!m.any_unknown());
    }

    #[test]
    fn test_any_unknown_true_when_feed_registered_but_no_heartbeat() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        assert!(m.any_unknown());
    }

    #[test]
    fn test_any_unknown_false_when_all_have_heartbeats() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC-USD", None);
        m.heartbeat("BTC-USD", 1_000).unwrap();
        assert!(!m.any_unknown());
    }

    // ── HealthMonitor::degraded_count ─────────────────────────────────────────

    #[test]
    fn test_degraded_count_zero_when_all_healthy() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.heartbeat("A", 9_500).unwrap();
        m.check_all(10_000);
        assert_eq!(m.degraded_count(), 0);
    }

    #[test]
    fn test_degraded_count_one_when_one_stale() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 9_500).unwrap(); // healthy
        m.heartbeat("B", 1_000).unwrap(); // stale
        m.check_all(10_000);
        assert_eq!(m.degraded_count(), 1);
    }

    #[test]
    fn test_degraded_count_zero_when_empty() {
        let m = HealthMonitor::new(5_000);
        assert_eq!(m.degraded_count(), 0);
    }

    // ── HealthMonitor::min_healthy_age_ms ────────────────────────────────────

    #[test]
    fn test_min_healthy_age_ms_none_when_no_healthy_feeds() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        // No heartbeat → Unknown, not Healthy
        assert!(m.min_healthy_age_ms(10_000).is_none());
    }

    #[test]
    fn test_min_healthy_age_ms_returns_most_recent() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 8_000).unwrap(); // 2s ago at now=10000
        m.heartbeat("B", 9_000).unwrap(); // 1s ago at now=10000
        m.check_all(10_000);
        // B is more recent → min age = 1000ms
        assert_eq!(m.min_healthy_age_ms(10_000), Some(1_000));
    }

    // ── HealthMonitor::healthy_feed_ids ───────────────────────────────────────

    #[test]
    fn test_healthy_feed_ids_empty_when_no_feeds() {
        let m = HealthMonitor::new(5_000);
        assert!(m.healthy_feed_ids().is_empty());
    }

    #[test]
    fn test_healthy_feed_ids_returns_healthy_only() {
        let m = HealthMonitor::new(5_000);
        m.register("A", None);
        m.register("B", None);
        m.heartbeat("A", 9_500).unwrap(); // healthy
        // B has no heartbeat → unknown
        m.check_all(10_000);
        let ids = m.healthy_feed_ids();
        assert_eq!(ids, vec!["A".to_string()]);
    }

    // ── HealthMonitor::time_since_last_heartbeat ──────────────────────────────

    #[test]
    fn test_time_since_last_heartbeat_correct() {
        let m = HealthMonitor::new(5_000);
        m.register("BTC", None);
        m.heartbeat("BTC", 9_000).unwrap();
        assert_eq!(m.time_since_last_heartbeat("BTC", 10_000), Some(1_000));
    }

    #[test]
    fn test_time_since_last_heartbeat_none_when_no_tick() {
        let m = HealthMonitor::new(5_000);
        m.register("ETH", None);
        assert!(m.time_since_last_heartbeat("ETH", 10_000).is_none());
    }

    #[test]
    fn test_time_since_last_heartbeat_none_when_unknown_feed() {
        let m = HealthMonitor::new(5_000);
        assert!(m.time_since_last_heartbeat("MISSING", 10_000).is_none());
    }
}
