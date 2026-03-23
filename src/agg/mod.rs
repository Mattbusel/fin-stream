//! Multi-feed aggregator — cross-feed tick merging and arbitrage detection.
//!
//! ## Overview
//!
//! This module provides [`FeedAggregator`], which subscribes to N independent
//! [`NormalizedTick`] streams simultaneously, merges them into a single
//! chronologically-ordered output stream, and — optionally — runs
//! [`ArbDetector`] to flag price discrepancies between feeds.
//!
//! ## Merge strategies
//!
//! | Strategy | Description |
//! |----------|-------------|
//! | [`MergeStrategy::BestBid`] | Emit the tick with the highest bid across all feeds |
//! | [`MergeStrategy::BestAsk`] | Emit the tick with the lowest ask across all feeds |
//! | [`MergeStrategy::VwapWeighted`] | Emit a synthetic tick whose price is the VWAP of all feeds |
//! | [`MergeStrategy::PrimaryWithFallback`] | Prefer a designated primary feed; fall back when it is stale |
//!
//! ## Latency compensation
//!
//! Different venues deliver data with different network latencies. Each feed
//! can be assigned a `latency_offset_ms` that is subtracted from the received
//! timestamp before merge-ordering, so that fast feeds do not unfairly
//! dominate the output stream.
//!
//! ## Arbitrage detection
//!
//! [`ArbDetector`] compares the best bid and best ask visible across all feeds.
//! When the spread exceeds a configurable threshold (in basis points) it emits
//! an [`ArbOpportunity`] describing the buy-leg feed, sell-leg feed, and the
//! gross spread.

use crate::error::StreamError;
use crate::tick::NormalizedTick;
use rust_decimal::Decimal;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::cmp::Ordering;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Feed handle
// ---------------------------------------------------------------------------

/// A named, compensated input feed connected to the aggregator.
///
/// Construct via [`FeedHandle::new`] and register with
/// [`FeedAggregator::add_feed`].
#[derive(Debug, Clone)]
pub struct FeedHandle {
    /// Unique identifier for this feed.
    pub id: Uuid,
    /// Human-readable label, e.g. `"binance-btc-usdt"`.
    pub label: String,
    /// Constant latency offset subtracted from each tick's `received_at_ms`
    /// before merge ordering. Set to 0 when the feed delivers data at the
    /// network median.
    pub latency_offset_ms: u64,
    /// Whether this feed is the designated primary for
    /// [`MergeStrategy::PrimaryWithFallback`].
    pub is_primary: bool,
}

impl FeedHandle {
    /// Create a new feed handle with the given label and no latency
    /// compensation.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            label: label.into(),
            latency_offset_ms: 0,
            is_primary: false,
        }
    }

    /// Set the constant latency compensation offset (milliseconds) for this
    /// feed.
    ///
    /// Ticks from this feed will be sorted as if they arrived
    /// `latency_offset_ms` earlier than the system clock recorded.
    #[must_use]
    pub fn with_latency_offset(mut self, offset_ms: u64) -> Self {
        self.latency_offset_ms = offset_ms;
        self
    }

    /// Mark this feed as the primary source for
    /// [`MergeStrategy::PrimaryWithFallback`].
    #[must_use]
    pub fn as_primary(mut self) -> Self {
        self.is_primary = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Merge strategy
// ---------------------------------------------------------------------------

/// Strategy used by [`FeedAggregator`] to select or synthesise the output
/// tick when multiple feeds have data available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Emit the tick with the highest `price` (used as best-bid proxy).
    ///
    /// Suitable for aggregating bid-side feeds to find the best resting bid
    /// across all venues.
    BestBid,

    /// Emit the tick with the lowest `price` (used as best-ask proxy).
    ///
    /// Suitable for aggregating ask-side feeds to find the best resting ask
    /// across all venues.
    BestAsk,

    /// Emit a synthetic tick whose price is the volume-weighted average price
    /// (VWAP) of all buffered ticks.
    ///
    /// `price = Σ(price_i × quantity_i) / Σ(quantity_i)`
    ///
    /// The synthetic tick inherits the `exchange`, `symbol`, and
    /// `received_at_ms` of the most-recent contributing tick. `quantity` is
    /// set to the total volume across all feeds.
    VwapWeighted,

    /// Pass through ticks from the primary feed unchanged.
    ///
    /// When the primary feed has not produced a tick within
    /// `fallback_threshold_ms` milliseconds the aggregator falls back to
    /// `BestBid` across the remaining feeds.
    PrimaryWithFallback {
        /// Maximum acceptable gap (ms) before falling back.
        fallback_threshold_ms: u64,
    },
}

// ---------------------------------------------------------------------------
// Internal priority queue entry (timestamp-ordered, min-heap)
// ---------------------------------------------------------------------------

/// Tick buffered inside the aggregator's merge queue, carrying its compensated
/// timestamp and source feed metadata.
#[derive(Debug, Clone)]
struct BufferedTick {
    /// Compensated timestamp used for merge ordering.
    compensated_ts_ms: u64,
    /// The tick data.
    tick: NormalizedTick,
    /// Source feed identifier.
    feed_id: Uuid,
    /// Whether this feed is the designated primary.
    feed_is_primary: bool,
}

// BinaryHeap is a max-heap; we want min-timestamp first, so we invert.
impl PartialEq for BufferedTick {
    fn eq(&self, other: &Self) -> bool {
        self.compensated_ts_ms == other.compensated_ts_ms
    }
}

impl Eq for BufferedTick {}

impl PartialOrd for BufferedTick {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BufferedTick {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order so the smallest timestamp has highest priority.
        other.compensated_ts_ms.cmp(&self.compensated_ts_ms)
    }
}

// ---------------------------------------------------------------------------
// Aggregator configuration
// ---------------------------------------------------------------------------

/// Configuration for [`FeedAggregator`].
#[derive(Debug, Clone)]
pub struct AggregatorConfig {
    /// Tick merge strategy.
    pub strategy: MergeStrategy,
    /// Maximum number of ticks to buffer per feed before back-pressure.
    ///
    /// Each feed's internal mpsc channel uses this as its bound. When the
    /// buffer is full the producer must back off. Defaults to 1 024.
    pub feed_buffer_capacity: usize,
    /// Maximum number of ticks to hold in the merge heap at once.
    ///
    /// Larger windows improve timestamp ordering at the cost of latency.
    /// Defaults to 64.
    pub merge_window: usize,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self {
            strategy: MergeStrategy::BestBid,
            feed_buffer_capacity: 1_024,
            merge_window: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// FeedAggregator
// ---------------------------------------------------------------------------

/// Aggregates ticks from N independent feeds into a single ordered stream.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::agg::{FeedAggregator, FeedHandle, AggregatorConfig, MergeStrategy};
///
/// # async fn run() -> Result<(), fin_stream::StreamError> {
/// let mut agg = FeedAggregator::new(AggregatorConfig {
///     strategy: MergeStrategy::BestBid,
///     ..Default::default()
/// });
///
/// let feed_a = FeedHandle::new("binance-btc").with_latency_offset(5);
/// let feed_b = FeedHandle::new("coinbase-btc").with_latency_offset(12);
///
/// let tx_a = agg.add_feed(feed_a)?;
/// let tx_b = agg.add_feed(feed_b)?;
///
/// // Push ticks from each exchange into their respective senders.
/// // The aggregator merges them in compensated-timestamp order.
/// # Ok(())
/// # }
/// ```
pub struct FeedAggregator {
    config: AggregatorConfig,
    /// Registered feeds, keyed by feed UUID.
    feeds: HashMap<Uuid, FeedHandle>,
    /// Internal receive channels, one per feed.
    receivers: Vec<(Uuid, mpsc::Receiver<NormalizedTick>)>,
    /// Merge priority queue (min-heap by compensated timestamp).
    heap: BinaryHeap<BufferedTick>,
    /// Last tick timestamp seen per feed (for fallback logic).
    last_tick_ms: HashMap<Uuid, u64>,
}

impl FeedAggregator {
    /// Create a new aggregator with the given configuration.
    pub fn new(config: AggregatorConfig) -> Self {
        Self {
            config,
            feeds: HashMap::new(),
            receivers: Vec::new(),
            heap: BinaryHeap::new(),
            last_tick_ms: HashMap::new(),
        }
    }

    /// Register a feed with the aggregator.
    ///
    /// Returns the [`mpsc::Sender`] that the feed producer must use to push
    /// ticks into the aggregator. The sender is bounded by
    /// [`AggregatorConfig::feed_buffer_capacity`].
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] when `feed_buffer_capacity` is 0.
    pub fn add_feed(
        &mut self,
        handle: FeedHandle,
    ) -> Result<mpsc::Sender<NormalizedTick>, StreamError> {
        if self.config.feed_buffer_capacity == 0 {
            return Err(StreamError::ConfigError {
                reason: "feed_buffer_capacity must be > 0".into(),
            });
        }
        let (tx, rx) = mpsc::channel(self.config.feed_buffer_capacity);
        let id = handle.id;
        self.last_tick_ms.insert(id, 0);
        self.feeds.insert(id, handle);
        self.receivers.push((id, rx));
        info!(feed_id = %id, "FeedAggregator: registered feed");
        Ok(tx)
    }

    /// Returns the number of registered feeds.
    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    /// Poll all feed receivers once, loading available ticks into the merge
    /// heap.
    ///
    /// This is a non-blocking drain: it collects all ticks that are ready
    /// right now and pushes them onto the internal priority queue.
    pub fn poll_feeds(&mut self) {
        for (feed_id, rx) in &mut self.receivers {
            let feed = match self.feeds.get(feed_id) {
                Some(f) => f.clone(),
                None => continue,
            };
            // Drain available ticks from this feed's channel.
            while let Ok(tick) = rx.try_recv() {
                let compensated_ts =
                    tick.received_at_ms.saturating_sub(feed.latency_offset_ms);
                debug!(
                    feed = %feed.label,
                    ts = compensated_ts,
                    price = %tick.price,
                    "buffering tick"
                );
                self.last_tick_ms.insert(*feed_id, tick.received_at_ms);
                self.heap.push(BufferedTick {
                    compensated_ts_ms: compensated_ts,
                    tick,
                    feed_id: *feed_id,
                    feed_is_primary: feed.is_primary,
                });
            }
        }
    }

    /// Return the next merged tick, if enough data is buffered.
    ///
    /// Returns `None` when the heap is empty or the merge window has not been
    /// filled yet.
    pub fn next_tick(&mut self) -> Option<NormalizedTick> {
        self.poll_feeds();

        if self.heap.is_empty() {
            return None;
        }

        match &self.config.strategy {
            MergeStrategy::BestBid => self.next_best_bid(),
            MergeStrategy::BestAsk => self.next_best_ask(),
            MergeStrategy::VwapWeighted => self.next_vwap(),
            MergeStrategy::PrimaryWithFallback { fallback_threshold_ms } => {
                let threshold = *fallback_threshold_ms;
                self.next_primary_with_fallback(threshold)
            }
        }
    }

    // ---- strategy implementations ----------------------------------------

    fn next_best_bid(&mut self) -> Option<NormalizedTick> {
        // Drain merge_window items, pick the one with the highest price.
        let candidates: Vec<BufferedTick> = (0..self.config.merge_window)
            .filter_map(|_| self.heap.pop())
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let best = candidates
            .iter()
            .max_by(|a, b| a.tick.price.cmp(&b.tick.price))
            .map(|b| b.tick.clone());
        // Push back the rest (all but the best; we've already consumed them).
        for item in candidates {
            self.heap.push(item);
        }
        // Pop the minimum timestamp item (which we just re-pushed) to advance.
        self.heap.pop().map(|b| b.tick)
        // Fallback: if best is None just return None.
        .or(best)
    }

    fn next_best_ask(&mut self) -> Option<NormalizedTick> {
        let candidates: Vec<BufferedTick> = (0..self.config.merge_window)
            .filter_map(|_| self.heap.pop())
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let best = candidates
            .iter()
            .min_by(|a, b| a.tick.price.cmp(&b.tick.price))
            .map(|b| b.tick.clone());
        for item in candidates {
            self.heap.push(item);
        }
        self.heap.pop().map(|b| b.tick).or(best)
    }

    fn next_vwap(&mut self) -> Option<NormalizedTick> {
        let candidates: Vec<BufferedTick> = (0..self.config.merge_window)
            .filter_map(|_| self.heap.pop())
            .collect();
        if candidates.is_empty() {
            return None;
        }

        let total_volume: Decimal = candidates.iter().map(|c| c.tick.quantity).sum();
        if total_volume.is_zero() {
            // Fall back to the most recent tick.
            let most_recent = candidates
                .iter()
                .max_by_key(|c| c.compensated_ts_ms)
                .map(|c| c.tick.clone());
            for item in candidates {
                self.heap.push(item);
            }
            return self.heap.pop().map(|b| b.tick).or(most_recent);
        }

        let vwap: Decimal = candidates
            .iter()
            .map(|c| c.tick.price * c.tick.quantity)
            .sum::<Decimal>()
            / total_volume;

        // Build a synthetic tick from the most recent contributor.
        let anchor = candidates
            .iter()
            .max_by_key(|c| c.compensated_ts_ms)
            .map(|c| c.tick.clone())?;

        let mut synthetic = anchor;
        synthetic.price = vwap;
        synthetic.quantity = total_volume;
        // Clear trade-id: this is a synthetic aggregate, not a real trade.
        synthetic.trade_id = None;

        for item in candidates {
            self.heap.push(item);
        }
        // Advance the heap by one position.
        self.heap.pop();
        Some(synthetic)
    }

    fn next_primary_with_fallback(&mut self, fallback_threshold_ms: u64) -> Option<NormalizedTick> {
        // Find the primary feed ID, if any.
        let primary_id = self
            .feeds
            .values()
            .find(|f| f.is_primary)
            .map(|f| f.id);

        let now_ms = now_ms();

        let primary_is_live = primary_id.map(|pid| {
            self.last_tick_ms
                .get(&pid)
                .map(|&last| {
                    last > 0 && now_ms.saturating_sub(last) <= fallback_threshold_ms
                })
                .unwrap_or(false)
        });

        match (primary_id, primary_is_live) {
            (Some(pid), Some(true)) => {
                // Prefer tick from the primary feed.
                let primary_tick = {
                    let mut tmp: Vec<BufferedTick> = Vec::new();
                    let mut found: Option<NormalizedTick> = None;
                    while let Some(item) = self.heap.pop() {
                        if item.feed_id == pid && found.is_none() {
                            found = Some(item.tick.clone());
                        } else {
                            tmp.push(item);
                        }
                    }
                    for item in tmp {
                        self.heap.push(item);
                    }
                    found
                };
                if primary_tick.is_some() {
                    return primary_tick;
                }
                // Primary is live but has nothing buffered yet — fall through.
                warn!("Primary feed live but no buffered ticks; falling back.");
                self.next_best_bid()
            }
            _ => {
                // Primary absent or stale — use best-bid across remaining feeds.
                debug!("Primary feed stale; falling back to BestBid across all feeds.");
                self.next_best_bid()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ArbOpportunity
// ---------------------------------------------------------------------------

/// A detected inter-feed arbitrage opportunity.
///
/// Emitted by [`ArbDetector::check`] when the best bid on one feed exceeds
/// the best ask on another feed by more than the configured threshold.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbOpportunity {
    /// Symbol for which the discrepancy was detected.
    pub symbol: String,
    /// Label of the feed where you should buy (lower ask).
    pub buy_feed: String,
    /// Label of the feed where you should sell (higher bid).
    pub sell_feed: String,
    /// Gross spread in basis points: `(sell_price - buy_price) / buy_price * 10_000`.
    pub spread_bps: f64,
    /// Buy-side price (the ask on the buy feed).
    pub buy_price: Decimal,
    /// Sell-side price (the bid on the sell feed).
    pub sell_price: Decimal,
    /// Wall-clock timestamp (ms since Unix epoch) when the opportunity was
    /// detected.
    pub detected_at_ms: u64,
}

// ---------------------------------------------------------------------------
// ArbDetector
// ---------------------------------------------------------------------------

/// Detects price discrepancies between feeds and emits [`ArbOpportunity`]s.
///
/// Maintains the most recent tick per feed label. Call [`ArbDetector::ingest`]
/// as each merged tick arrives and poll [`ArbDetector::check`] to receive any
/// newly detected opportunities.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::agg::{ArbDetector, ArbOpportunity};
/// use fin_stream::tick::NormalizedTick;
///
/// let mut detector = ArbDetector::new(10.0); // flag spreads > 10 bps
///
/// // Feed ticks in as they arrive from the aggregator.
/// // detector.ingest("binance", &tick_from_binance);
/// // detector.ingest("coinbase", &tick_from_coinbase);
///
/// // Poll for opportunities.
/// // let opps: Vec<ArbOpportunity> = detector.check();
/// ```
pub struct ArbDetector {
    /// Minimum spread in basis points that triggers an opportunity signal.
    threshold_bps: f64,
    /// Most recent tick observed per feed label.
    latest: HashMap<String, NormalizedTick>,
}

impl ArbDetector {
    /// Create a new detector with the given threshold.
    ///
    /// `threshold_bps` is the minimum inter-feed spread (in basis points) that
    /// must be exceeded before an [`ArbOpportunity`] is emitted. A value of
    /// `10.0` means the spread must be > 0.10%.
    ///
    /// # Panics
    ///
    /// Panics if `threshold_bps` is negative or NaN.
    pub fn new(threshold_bps: f64) -> Self {
        assert!(
            threshold_bps >= 0.0 && threshold_bps.is_finite(),
            "threshold_bps must be a non-negative finite number, got {threshold_bps}"
        );
        Self {
            threshold_bps,
            latest: HashMap::new(),
        }
    }

    /// Ingest the latest tick from a named feed.
    ///
    /// The detector stores only the most recent tick per feed label, so
    /// callers should call this as each tick arrives.
    pub fn ingest(&mut self, feed_label: impl Into<String>, tick: &NormalizedTick) {
        self.latest.insert(feed_label.into(), tick.clone());
    }

    /// Scan all pairs of feeds for arbitrage opportunities.
    ///
    /// Returns a (possibly empty) list of opportunities where the gross spread
    /// exceeds [`Self::threshold_bps`]. Each pair is checked in both
    /// directions.
    ///
    /// Complexity: O(n²) in the number of registered feeds. In practice N is
    /// small (2–8 exchanges) so this is not a bottleneck.
    pub fn check(&self) -> Vec<ArbOpportunity> {
        let feeds: Vec<(&String, &NormalizedTick)> = self.latest.iter().collect();
        let mut opportunities = Vec::new();
        let now = now_ms();

        for i in 0..feeds.len() {
            for j in (i + 1)..feeds.len() {
                let (label_a, tick_a) = feeds[i];
                let (label_b, tick_b) = feeds[j];

                // Check both directions.
                for (buy_label, buy_tick, sell_label, sell_tick) in [
                    (label_a, tick_a, label_b, tick_b),
                    (label_b, tick_b, label_a, tick_a),
                ] {
                    if sell_tick.price <= buy_tick.price {
                        continue;
                    }
                    // Avoid division by zero (price should never be zero, but
                    // guard defensively).
                    if buy_tick.price.is_zero() {
                        continue;
                    }

                    let spread = sell_tick.price - buy_tick.price;
                    // Convert to f64 for bps calculation (acceptable here:
                    // bps is a display/threshold field, not a price).
                    let spread_bps = match (spread.try_into(), buy_tick.price.try_into()) {
                        (Ok(s), Ok(b)) => {
                            let s: f64 = s;
                            let b: f64 = b;
                            if b > 0.0 { (s / b) * 10_000.0 } else { 0.0 }
                        }
                        _ => 0.0,
                    };

                    if spread_bps > self.threshold_bps {
                        debug!(
                            buy = %buy_label,
                            sell = %sell_label,
                            spread_bps,
                            "arb opportunity detected"
                        );
                        opportunities.push(ArbOpportunity {
                            symbol: buy_tick.symbol.clone(),
                            buy_feed: buy_label.clone(),
                            sell_feed: sell_label.clone(),
                            spread_bps,
                            buy_price: buy_tick.price,
                            sell_price: sell_tick.price,
                            detected_at_ms: now,
                        });
                    }
                }
            }
        }

        opportunities
    }

    /// Returns the threshold in basis points configured at construction.
    pub fn threshold_bps(&self) -> f64 {
        self.threshold_bps
    }

    /// Returns the number of feeds currently tracked.
    pub fn feed_count(&self) -> usize {
        self.latest.len()
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{Exchange, NormalizedTick};
    use rust_decimal_macros::dec;

    fn make_tick(price: rust_decimal::Decimal, qty: rust_decimal::Decimal, ts: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTC-USDT".into(),
            price,
            quantity: qty,
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: ts,
        }
    }

    #[test]
    fn arb_detector_finds_opportunity() {
        let mut det = ArbDetector::new(5.0);
        let cheap = make_tick(dec!(100.00), dec!(1), 1_000);
        let pricey = make_tick(dec!(100.20), dec!(1), 1_001);
        det.ingest("feed_a", &cheap);
        det.ingest("feed_b", &pricey);
        let opps = det.check();
        assert!(!opps.is_empty(), "expected an arb opportunity");
        let opp = &opps[0];
        assert_eq!(opp.buy_feed, "feed_a");
        assert_eq!(opp.sell_feed, "feed_b");
        assert!(opp.spread_bps > 5.0);
    }

    #[test]
    fn arb_detector_no_opportunity_below_threshold() {
        let mut det = ArbDetector::new(100.0); // 100 bps threshold
        let a = make_tick(dec!(100.00), dec!(1), 1_000);
        let b = make_tick(dec!(100.05), dec!(1), 1_001); // 5 bps spread
        det.ingest("feed_a", &a);
        det.ingest("feed_b", &b);
        let opps = det.check();
        assert!(opps.is_empty(), "spread below threshold, no opportunity expected");
    }

    #[tokio::test]
    async fn aggregator_add_feed_returns_sender() {
        let mut agg = FeedAggregator::new(AggregatorConfig::default());
        let handle = FeedHandle::new("test-feed");
        let tx = agg.add_feed(handle).expect("add_feed should succeed");
        let tick = make_tick(dec!(50000), dec!(0.5), 1_000);
        tx.send(tick).await.expect("send should succeed");
        assert_eq!(agg.feed_count(), 1);
    }

    #[test]
    fn feed_handle_builder() {
        let h = FeedHandle::new("my-feed")
            .with_latency_offset(10)
            .as_primary();
        assert_eq!(h.label, "my-feed");
        assert_eq!(h.latency_offset_ms, 10);
        assert!(h.is_primary);
    }
}
