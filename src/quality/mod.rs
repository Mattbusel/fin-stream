//! Feed quality scoring, gap detection, and tick deduplication.
//!
//! ## Overview
//!
//! | Type | Responsibility |
//! |------|----------------|
//! | [`QualityScorer`] | Tracks per-symbol latency, gap rate, duplicate rate and emits a 0–100 score |
//! | [`FeedGapDetector`] | Detects timestamp gaps exceeding a configurable threshold |
//! | [`TickDeduplicator`] | LRU cache of recent tick fingerprints; flags exact duplicates |
//! | [`QualityReport`] | Aggregated system-wide health summary |

use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use crate::tick::NormalizedTick;

// ──────────────────────────────────────────────────────────────────────────────
// FeedQualityMetrics
// ──────────────────────────────────────────────────────────────────────────────

/// Per-symbol quality metrics snapshot.
#[derive(Debug, Clone)]
pub struct FeedQualityMetrics {
    /// Symbol these metrics apply to.
    pub symbol: String,
    /// 50th-percentile tick-to-tick latency in milliseconds.
    pub latency_p50_ms: f64,
    /// 99th-percentile tick-to-tick latency in milliseconds.
    pub latency_p99_ms: f64,
    /// Fraction of expected ticks that were missing (`0.0`–`1.0`).
    pub gap_rate: f64,
    /// Fraction of received ticks that were exact duplicates (`0.0`–`1.0`).
    pub duplicate_rate: f64,
    /// Composite quality score in `[0, 100]`.
    ///
    /// `score = 100 * (1 - gap_rate) * (1 - duplicate_rate) * exp(-latency_p99_ms / 1000)`
    pub score: f64,
}

impl FeedQualityMetrics {
    /// Compute the composite quality score from the constituent metrics.
    ///
    /// Formula: `score = 100 * (1 - gap_rate) * (1 - duplicate_rate) * latency_weight`
    /// where `latency_weight = exp(-latency_p99_ms / 1000.0)`.
    pub fn compute_score(gap_rate: f64, duplicate_rate: f64, latency_p99_ms: f64) -> f64 {
        let latency_weight = (-latency_p99_ms / 1000.0).exp();
        100.0
            * (1.0 - gap_rate).clamp(0.0, 1.0)
            * (1.0 - duplicate_rate).clamp(0.0, 1.0)
            * latency_weight
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// QualityScorer
// ──────────────────────────────────────────────────────────────────────────────

/// Per-symbol state maintained by [`QualityScorer`].
struct SymbolState {
    /// Rolling window of inter-tick latencies (ms).
    latencies: VecDeque<f64>,
    /// Total ticks seen.
    total_ticks: u64,
    /// Ticks flagged as gaps.
    gap_count: u64,
    /// Ticks flagged as duplicates.
    duplicate_count: u64,
    /// Maximum rolling window size.
    window: usize,
}

impl SymbolState {
    fn new(window: usize) -> Self {
        Self {
            latencies: VecDeque::with_capacity(window),
            total_ticks: 0,
            gap_count: 0,
            duplicate_count: 0,
            window,
        }
    }

    fn push_latency(&mut self, ms: f64) {
        if self.latencies.len() == self.window {
            self.latencies.pop_front();
        }
        self.latencies.push_back(ms);
    }

    fn percentile(&self, pct: f64) -> f64 {
        if self.latencies.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.latencies.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn gap_rate(&self) -> f64 {
        if self.total_ticks == 0 {
            0.0
        } else {
            self.gap_count as f64 / self.total_ticks as f64
        }
    }

    fn duplicate_rate(&self) -> f64 {
        if self.total_ticks == 0 {
            0.0
        } else {
            self.duplicate_count as f64 / self.total_ticks as f64
        }
    }
}

/// Tracks per-symbol feed quality using a rolling window of recent ticks.
pub struct QualityScorer {
    state: DashMap<String, SymbolState>,
    window: usize,
}

impl QualityScorer {
    /// Create a new scorer with the given rolling-window size (number of ticks).
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "window must be > 0");
        Self {
            state: DashMap::new(),
            window,
        }
    }

    /// Record a new tick for a symbol.
    ///
    /// * `symbol` — instrument identifier
    /// * `received_at_ms` — local receipt timestamp in milliseconds
    /// * `is_gap` — whether this tick represents a detected gap (missing data)
    /// * `is_duplicate` — whether this tick is a duplicate
    /// * `prev_received_at_ms` — previous tick's receipt time (for latency computation)
    pub fn record_tick(
        &self,
        symbol: &str,
        received_at_ms: u64,
        prev_received_at_ms: Option<u64>,
        is_gap: bool,
        is_duplicate: bool,
    ) {
        let mut entry = self
            .state
            .entry(symbol.to_string())
            .or_insert_with(|| SymbolState::new(self.window));

        entry.total_ticks += 1;
        if is_gap {
            entry.gap_count += 1;
        }
        if is_duplicate {
            entry.duplicate_count += 1;
        }
        if let Some(prev) = prev_received_at_ms {
            let delta = received_at_ms.saturating_sub(prev) as f64;
            entry.push_latency(delta);
        }
    }

    /// Get the current quality metrics for a symbol.
    ///
    /// Returns `None` if no ticks have been recorded for the symbol.
    pub fn metrics(&self, symbol: &str) -> Option<FeedQualityMetrics> {
        let state = self.state.get(symbol)?;
        let latency_p50_ms = state.percentile(50.0);
        let latency_p99_ms = state.percentile(99.0);
        let gap_rate = state.gap_rate();
        let duplicate_rate = state.duplicate_rate();
        let score = FeedQualityMetrics::compute_score(gap_rate, duplicate_rate, latency_p99_ms);
        Some(FeedQualityMetrics {
            symbol: symbol.to_string(),
            latency_p50_ms,
            latency_p99_ms,
            gap_rate,
            duplicate_rate,
            score,
        })
    }

    /// All tracked symbols.
    pub fn symbols(&self) -> Vec<String> {
        self.state.iter().map(|e| e.key().clone()).collect()
    }

    /// Generate a [`QualityReport`] aggregating all tracked symbols.
    pub fn report(&self) -> QualityReport {
        let per_feed: Vec<FeedQualityMetrics> = self
            .state
            .iter()
            .filter_map(|e| self.metrics(e.key()))
            .collect();

        let worst_feed = per_feed
            .iter()
            .min_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .map(|m| m.symbol.clone());

        let best_feed = per_feed
            .iter()
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .map(|m| m.symbol.clone());

        let system_health_score = if per_feed.is_empty() {
            100.0
        } else {
            per_feed.iter().map(|m| m.score).sum::<f64>() / per_feed.len() as f64
        };

        QualityReport {
            per_feed,
            worst_feed,
            best_feed,
            system_health_score,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// FeedGapDetector
// ──────────────────────────────────────────────────────────────────────────────

/// Detects gaps in a tick stream based on inter-tick timestamp deltas.
pub struct FeedGapDetector {
    /// Gap threshold in milliseconds. Deltas exceeding this are flagged.
    threshold_ms: u64,
    /// Last seen timestamp per symbol.
    last_ts: HashMap<String, u64>,
}

impl FeedGapDetector {
    /// Create a new gap detector.
    ///
    /// * `threshold_ms` — inter-tick gap threshold; deltas greater than this
    ///   are reported as gaps.
    pub fn new(threshold_ms: u64) -> Self {
        Self {
            threshold_ms,
            last_ts: HashMap::new(),
        }
    }

    /// Process a tick timestamp for the given symbol.
    ///
    /// Returns `true` if a gap was detected (i.e. the delta from the previous
    /// tick exceeds `threshold_ms`), `false` otherwise (or if this is the first
    /// tick for the symbol).
    pub fn process(&mut self, symbol: &str, ts_ms: u64) -> bool {
        let prev = self.last_ts.insert(symbol.to_string(), ts_ms);
        match prev {
            None => false,
            Some(prev_ts) => ts_ms.saturating_sub(prev_ts) > self.threshold_ms,
        }
    }

    /// Return the last seen timestamp for a symbol, if any.
    pub fn last_ts(&self, symbol: &str) -> Option<u64> {
        self.last_ts.get(symbol).copied()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TickDeduplicator
// ──────────────────────────────────────────────────────────────────────────────

/// Fields to include when computing a tick's deduplication fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFields {
    /// Hash price, quantity, and exchange timestamp together.
    PriceQtyTimestamp,
    /// Hash only the exchange timestamp (useful when price/qty may jitter).
    TimestampOnly,
}

/// Configuration for [`TickDeduplicator`].
#[derive(Debug, Clone)]
pub struct DeduplicatorConfig {
    /// Time window (ms) within which identical fingerprints are flagged as duplicates.
    pub window_ms: u64,
    /// Which fields to include in the fingerprint hash.
    pub hash_fields: HashFields,
}

impl DeduplicatorConfig {
    /// Create a new config.
    pub fn new(window_ms: u64, hash_fields: HashFields) -> Self {
        Self { window_ms, hash_fields }
    }
}

/// Decision returned by [`TickDeduplicator::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupDecision {
    /// This tick appears to be unique within the deduplication window.
    Unique,
    /// This tick matches a recently seen fingerprint and is a duplicate.
    Duplicate,
}

/// LRU cache-based tick deduplicator.
///
/// Maintains a fixed-capacity window of recent tick fingerprints. When a new
/// tick's fingerprint matches one already in the window it is classified as a
/// [`DedupDecision::Duplicate`].
///
/// Fingerprinting uses [`std::hash::DefaultHasher`] (SipHash 1-3 in Rust's
/// standard library) over the selected fields. This is not cryptographic but
/// is sufficient for deduplication purposes.
pub struct TickDeduplicator {
    config: DeduplicatorConfig,
    /// Ring buffer of (fingerprint, received_at_ms) pairs.
    seen: VecDeque<(u64, u64)>,
    /// Maximum number of fingerprints to retain.
    capacity: usize,
}

impl TickDeduplicator {
    /// Create a new deduplicator.
    ///
    /// * `config` — deduplication window and hash field selection
    /// * `capacity` — maximum number of recent fingerprints to retain in the LRU window
    pub fn new(config: DeduplicatorConfig, capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            config,
            seen: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Check whether a tick is a duplicate.
    ///
    /// Evicts fingerprints older than `window_ms` before checking, then adds
    /// the new fingerprint to the cache.
    pub fn check(&mut self, tick: &NormalizedTick) -> DedupDecision {
        let fp = self.fingerprint(tick);
        let now = tick.received_at_ms;

        // Evict expired entries.
        while self
            .seen
            .front()
            .is_some_and(|(_, ts)| now.saturating_sub(*ts) > self.config.window_ms)
        {
            self.seen.pop_front();
        }

        // Check for existing fingerprint.
        let is_dup = self.seen.iter().any(|(h, _)| *h == fp);

        // Add to cache (evict oldest if at capacity).
        if self.seen.len() == self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back((fp, now));

        if is_dup {
            DedupDecision::Duplicate
        } else {
            DedupDecision::Unique
        }
    }

    fn fingerprint(&self, tick: &NormalizedTick) -> u64 {
        let mut h = DefaultHasher::new();
        tick.symbol.hash(&mut h);
        match self.config.hash_fields {
            HashFields::PriceQtyTimestamp => {
                tick.price.to_string().hash(&mut h);
                tick.quantity.to_string().hash(&mut h);
                tick.exchange_ts_ms.hash(&mut h);
            }
            HashFields::TimestampOnly => {
                tick.exchange_ts_ms.hash(&mut h);
            }
        }
        h.finish()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// QualityReport
// ──────────────────────────────────────────────────────────────────────────────

/// Aggregated feed quality report for all tracked symbols.
#[derive(Debug, Clone)]
pub struct QualityReport {
    /// Per-symbol quality metrics.
    pub per_feed: Vec<FeedQualityMetrics>,
    /// Symbol with the lowest quality score (`None` if no feeds tracked).
    pub worst_feed: Option<String>,
    /// Symbol with the highest quality score (`None` if no feeds tracked).
    pub best_feed: Option<String>,
    /// Average quality score across all feeds (100.0 when no feeds tracked).
    pub system_health_score: f64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Shared scorer (Arc wrapper)
// ──────────────────────────────────────────────────────────────────────────────

/// Thread-safe shared [`QualityScorer`] wrapped in an `Arc`.
pub type SharedQualityScorer = Arc<QualityScorer>;

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use crate::tick::{Exchange, NormalizedTick};

    fn make_tick(symbol: &str, price: f64, ts_ms: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price: Decimal::from_f64_retain(price).unwrap_or(Decimal::ONE),
            quantity: Decimal::ONE,
            side: None,
            trade_id: None,
            exchange_ts_ms: Some(ts_ms),
            received_at_ms: ts_ms,
        }
    }

    // --- FeedQualityMetrics::compute_score ---

    #[test]
    fn score_perfect_feed() {
        let score = FeedQualityMetrics::compute_score(0.0, 0.0, 0.0);
        // exp(0) = 1.0 → score = 100 * 1 * 1 * 1 = 100
        assert!((score - 100.0).abs() < 1e-6);
    }

    #[test]
    fn score_high_latency_reduces_score() {
        let score_low = FeedQualityMetrics::compute_score(0.0, 0.0, 10.0);
        let score_high = FeedQualityMetrics::compute_score(0.0, 0.0, 500.0);
        assert!(score_high < score_low);
    }

    #[test]
    fn score_gap_rate_reduces_score() {
        let score_no_gap = FeedQualityMetrics::compute_score(0.0, 0.0, 0.0);
        let score_gap = FeedQualityMetrics::compute_score(0.1, 0.0, 0.0);
        assert!(score_gap < score_no_gap);
    }

    #[test]
    fn score_duplicate_rate_reduces_score() {
        let score_no_dup = FeedQualityMetrics::compute_score(0.0, 0.0, 0.0);
        let score_dup = FeedQualityMetrics::compute_score(0.0, 0.2, 0.0);
        assert!(score_dup < score_no_dup);
    }

    #[test]
    fn score_all_bad_near_zero() {
        // gap_rate=1, dup_rate=0 → (1-1)*... = 0
        let score = FeedQualityMetrics::compute_score(1.0, 0.0, 999.0);
        assert!(score < 1.0);
    }

    // --- QualityScorer ---

    #[test]
    fn scorer_returns_none_for_unknown_symbol() {
        let scorer = QualityScorer::new(100);
        assert!(scorer.metrics("UNKNOWN").is_none());
    }

    #[test]
    fn scorer_records_tick_and_returns_metrics() {
        let scorer = QualityScorer::new(100);
        scorer.record_tick("BTC", 1000, Some(990), false, false);
        let m = scorer.metrics("BTC").expect("metrics should exist");
        assert_eq!(m.symbol, "BTC");
        assert!(m.score > 0.0);
    }

    #[test]
    fn scorer_gap_rate_reflects_gaps() {
        let scorer = QualityScorer::new(100);
        scorer.record_tick("ETH", 1000, None, false, false);
        scorer.record_tick("ETH", 2000, Some(1000), true, false); // gap
        scorer.record_tick("ETH", 3000, Some(2000), false, false);
        let m = scorer.metrics("ETH").unwrap();
        // 1 gap out of 3 ticks = 1/3
        assert!((m.gap_rate - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn scorer_duplicate_rate_reflects_duplicates() {
        let scorer = QualityScorer::new(100);
        scorer.record_tick("SOL", 1000, None, false, false);
        scorer.record_tick("SOL", 1001, Some(1000), false, true); // dup
        let m = scorer.metrics("SOL").unwrap();
        assert!((m.duplicate_rate - 0.5).abs() < 1e-6);
    }

    #[test]
    fn scorer_latency_percentiles() {
        let scorer = QualityScorer::new(100);
        let mut prev = 1000_u64;
        for i in 1..=10 {
            scorer.record_tick("AAPL", prev + (i * 10), Some(prev), false, false);
            prev += i * 10;
        }
        let m = scorer.metrics("AAPL").unwrap();
        assert!(m.latency_p50_ms > 0.0);
        assert!(m.latency_p99_ms >= m.latency_p50_ms);
    }

    // --- QualityReport ---

    #[test]
    fn report_empty_scorer_is_healthy() {
        let scorer = QualityScorer::new(10);
        let report = scorer.report();
        assert!(report.per_feed.is_empty());
        assert!(report.worst_feed.is_none());
        assert!(report.best_feed.is_none());
        assert!((report.system_health_score - 100.0).abs() < 1e-6);
    }

    #[test]
    fn report_identifies_worst_and_best() {
        let scorer = QualityScorer::new(100);
        // Good feed: no gaps, no dups, low latency
        scorer.record_tick("GOOD", 1000, Some(999), false, false);
        // Bad feed: gaps
        scorer.record_tick("BAD", 1000, Some(0), true, false);
        let report = scorer.report();
        assert_eq!(report.worst_feed.as_deref(), Some("BAD"));
        assert_eq!(report.best_feed.as_deref(), Some("GOOD"));
    }

    // --- FeedGapDetector ---

    #[test]
    fn gap_detector_first_tick_no_gap() {
        let mut det = FeedGapDetector::new(1000);
        assert!(!det.process("BTC", 5000));
    }

    #[test]
    fn gap_detector_within_threshold_no_gap() {
        let mut det = FeedGapDetector::new(1000);
        det.process("BTC", 5000);
        assert!(!det.process("BTC", 5500)); // delta = 500 ms < 1000 ms
    }

    #[test]
    fn gap_detector_exceeds_threshold() {
        let mut det = FeedGapDetector::new(1000);
        det.process("BTC", 5000);
        assert!(det.process("BTC", 7000)); // delta = 2000 ms > 1000 ms
    }

    #[test]
    fn gap_detector_independent_symbols() {
        let mut det = FeedGapDetector::new(500);
        det.process("BTC", 1000);
        det.process("ETH", 2000);
        // BTC: delta = 1500 ms > 500 ms → gap
        assert!(det.process("BTC", 2500));
        // ETH: delta = 100 ms < 500 ms → no gap
        assert!(!det.process("ETH", 2100));
    }

    // --- TickDeduplicator ---

    #[test]
    fn dedup_unique_tick_accepted() {
        let cfg = DeduplicatorConfig::new(5000, HashFields::PriceQtyTimestamp);
        let mut ded = TickDeduplicator::new(cfg, 128);
        let t = make_tick("BTC", 50000.0, 1000);
        assert_eq!(ded.check(&t), DedupDecision::Unique);
    }

    #[test]
    fn dedup_duplicate_tick_flagged() {
        let cfg = DeduplicatorConfig::new(5000, HashFields::PriceQtyTimestamp);
        let mut ded = TickDeduplicator::new(cfg, 128);
        let t = make_tick("BTC", 50000.0, 1000);
        ded.check(&t);
        assert_eq!(ded.check(&t), DedupDecision::Duplicate);
    }

    #[test]
    fn dedup_different_price_is_unique() {
        let cfg = DeduplicatorConfig::new(5000, HashFields::PriceQtyTimestamp);
        let mut ded = TickDeduplicator::new(cfg, 128);
        let t1 = make_tick("BTC", 50000.0, 1000);
        let t2 = make_tick("BTC", 50001.0, 1000);
        ded.check(&t1);
        assert_eq!(ded.check(&t2), DedupDecision::Unique);
    }

    #[test]
    fn dedup_timestamp_only_mode() {
        let cfg = DeduplicatorConfig::new(5000, HashFields::TimestampOnly);
        let mut ded = TickDeduplicator::new(cfg, 128);
        let t1 = make_tick("BTC", 50000.0, 1000);
        let t2 = make_tick("BTC", 99999.0, 1000); // different price, same ts
        ded.check(&t1);
        // In TimestampOnly mode, same timestamp → duplicate
        assert_eq!(ded.check(&t2), DedupDecision::Duplicate);
    }

    #[test]
    fn dedup_expired_entries_evicted() {
        let cfg = DeduplicatorConfig::new(100, HashFields::PriceQtyTimestamp);
        let mut ded = TickDeduplicator::new(cfg, 128);
        let mut t = make_tick("BTC", 50000.0, 1000);
        ded.check(&t);
        // Same tick but now ts is far in the future — old entry should be evicted.
        t.received_at_ms = 5000; // 4000ms > window of 100ms
        t.exchange_ts_ms = Some(5000);
        assert_eq!(ded.check(&t), DedupDecision::Unique);
    }
}
