//! # Module: pipeline
//!
//! Composable tick normalization pipeline.
//!
//! ## Responsibility
//! Provides a `TickPipeline` that chains `TickFilter` and `TickTransform`
//! implementations. Each tick passes through all filters (AND-logic); any
//! failed filter drops the tick. Passing ticks are fed through all transforms
//! in order.
//!
//! ## Guarantees
//! - Filters are applied before transforms
//! - All built-in filters and transforms are `Send + Sync`
//! - `process` returns `None` if any filter rejects the tick

use crate::tick::NormalizedTick;
use std::collections::HashSet;

// ─── Traits ───────────────────────────────────────────────────────────────────

/// Decides whether a tick should pass through the pipeline.
pub trait TickFilter: Send + Sync {
    /// Return `true` to keep the tick, `false` to drop it.
    fn filter(&self, tick: &NormalizedTick) -> bool;
}

/// Mutates a tick as it passes through the pipeline.
pub trait TickTransform: Send + Sync {
    /// Apply the transformation and return the (possibly modified) tick.
    fn transform(&self, tick: NormalizedTick) -> NormalizedTick;
}

// ─── Built-in Filters ─────────────────────────────────────────────────────────

/// Drops ticks whose price is outside `[min, max]`.
pub struct PriceRangeFilter {
    /// Minimum acceptable price (inclusive).
    pub min: rust_decimal::Decimal,
    /// Maximum acceptable price (inclusive).
    pub max: rust_decimal::Decimal,
}

impl TickFilter for PriceRangeFilter {
    fn filter(&self, tick: &NormalizedTick) -> bool {
        tick.price >= self.min && tick.price <= self.max
    }
}

/// Drops ticks whose quantity is below `min`.
pub struct VolumeFilter {
    /// Minimum acceptable quantity (inclusive).
    pub min: rust_decimal::Decimal,
}

impl TickFilter for VolumeFilter {
    fn filter(&self, tick: &NormalizedTick) -> bool {
        tick.quantity >= self.min
    }
}

/// Drops ticks whose symbol is not in the allowed set.
pub struct SymbolFilter {
    /// Set of allowed symbols.
    pub symbols: HashSet<String>,
}

impl TickFilter for SymbolFilter {
    fn filter(&self, tick: &NormalizedTick) -> bool {
        self.symbols.contains(&tick.symbol)
    }
}

/// Drops ticks older than `max_age_ms` milliseconds relative to a reference
/// timestamp (set at construction time as "now").
///
/// Uses `received_at_ms` to determine staleness.
pub struct StaleFilter {
    /// Maximum age in milliseconds.
    pub max_age_ms: u64,
    /// Reference "now" timestamp in milliseconds.
    pub reference_ms: u64,
}

impl StaleFilter {
    /// Create a `StaleFilter` with the current system time as reference.
    pub fn new(max_age_ms: u64) -> Self {
        let reference_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self { max_age_ms, reference_ms }
    }

    /// Create a `StaleFilter` with an explicit reference timestamp (useful for tests).
    pub fn with_reference(max_age_ms: u64, reference_ms: u64) -> Self {
        Self { max_age_ms, reference_ms }
    }
}

impl TickFilter for StaleFilter {
    fn filter(&self, tick: &NormalizedTick) -> bool {
        // Tick is fresh if reference - received_at <= max_age_ms
        self.reference_ms.saturating_sub(tick.received_at_ms) <= self.max_age_ms
    }
}

// ─── Built-in Transforms ─────────────────────────────────────────────────────

/// Rounds tick prices to the specified number of decimal places.
pub struct PriceRounder {
    /// Number of decimal places.
    pub decimals: u32,
}

impl TickTransform for PriceRounder {
    fn transform(&self, mut tick: NormalizedTick) -> NormalizedTick {
        // Use Decimal's built-in round_dp which takes scale (decimal places)
        tick.price = tick.price.round_dp(self.decimals);
        tick
    }
}

/// Scales tick quantities by a constant factor.
pub struct VolumeNormalizer {
    /// Multiplicative scale factor.
    pub scale_factor: rust_decimal::Decimal,
}

impl TickTransform for VolumeNormalizer {
    fn transform(&self, mut tick: NormalizedTick) -> NormalizedTick {
        tick.quantity *= self.scale_factor;
        tick
    }
}

/// Aligns tick timestamps to the nearest `granularity_ms` boundary.
///
/// `aligned = round(received_at_ms / granularity_ms) * granularity_ms`
pub struct TimestampAligner {
    /// Granularity in milliseconds (e.g. 1000 for 1-second buckets).
    pub granularity_ms: u64,
}

impl TickTransform for TimestampAligner {
    fn transform(&self, mut tick: NormalizedTick) -> NormalizedTick {
        if self.granularity_ms == 0 {
            return tick;
        }
        let g = self.granularity_ms;
        // Round to nearest boundary
        let half = g / 2;
        tick.received_at_ms = ((tick.received_at_ms + half) / g) * g;
        if let Some(ets) = tick.exchange_ts_ms {
            tick.exchange_ts_ms = Some(((ets + half) / g) * g);
        }
        tick
    }
}

// ─── Pipeline ─────────────────────────────────────────────────────────────────

/// Composable tick normalization pipeline.
///
/// Add filters with `add_filter` and transforms with `add_transform`.
/// Filters are applied first; all filters must pass for a tick to continue.
/// Transforms are then applied in the order they were added.
pub struct TickPipeline {
    filters: Vec<Box<dyn TickFilter>>,
    transforms: Vec<Box<dyn TickTransform>>,
}

impl TickPipeline {
    /// Create an empty pipeline (no filters, no transforms).
    pub fn new() -> Self {
        Self { filters: Vec::new(), transforms: Vec::new() }
    }

    /// Add a filter. The filter will be applied to all subsequent ticks.
    pub fn add_filter(&mut self, filter: impl TickFilter + 'static) {
        self.filters.push(Box::new(filter));
    }

    /// Add a transform. Transforms are applied in insertion order.
    pub fn add_transform(&mut self, transform: impl TickTransform + 'static) {
        self.transforms.push(Box::new(transform));
    }

    /// Process a single tick through the pipeline.
    ///
    /// Returns `None` if any filter rejects the tick.
    /// Returns `Some(transformed_tick)` otherwise.
    pub fn process(&self, tick: NormalizedTick) -> Option<NormalizedTick> {
        // Apply all filters
        for filter in &self.filters {
            if !filter.filter(&tick) {
                return None;
            }
        }
        // Apply all transforms
        let mut out = tick;
        for transform in &self.transforms {
            out = transform.transform(out);
        }
        Some(out)
    }
}

impl Default for TickPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{Exchange, NormalizedTick};
    use rust_decimal_macros::dec;

    fn make_tick(symbol: &str, price: rust_decimal::Decimal, qty: rust_decimal::Decimal, received_at_ms: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price,
            quantity: qty,
            side: None,
            trade_id: None,
            exchange_ts_ms: Some(received_at_ms),
            received_at_ms,
        }
    }

    #[test]
    fn test_empty_pipeline_passes_tick() {
        let pipeline = TickPipeline::new();
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_price_range_filter_passes() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(PriceRangeFilter { min: dec!(1000), max: dec!(100000) });
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_price_range_filter_rejects_low() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(PriceRangeFilter { min: dec!(1000), max: dec!(100000) });
        let tick = make_tick("BTC", dec!(500), dec!(1), 1000);
        assert!(pipeline.process(tick).is_none());
    }

    #[test]
    fn test_price_range_filter_rejects_high() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(PriceRangeFilter { min: dec!(1000), max: dec!(100000) });
        let tick = make_tick("BTC", dec!(200000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_none());
    }

    #[test]
    fn test_volume_filter_passes() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(VolumeFilter { min: dec!(0.5) });
        let tick = make_tick("ETH", dec!(3000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_volume_filter_rejects_low() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(VolumeFilter { min: dec!(1.0) });
        let tick = make_tick("ETH", dec!(3000), dec!(0.1), 1000);
        assert!(pipeline.process(tick).is_none());
    }

    #[test]
    fn test_symbol_filter_passes_allowed() {
        let mut pipeline = TickPipeline::new();
        let mut syms = HashSet::new();
        syms.insert("BTC".to_string());
        pipeline.add_filter(SymbolFilter { symbols: syms });
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_symbol_filter_rejects_unknown() {
        let mut pipeline = TickPipeline::new();
        let syms: HashSet<String> = ["BTC".to_string()].into();
        pipeline.add_filter(SymbolFilter { symbols: syms });
        let tick = make_tick("ETH", dec!(3000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_none());
    }

    #[test]
    fn test_stale_filter_passes_fresh() {
        let now_ms = 1_000_000_u64;
        let filter = StaleFilter::with_reference(5000, now_ms);
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(filter);
        // Tick received 1000ms ago → fresh
        let tick = make_tick("BTC", dec!(50000), dec!(1), now_ms - 1000);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_stale_filter_rejects_old() {
        let now_ms = 1_000_000_u64;
        let filter = StaleFilter::with_reference(1000, now_ms);
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(filter);
        // Tick received 5000ms ago → stale
        let tick = make_tick("BTC", dec!(50000), dec!(1), now_ms - 5000);
        assert!(pipeline.process(tick).is_none());
    }

    #[test]
    fn test_price_rounder_transform() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_transform(PriceRounder { decimals: 2 });
        let tick = make_tick("BTC", dec!(50000.12345), dec!(1), 1000);
        let out = pipeline.process(tick).unwrap();
        // Should be rounded to 2 decimal places
        assert_eq!(out.price, dec!(50000.12));
    }

    #[test]
    fn test_volume_normalizer_transform() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_transform(VolumeNormalizer { scale_factor: dec!(0.001) });
        let tick = make_tick("BTC", dec!(50000), dec!(1000), 1000);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.quantity, dec!(1));
    }

    #[test]
    fn test_timestamp_aligner_rounds_to_granularity() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_transform(TimestampAligner { granularity_ms: 1000 });
        // 1500ms → rounds to 2000ms (nearest 1000ms boundary)
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1500);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.received_at_ms, 2000);
    }

    #[test]
    fn test_timestamp_aligner_rounds_down() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_transform(TimestampAligner { granularity_ms: 1000 });
        // 1200ms → rounds to 1000ms
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1200);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.received_at_ms, 1000);
    }

    #[test]
    fn test_multiple_filters_all_must_pass() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(PriceRangeFilter { min: dec!(1000), max: dec!(100000) });
        pipeline.add_filter(VolumeFilter { min: dec!(0.5) });
        // Good tick
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1000);
        assert!(pipeline.process(tick).is_some());
        // Bad price
        let tick2 = make_tick("BTC", dec!(500), dec!(1), 1000);
        assert!(pipeline.process(tick2).is_none());
        // Bad volume
        let tick3 = make_tick("BTC", dec!(50000), dec!(0.1), 1000);
        assert!(pipeline.process(tick3).is_none());
    }

    #[test]
    fn test_transforms_applied_in_order() {
        let mut pipeline = TickPipeline::new();
        // First scale volume up by 2, then scale down by 0.5 → net 1x
        pipeline.add_transform(VolumeNormalizer { scale_factor: dec!(2) });
        pipeline.add_transform(VolumeNormalizer { scale_factor: dec!(0.5) });
        let tick = make_tick("BTC", dec!(50000), dec!(100), 1000);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.quantity, dec!(100));
    }

    #[test]
    fn test_filter_and_transform_combined() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_filter(PriceRangeFilter { min: dec!(100), max: dec!(200) });
        pipeline.add_transform(PriceRounder { decimals: 0 });
        let tick = make_tick("STOCK", dec!(150.75), dec!(10), 1000);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.price, dec!(151));
    }

    #[test]
    fn test_default_pipeline_passes_all() {
        let pipeline = TickPipeline::default();
        let tick = make_tick("XYZ", dec!(999999), dec!(0.0001), 0);
        assert!(pipeline.process(tick).is_some());
    }

    #[test]
    fn test_timestamp_aligner_zero_granularity_noop() {
        let mut pipeline = TickPipeline::new();
        pipeline.add_transform(TimestampAligner { granularity_ms: 0 });
        let tick = make_tick("BTC", dec!(50000), dec!(1), 1234);
        let out = pipeline.process(tick).unwrap();
        assert_eq!(out.received_at_ms, 1234);
    }
}
