//! High-throughput tick processing pipeline.
//!
//! Provides [`TickProcessor`] which normalises raw ticks, applies composable
//! filters, maintains per-symbol statistics, and routes processed ticks to
//! named sinks — all concurrently using [`dashmap::DashMap`] and atomics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use dashmap::DashMap;

// Re-export RawTick from data_pipeline so callers can use a single type.
pub use crate::data_pipeline::RawTick;

// ---------------------------------------------------------------------------
// Trade side
// ---------------------------------------------------------------------------

/// Aggressor side of a trade tick.
#[derive(Debug, Clone, PartialEq)]
pub enum TradeSide {
    /// Buyer-initiated trade.
    Buy,
    /// Seller-initiated trade.
    Sell,
    /// Side could not be determined.
    Unknown,
}

// ---------------------------------------------------------------------------
// Processed tick
// ---------------------------------------------------------------------------

/// A validated and normalised market data tick ready for downstream consumption.
#[derive(Debug, Clone)]
pub struct ProcessedTick {
    /// Ticker symbol (upper-cased).
    pub symbol: String,
    /// Nanosecond Unix timestamp.
    pub timestamp_ns: u64,
    /// Trade price.
    pub price: f64,
    /// Trade size (shares or contracts).
    pub size: f64,
    /// Aggressor side (if determinable).
    pub side: Option<TradeSide>,
    /// Monotonically increasing sequence number assigned by this processor.
    pub sequence_num: u64,
    /// Source identifier (e.g. exchange or feed name).
    pub source: String,
}

// ---------------------------------------------------------------------------
// Tick filter
// ---------------------------------------------------------------------------

/// Composable filter applied to each incoming tick.
///
/// A tick passes only when ALL filters accept it.
#[derive(Debug, Clone)]
pub enum TickFilter {
    /// Reject ticks with size below the threshold.
    MinSize(f64),
    /// Reject ticks with price above the threshold.
    MaxPrice(f64),
    /// Reject ticks with price below the threshold.
    MinPrice(f64),
    /// Accept only ticks whose symbol is in this set.
    SymbolSet(Vec<String>),
    /// Accept only ticks from exchanges in this set.
    ExchangeSet(Vec<String>),
    /// Accept only ticks within the nanosecond timestamp range [start_ns, end_ns].
    TimeRange {
        /// Inclusive start of the time window (ns).
        start_ns: u64,
        /// Inclusive end of the time window (ns).
        end_ns: u64,
    },
}

impl TickFilter {
    /// Return `true` if the raw tick passes this filter.
    fn accepts(&self, tick: &RawTick) -> bool {
        match self {
            TickFilter::MinSize(min) => tick.size >= *min,
            TickFilter::MaxPrice(max) => tick.price <= *max,
            TickFilter::MinPrice(min) => tick.price >= *min,
            TickFilter::SymbolSet(syms) => syms.iter().any(|s| s.eq_ignore_ascii_case(&tick.symbol)),
            TickFilter::ExchangeSet(exs) => {
                exs.iter().any(|e| e.eq_ignore_ascii_case(&tick.exchange))
            }
            TickFilter::TimeRange { start_ns, end_ns } => {
                tick.timestamp_ns >= *start_ns && tick.timestamp_ns <= *end_ns
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-symbol statistics
// ---------------------------------------------------------------------------

/// Cumulative statistics for a single symbol.
#[derive(Debug, Clone)]
pub struct TickStats {
    /// Ticker symbol.
    pub symbol: String,
    /// Number of ticks processed.
    pub tick_count: u64,
    /// Total traded volume.
    pub total_volume: f64,
    /// Volume-weighted average price.
    pub vwap: f64,
    /// Lowest price seen.
    pub min_price: f64,
    /// Highest price seen.
    pub max_price: f64,
    /// Most recent price.
    pub last_price: f64,
    /// Timestamp of the most recent tick (ns).
    pub last_timestamp_ns: u64,
    // Internal accumulators for VWAP.
    pv_sum: f64,
    vol_sum: f64,
}

impl TickStats {
    fn new(symbol: String) -> Self {
        Self {
            symbol,
            tick_count: 0,
            total_volume: 0.0,
            vwap: 0.0,
            min_price: f64::MAX,
            max_price: f64::MIN,
            last_price: 0.0,
            last_timestamp_ns: 0,
            pv_sum: 0.0,
            vol_sum: 0.0,
        }
    }

    fn update(&mut self, tick: &ProcessedTick) {
        self.tick_count += 1;
        self.total_volume += tick.size;
        self.pv_sum += tick.price * tick.size;
        self.vol_sum += tick.size;
        self.vwap = if self.vol_sum > 0.0 { self.pv_sum / self.vol_sum } else { 0.0 };
        if tick.price < self.min_price { self.min_price = tick.price; }
        if tick.price > self.max_price { self.max_price = tick.price; }
        self.last_price = tick.price;
        self.last_timestamp_ns = tick.timestamp_ns;
    }
}

// ---------------------------------------------------------------------------
// TickRouter
// ---------------------------------------------------------------------------

/// Routes processed ticks to named sinks based on symbol.
#[derive(Debug, Clone, Default)]
pub struct TickRouter {
    /// Mapping from symbol → list of sink names.
    pub routes: Arc<DashMap<String, Vec<String>>>,
}

impl TickRouter {
    /// Register `sink` as a destination for ticks with `symbol`.
    pub fn add_route(&self, symbol: &str, sink: &str) {
        self.routes
            .entry(symbol.to_uppercase())
            .or_default()
            .push(sink.to_string());
    }

    /// Return the list of sink names for the tick's symbol (empty if none registered).
    pub fn route(&self, tick: &ProcessedTick) -> Vec<String> {
        self.routes
            .get(&tick.symbol)
            .map(|v| v.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// TickProcessor
// ---------------------------------------------------------------------------

/// High-throughput tick processing engine.
///
/// Thread-safe: all mutable state is protected by atomics or `DashMap`.
pub struct TickProcessor {
    filters: Vec<TickFilter>,
    stats: Arc<DashMap<String, TickStats>>,
    router: TickRouter,
    sequence: Arc<AtomicU64>,
    processed_count: Arc<AtomicU64>,
    filtered_count: Arc<AtomicU64>,
}

impl Default for TickProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl TickProcessor {
    /// Create a new empty processor with no filters and no routes.
    pub fn new() -> Self {
        Self {
            filters: Vec::new(),
            stats: Arc::new(DashMap::new()),
            router: TickRouter::default(),
            sequence: Arc::new(AtomicU64::new(0)),
            processed_count: Arc::new(AtomicU64::new(0)),
            filtered_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Add a filter to the processing pipeline.
    pub fn add_filter(&mut self, filter: TickFilter) {
        self.filters.push(filter);
    }

    /// Process one raw tick.
    ///
    /// Returns `None` if the tick is rejected by any filter or is invalid
    /// (non-positive price or size).
    pub fn process(&self, raw: RawTick) -> Option<ProcessedTick> {
        // Sanity checks.
        if raw.price <= 0.0 || raw.size <= 0.0 {
            self.filtered_count.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // Apply filters.
        for filter in &self.filters {
            if !filter.accepts(&raw) {
                self.filtered_count.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.processed_count.fetch_add(1, Ordering::Relaxed);

        let tick = ProcessedTick {
            symbol: raw.symbol.to_uppercase(),
            timestamp_ns: raw.timestamp_ns,
            price: raw.price,
            size: raw.size,
            side: None,
            sequence_num: seq,
            source: raw.exchange.clone(),
        };

        self.update_stats(&tick);
        Some(tick)
    }

    /// Process a batch of raw ticks, returning only those that pass all filters.
    pub fn process_batch(&self, raws: Vec<RawTick>) -> Vec<ProcessedTick> {
        raws.into_iter().filter_map(|r| self.process(r)).collect()
    }

    /// Update the running statistics for the tick's symbol.
    pub fn update_stats(&self, tick: &ProcessedTick) {
        self.stats
            .entry(tick.symbol.clone())
            .or_insert_with(|| TickStats::new(tick.symbol.clone()))
            .update(tick);
    }

    /// Retrieve a snapshot of statistics for a single symbol.
    pub fn stats_for(&self, symbol: &str) -> Option<TickStats> {
        self.stats.get(&symbol.to_uppercase()).map(|s| s.clone())
    }

    /// Retrieve statistics snapshots for all tracked symbols.
    pub fn all_stats(&self) -> Vec<TickStats> {
        self.stats.iter().map(|e| e.value().clone()).collect()
    }

    /// Ticks processed per second over `elapsed_secs`.
    pub fn throughput_rps(&self, elapsed_secs: f64) -> f64 {
        if elapsed_secs <= 0.0 { return 0.0; }
        self.processed_count.load(Ordering::Relaxed) as f64 / elapsed_secs
    }

    /// Reset all per-symbol statistics and counters.
    pub fn reset_stats(&self) {
        self.stats.clear();
        self.processed_count.store(0, Ordering::Relaxed);
        self.filtered_count.store(0, Ordering::Relaxed);
        // Do not reset sequence — sequence numbers must remain monotonic.
    }

    /// Total number of ticks that passed all filters.
    pub fn processed_count(&self) -> u64 {
        self.processed_count.load(Ordering::Relaxed)
    }

    /// Total number of ticks rejected by at least one filter.
    pub fn filtered_count(&self) -> u64 {
        self.filtered_count.load(Ordering::Relaxed)
    }

    /// Access the tick router.
    pub fn router(&self) -> &TickRouter {
        &self.router
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(symbol: &str, price: f64, size: f64, ts: u64) -> RawTick {
        RawTick {
            symbol: symbol.to_string(),
            timestamp_ns: ts,
            price,
            size,
            exchange: "NYSE".to_string(),
            conditions: vec![],
        }
    }

    #[test]
    fn test_process_valid_tick() {
        let proc = TickProcessor::new();
        let raw = make_tick("AAPL", 150.0, 100.0, 1_000_000);
        let result = proc.process(raw);
        assert!(result.is_some());
        let tick = result.unwrap();
        assert_eq!(tick.symbol, "AAPL");
        assert_eq!(tick.sequence_num, 0);
    }

    #[test]
    fn test_filter_min_size() {
        let mut proc = TickProcessor::new();
        proc.add_filter(TickFilter::MinSize(200.0));
        let small = make_tick("AAPL", 150.0, 100.0, 1_000_000);
        let large = make_tick("AAPL", 150.0, 300.0, 1_000_001);
        assert!(proc.process(small).is_none());
        assert!(proc.process(large).is_some());
    }

    #[test]
    fn test_filter_symbol_set() {
        let mut proc = TickProcessor::new();
        proc.add_filter(TickFilter::SymbolSet(vec!["AAPL".to_string(), "MSFT".to_string()]));
        let allowed = make_tick("AAPL", 150.0, 100.0, 1);
        let blocked = make_tick("GOOG", 2800.0, 10.0, 2);
        assert!(proc.process(allowed).is_some());
        assert!(proc.process(blocked).is_none());
    }

    #[test]
    fn test_stats_vwap() {
        let proc = TickProcessor::new();
        proc.process(make_tick("TSLA", 200.0, 50.0, 1));
        proc.process(make_tick("TSLA", 210.0, 150.0, 2));
        let stats = proc.stats_for("TSLA").expect("stats");
        // VWAP = (200*50 + 210*150) / 200 = (10000 + 31500) / 200 = 207.5
        assert!((stats.vwap - 207.5).abs() < 1e-9);
        assert_eq!(stats.tick_count, 2);
    }

    #[test]
    fn test_invalid_tick_rejected() {
        let proc = TickProcessor::new();
        assert!(proc.process(make_tick("X", -1.0, 100.0, 1)).is_none());
        assert!(proc.process(make_tick("X", 100.0, 0.0, 2)).is_none());
        assert_eq!(proc.filtered_count(), 2);
    }

    #[test]
    fn test_process_batch() {
        let mut proc = TickProcessor::new();
        proc.add_filter(TickFilter::MinPrice(100.0));
        let raws = vec![
            make_tick("A", 50.0, 10.0, 1),  // filtered
            make_tick("B", 150.0, 10.0, 2), // passes
            make_tick("C", 200.0, 10.0, 3), // passes
        ];
        let out = proc.process_batch(raws);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_tick_router() {
        let router = TickRouter::default();
        router.add_route("AAPL", "sink_a");
        router.add_route("AAPL", "sink_b");
        router.add_route("MSFT", "sink_c");

        let tick = ProcessedTick {
            symbol: "AAPL".to_string(),
            timestamp_ns: 0,
            price: 150.0,
            size: 100.0,
            side: None,
            sequence_num: 0,
            source: "NYSE".to_string(),
        };
        let sinks = router.route(&tick);
        assert_eq!(sinks.len(), 2);
        assert!(sinks.contains(&"sink_a".to_string()));
    }

    #[test]
    fn test_throughput_rps() {
        let proc = TickProcessor::new();
        for i in 0..100u64 {
            proc.process(make_tick("Z", 100.0, 1.0, i));
        }
        let rps = proc.throughput_rps(0.1);
        assert!((rps - 1000.0).abs() < 1.0);
    }

    #[test]
    fn test_reset_stats() {
        let proc = TickProcessor::new();
        proc.process(make_tick("AAPL", 100.0, 10.0, 1));
        proc.reset_stats();
        assert!(proc.stats_for("AAPL").is_none());
        assert_eq!(proc.processed_count(), 0);
    }

    #[test]
    fn test_time_range_filter() {
        let mut proc = TickProcessor::new();
        proc.add_filter(TickFilter::TimeRange { start_ns: 1000, end_ns: 2000 });
        assert!(proc.process(make_tick("X", 100.0, 1.0, 1500)).is_some());
        assert!(proc.process(make_tick("X", 100.0, 1.0, 500)).is_none());
        assert!(proc.process(make_tick("X", 100.0, 1.0, 3000)).is_none());
    }
}
