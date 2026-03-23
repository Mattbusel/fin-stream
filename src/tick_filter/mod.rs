//! # Module: tick_filter
//!
//! ## Responsibility
//! Multi-stage tick data quality filtering pipeline. Filters ticks for
//! outlier prices, staleness, duplicates, and minimum size constraints.
//!
//! ## Filter Stages (applied in order)
//! 1. `SizeFilter` — reject ticks below minimum size
//! 2. `StaleFilter` — reject ticks older than a staleness threshold
//! 3. `DuplicateFilter` — reject consecutive identical ticks
//! 4. `OutlierFilter` — reject ticks deviating more than N std from rolling mean
//!
//! ## Guarantees
//! - No panics; construction validates all parameters
//! - `FilterStats` tracks per-filter rejection counts
//! - `DuplicateFilter` only deduplicates consecutive ticks (not global)

use std::collections::VecDeque;

// ─────────────────────────────────────────
//  FilterConfig
// ─────────────────────────────────────────

/// Configuration for the tick filter pipeline.
///
/// # Example
/// ```rust
/// use fin_stream::tick_filter::FilterConfig;
///
/// let cfg = FilterConfig {
///     max_price_deviation_pct: 3.0,
///     min_size: 0.01,
///     stale_after_ms: 5_000,
///     outlier_window: 20,
///     outlier_std_threshold: 3.0,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct FilterConfig {
    /// Maximum allowed percentage deviation from rolling mean before a tick
    /// is considered an outlier. E.g. `3.0` = 3%.
    pub max_price_deviation_pct: f64,
    /// Minimum trade size (quantity). Ticks below this are rejected.
    pub min_size: f64,
    /// Ticks with `received_at_ms` older than `now - stale_after_ms` are rejected.
    pub stale_after_ms: u64,
    /// Number of recent prices used for the outlier rolling window.
    pub outlier_window: usize,
    /// Number of standard deviations from rolling mean to trigger outlier rejection.
    pub outlier_std_threshold: f64,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            max_price_deviation_pct: 3.0,
            min_size: 0.001,
            stale_after_ms: 5_000,
            outlier_window: 20,
            outlier_std_threshold: 3.0,
        }
    }
}

// ─────────────────────────────────────────
//  Minimal tick representation for the filter
// ─────────────────────────────────────────

/// A lightweight tick suitable for filter-stage processing.
///
/// In production code, this maps from `NormalizedTick`. In tests, it can
/// be constructed directly.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterableTick {
    /// Instrument symbol.
    pub symbol: String,
    /// Trade price (f64 for filter arithmetic).
    pub price: f64,
    /// Trade quantity / size.
    pub quantity: f64,
    /// Local receipt timestamp in milliseconds since Unix epoch.
    pub received_at_ms: u64,
}

// ─────────────────────────────────────────
//  FilterResult
// ─────────────────────────────────────────

/// Result of applying the filter pipeline to a single tick.
#[derive(Debug, Clone)]
pub enum FilterResult {
    /// The tick passed all filter stages.
    Pass(FilterableTick),
    /// The tick was rejected by a filter stage.
    Filtered {
        /// Human-readable reason for rejection.
        reason: String,
    },
}

impl FilterResult {
    /// Returns `true` if the tick passed all filters.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, FilterResult::Pass(_))
    }

    /// Returns the tick if it passed, otherwise `None`.
    #[must_use]
    pub fn tick(self) -> Option<FilterableTick> {
        match self {
            FilterResult::Pass(t) => Some(t),
            FilterResult::Filtered { .. } => None,
        }
    }
}

// ─────────────────────────────────────────
//  FilterStats
// ─────────────────────────────────────────

/// Cumulative counts of ticks rejected by each filter stage.
#[derive(Debug, Clone, Default)]
pub struct FilterStats {
    /// Total ticks submitted to the pipeline.
    pub total: u64,
    /// Ticks rejected by `SizeFilter`.
    pub rejected_size: u64,
    /// Ticks rejected by `StaleFilter`.
    pub rejected_stale: u64,
    /// Ticks rejected by `DuplicateFilter`.
    pub rejected_duplicate: u64,
    /// Ticks rejected by `OutlierFilter`.
    pub rejected_outlier: u64,
    /// Ticks that passed all filters.
    pub passed: u64,
}

impl FilterStats {
    /// Total rejected ticks across all stages.
    #[must_use]
    pub fn total_rejected(&self) -> u64 {
        self.rejected_size
            + self.rejected_stale
            + self.rejected_duplicate
            + self.rejected_outlier
    }

    /// Pass-through rate `[0.0, 1.0]`. Returns `0.0` if no ticks processed.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.passed as f64 / self.total as f64
    }
}

// ─────────────────────────────────────────
//  Individual filter stages
// ─────────────────────────────────────────

/// Rejects ticks whose quantity is below `min_size`.
#[derive(Debug, Clone)]
pub struct SizeFilter {
    min_size: f64,
}

impl SizeFilter {
    /// Constructs a [`SizeFilter`].
    #[must_use]
    pub fn new(min_size: f64) -> Self {
        Self { min_size }
    }

    /// Returns `Some(reason)` if the tick is rejected, `None` if it passes.
    #[must_use]
    pub fn check(&self, tick: &FilterableTick) -> Option<String> {
        if tick.quantity < self.min_size {
            Some(format!(
                "size {} below minimum {}",
                tick.quantity, self.min_size
            ))
        } else {
            None
        }
    }
}

/// Rejects ticks older than `stale_after_ms` relative to `now_ms`.
#[derive(Debug, Clone)]
pub struct StaleFilter {
    stale_after_ms: u64,
}

impl StaleFilter {
    /// Constructs a [`StaleFilter`].
    #[must_use]
    pub fn new(stale_after_ms: u64) -> Self {
        Self { stale_after_ms }
    }

    /// Returns `Some(reason)` if the tick is stale, `None` if fresh.
    #[must_use]
    pub fn check(&self, tick: &FilterableTick, now_ms: u64) -> Option<String> {
        let age = now_ms.saturating_sub(tick.received_at_ms);
        if age > self.stale_after_ms {
            Some(format!(
                "tick is {}ms old (threshold {}ms)",
                age, self.stale_after_ms
            ))
        } else {
            None
        }
    }
}

/// Rejects consecutive identical ticks (same symbol + price + quantity).
#[derive(Debug, Clone, Default)]
pub struct DuplicateFilter {
    last: Option<(String, u64, u64)>, // (symbol, price_bits, qty_bits)
}

impl DuplicateFilter {
    /// Constructs a [`DuplicateFilter`].
    #[must_use]
    pub fn new() -> Self {
        Self { last: None }
    }

    /// Returns `Some(reason)` if the tick is a duplicate of the previous one.
    pub fn check(&mut self, tick: &FilterableTick) -> Option<String> {
        let key = (
            tick.symbol.clone(),
            tick.price.to_bits(),
            tick.quantity.to_bits(),
        );
        if self.last.as_ref() == Some(&key) {
            Some(format!(
                "duplicate tick: symbol={} price={} qty={}",
                tick.symbol, tick.price, tick.quantity
            ))
        } else {
            self.last = Some(key);
            None
        }
    }

    /// Reset the duplicate state (e.g., on reconnect).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Rejects ticks whose price deviates more than `std_threshold` standard
/// deviations from a rolling mean of recent prices.
#[derive(Debug, Clone)]
pub struct OutlierFilter {
    window: usize,
    std_threshold: f64,
    prices: VecDeque<f64>,
}

impl OutlierFilter {
    /// Constructs an [`OutlierFilter`].
    ///
    /// Before the window is filled, all ticks pass (insufficient data to compute std).
    #[must_use]
    pub fn new(window: usize, std_threshold: f64) -> Self {
        Self {
            window: window.max(2),
            std_threshold,
            prices: VecDeque::new(),
        }
    }

    /// Returns `Some(reason)` if the tick is an outlier, `None` otherwise.
    pub fn check(&mut self, tick: &FilterableTick) -> Option<String> {
        if self.prices.len() < self.window {
            self.prices.push_back(tick.price);
            return None;
        }
        // Compute mean and std of current window
        let mean: f64 = self.prices.iter().sum::<f64>() / self.prices.len() as f64;
        let variance: f64 = self
            .prices
            .iter()
            .map(|p| (p - mean).powi(2))
            .sum::<f64>()
            / (self.prices.len() - 1) as f64;
        let std = variance.sqrt();
        let z = if std == 0.0 {
            0.0
        } else {
            (tick.price - mean).abs() / std
        };
        // Slide window
        self.prices.pop_front();
        self.prices.push_back(tick.price);

        if z > self.std_threshold {
            Some(format!(
                "price {} is {:.2} std from mean {:.4} (threshold {})",
                tick.price, z, mean, self.std_threshold
            ))
        } else {
            None
        }
    }
}

// ─────────────────────────────────────────
//  TickFilter — composed pipeline
// ─────────────────────────────────────────

/// Multi-stage tick quality filter.
///
/// Applies SizeFilter → StaleFilter → DuplicateFilter → OutlierFilter in order.
/// The first rejection encountered is returned; subsequent stages are skipped.
///
/// # Example
/// ```rust
/// use fin_stream::tick_filter::{FilterConfig, FilterableTick, TickFilter};
///
/// let mut filter = TickFilter::new(FilterConfig::default());
/// let tick = FilterableTick {
///     symbol: "BTC-USD".into(),
///     price: 50_000.0,
///     quantity: 0.1,
///     received_at_ms: 1_700_000_000_000,
/// };
/// // With a very old timestamp this will be filtered:
/// let result = filter.apply(tick, 1_700_001_000_000);
/// assert!(!result.is_pass());
/// ```
#[derive(Debug)]
pub struct TickFilter {
    size_filter: SizeFilter,
    stale_filter: StaleFilter,
    duplicate_filter: DuplicateFilter,
    outlier_filter: OutlierFilter,
    /// Cumulative filter statistics.
    pub stats: FilterStats,
}

impl TickFilter {
    /// Constructs a [`TickFilter`] from a [`FilterConfig`].
    #[must_use]
    pub fn new(config: FilterConfig) -> Self {
        Self {
            size_filter: SizeFilter::new(config.min_size),
            stale_filter: StaleFilter::new(config.stale_after_ms),
            duplicate_filter: DuplicateFilter::new(),
            outlier_filter: OutlierFilter::new(
                config.outlier_window,
                config.outlier_std_threshold,
            ),
            stats: FilterStats::default(),
        }
    }

    /// Apply all filter stages to a tick.
    ///
    /// - `now_ms`: current system time in milliseconds (for staleness check).
    pub fn apply(&mut self, tick: FilterableTick, now_ms: u64) -> FilterResult {
        self.stats.total += 1;

        if let Some(reason) = self.size_filter.check(&tick) {
            self.stats.rejected_size += 1;
            return FilterResult::Filtered { reason };
        }
        if let Some(reason) = self.stale_filter.check(&tick, now_ms) {
            self.stats.rejected_stale += 1;
            return FilterResult::Filtered { reason };
        }
        if let Some(reason) = self.duplicate_filter.check(&tick) {
            self.stats.rejected_duplicate += 1;
            return FilterResult::Filtered { reason };
        }
        if let Some(reason) = self.outlier_filter.check(&tick) {
            self.stats.rejected_outlier += 1;
            return FilterResult::Filtered { reason };
        }
        self.stats.passed += 1;
        FilterResult::Pass(tick)
    }

    /// Reset internal state (duplicate tracker, outlier window, stats).
    pub fn reset(&mut self) {
        self.duplicate_filter.reset();
        self.outlier_filter.prices.clear();
        self.stats = FilterStats::default();
    }
}

// ─────────────────────────────────────────
//  Unit Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(price: f64, quantity: f64, received_at_ms: u64) -> FilterableTick {
        FilterableTick {
            symbol: "TEST".into(),
            price,
            quantity,
            received_at_ms,
        }
    }

    const NOW_MS: u64 = 1_700_000_000_000;

    // SizeFilter
    #[test]
    fn size_filter_rejects_small() {
        let f = SizeFilter::new(0.1);
        let t = tick(100.0, 0.05, NOW_MS);
        assert!(f.check(&t).is_some());
    }

    #[test]
    fn size_filter_passes_exact() {
        let f = SizeFilter::new(0.1);
        let t = tick(100.0, 0.1, NOW_MS);
        assert!(f.check(&t).is_none());
    }

    // StaleFilter
    #[test]
    fn stale_filter_rejects_old() {
        let f = StaleFilter::new(1_000);
        let old = tick(100.0, 1.0, NOW_MS - 2_000);
        assert!(f.check(&old, NOW_MS).is_some());
    }

    #[test]
    fn stale_filter_passes_fresh() {
        let f = StaleFilter::new(1_000);
        let fresh = tick(100.0, 1.0, NOW_MS - 500);
        assert!(f.check(&fresh, NOW_MS).is_none());
    }

    // DuplicateFilter
    #[test]
    fn duplicate_filter_rejects_consecutive_same() {
        let mut f = DuplicateFilter::new();
        let t = tick(100.0, 1.0, NOW_MS);
        assert!(f.check(&t).is_none()); // first — passes
        let t2 = tick(100.0, 1.0, NOW_MS + 1);
        assert!(f.check(&t2).is_some()); // duplicate
    }

    #[test]
    fn duplicate_filter_passes_different_price() {
        let mut f = DuplicateFilter::new();
        assert!(f.check(&tick(100.0, 1.0, NOW_MS)).is_none());
        assert!(f.check(&tick(100.1, 1.0, NOW_MS + 1)).is_none());
    }

    #[test]
    fn duplicate_filter_reset_clears_state() {
        let mut f = DuplicateFilter::new();
        assert!(f.check(&tick(100.0, 1.0, NOW_MS)).is_none());
        f.reset();
        // After reset, same tick should pass again
        assert!(f.check(&tick(100.0, 1.0, NOW_MS)).is_none());
    }

    // OutlierFilter
    #[test]
    fn outlier_filter_passes_before_window_full() {
        let mut f = OutlierFilter::new(5, 3.0);
        for _ in 0..4 {
            assert!(f.check(&tick(100.0, 1.0, NOW_MS)).is_none());
        }
    }

    #[test]
    fn outlier_filter_rejects_spike() {
        let mut f = OutlierFilter::new(5, 3.0);
        // Fill window with stable prices
        for _ in 0..5 {
            f.check(&tick(100.0, 1.0, NOW_MS));
        }
        // Spike: 100 + 1000 std units above
        let result = f.check(&tick(10_000.0, 1.0, NOW_MS));
        assert!(result.is_some());
    }

    #[test]
    fn outlier_filter_passes_normal() {
        let mut f = OutlierFilter::new(5, 3.0);
        for _ in 0..5 {
            f.check(&tick(100.0, 1.0, NOW_MS));
        }
        // Small movement — should pass
        assert!(f.check(&tick(100.1, 1.0, NOW_MS)).is_none());
    }

    // TickFilter (composed)
    #[test]
    fn tick_filter_passes_good_tick() {
        let mut filter = TickFilter::new(FilterConfig::default());
        let t = FilterableTick {
            symbol: "BTC".into(),
            price: 50_000.0,
            quantity: 0.1,
            received_at_ms: NOW_MS,
        };
        let result = filter.apply(t, NOW_MS);
        assert!(result.is_pass());
        assert_eq!(filter.stats.passed, 1);
    }

    #[test]
    fn tick_filter_rejects_stale() {
        let mut filter = TickFilter::new(FilterConfig {
            stale_after_ms: 1_000,
            ..FilterConfig::default()
        });
        let t = FilterableTick {
            symbol: "ETH".into(),
            price: 3_000.0,
            quantity: 1.0,
            received_at_ms: NOW_MS - 2_000,
        };
        let result = filter.apply(t, NOW_MS);
        assert!(!result.is_pass());
        assert_eq!(filter.stats.rejected_stale, 1);
    }

    #[test]
    fn tick_filter_rejects_small_size() {
        let mut filter = TickFilter::new(FilterConfig {
            min_size: 1.0,
            ..FilterConfig::default()
        });
        let t = FilterableTick {
            symbol: "ETH".into(),
            price: 3_000.0,
            quantity: 0.001,
            received_at_ms: NOW_MS,
        };
        assert!(!filter.apply(t, NOW_MS).is_pass());
        assert_eq!(filter.stats.rejected_size, 1);
    }

    #[test]
    fn tick_filter_stats_pass_rate() {
        let mut filter = TickFilter::new(FilterConfig::default());
        for _ in 0..5 {
            let t = FilterableTick {
                symbol: "X".into(),
                price: 100.0,
                quantity: 1.0,
                received_at_ms: NOW_MS,
            };
            filter.apply(t, NOW_MS);
        }
        // First passes, rest are duplicates (consecutive identical)
        assert_eq!(filter.stats.passed, 1);
        assert_eq!(filter.stats.rejected_duplicate, 4);
        assert!((filter.stats.pass_rate() - 0.2).abs() < 1e-10);
    }

    #[test]
    fn tick_filter_reset_clears_stats() {
        let mut filter = TickFilter::new(FilterConfig::default());
        let t = FilterableTick {
            symbol: "X".into(),
            price: 100.0,
            quantity: 1.0,
            received_at_ms: NOW_MS,
        };
        filter.apply(t, NOW_MS);
        filter.reset();
        assert_eq!(filter.stats.total, 0);
    }

    #[test]
    fn filter_result_tick_extracts_on_pass() {
        let t = FilterableTick {
            symbol: "Y".into(),
            price: 200.0,
            quantity: 2.0,
            received_at_ms: NOW_MS,
        };
        let result = FilterResult::Pass(t.clone());
        assert_eq!(result.tick(), Some(t));
    }

    #[test]
    fn filter_result_tick_none_on_filtered() {
        let result = FilterResult::Filtered {
            reason: "test".into(),
        };
        assert_eq!(result.tick(), None);
    }
}
