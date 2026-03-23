//! Market data normalization, OHLCV bar construction, and quality checking pipeline.
//!
//! Provides a composable pipeline from raw ticks to clean OHLCV bars with
//! per-symbol quality reporting.

use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// RawTick
// ---------------------------------------------------------------------------

/// A raw market data tick as received from an exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTick {
    /// Ticker symbol.
    pub symbol: String,
    /// Nanosecond timestamp (Unix epoch).
    pub timestamp_ns: u64,
    /// Trade price.
    pub price: f64,
    /// Trade size (shares or contracts).
    pub size: f64,
    /// Exchange identifier (e.g. "NYSE", "NASDAQ").
    pub exchange: String,
    /// Sale condition codes (e.g. ["@", "I"]).
    pub conditions: Vec<String>,
}

// ---------------------------------------------------------------------------
// OhlcvBar
// ---------------------------------------------------------------------------

/// A completed OHLCV bar for a symbol.
#[derive(Debug, Clone)]
pub struct OhlcvBar {
    /// Ticker symbol.
    pub symbol: String,
    /// Bar open timestamp (nanoseconds).
    pub open_time_ns: u64,
    /// Bar close timestamp (nanoseconds, last tick that closed the bar).
    pub close_time_ns: u64,
    /// Opening price.
    pub open: f64,
    /// Highest price in bar.
    pub high: f64,
    /// Lowest price in bar.
    pub low: f64,
    /// Closing price.
    pub close: f64,
    /// Total volume.
    pub volume: f64,
    /// Volume-weighted average price.
    pub vwap: f64,
    /// Number of trades in bar.
    pub trade_count: usize,
}

// ---------------------------------------------------------------------------
// DataQualityFlag
// ---------------------------------------------------------------------------

/// Quality issue detected in a tick.
#[derive(Debug, Clone, PartialEq)]
pub enum DataQualityFlag {
    /// Tick passed all quality checks.
    Clean,
    /// Price has not changed for more than `age_ms` milliseconds.
    StalePrice {
        /// Staleness age in milliseconds.
        age_ms: u64,
    },
    /// Price moved more than `deviation_sigma` standard deviations from recent mean.
    SuspiciousSpike {
        /// Deviation in sigma units.
        deviation_sigma: f64,
    },
    /// Gap detected in tick stream (no data for `gap_ms` ms).
    MissingData {
        /// Gap duration in milliseconds.
        gap_ms: u64,
    },
    /// Trade volume is statistically anomalous.
    OutlierVolume,
    /// Exact duplicate of a previously seen tick.
    DuplicateTick,
}

// ---------------------------------------------------------------------------
// QualityReport
// ---------------------------------------------------------------------------

/// Summary quality statistics for a symbol's tick stream.
#[derive(Debug, Clone, Default)]
pub struct QualityReport {
    /// Symbol identifier.
    pub symbol: String,
    /// Total ticks evaluated.
    pub total_ticks: usize,
    /// Ticks with no quality flags (Clean).
    pub clean_ticks: usize,
    /// Count of each flag type observed.
    pub flagged: HashMap<String, usize>,
    /// Overall data quality score (0.0–1.0).
    pub data_quality_score: f64,
}

// ---------------------------------------------------------------------------
// TickNormalizer
// ---------------------------------------------------------------------------

/// Normalizes raw ticks: rounds prices, standardises exchange codes, etc.
pub struct TickNormalizer {
    /// Number of decimal places to round prices to.
    pub price_decimals: u32,
}

impl TickNormalizer {
    /// Create a normalizer rounding prices to `price_decimals` decimal places.
    pub fn new(price_decimals: u32) -> Self {
        Self { price_decimals }
    }

    /// Normalize a single tick in place:
    /// - rounds price to configured decimal places
    /// - upper-cases exchange code
    pub fn normalize(&self, mut tick: RawTick) -> RawTick {
        let factor = 10_f64.powi(self.price_decimals as i32);
        tick.price = (tick.price * factor).round() / factor;
        tick.exchange = tick.exchange.to_uppercase();
        tick
    }

    /// Returns `true` if the tick should be kept (none of `excluded` conditions present).
    pub fn filter_conditions(tick: &RawTick, excluded: &[&str]) -> bool {
        for cond in &tick.conditions {
            if excluded.contains(&cond.as_str()) {
                return false;
            }
        }
        true
    }

    /// Remove exact duplicate ticks (same timestamp_ns, price, and size) in place.
    ///
    /// Preserves the first occurrence and removes subsequent duplicates.
    pub fn deduplicate(ticks: &mut Vec<RawTick>) {
        let mut seen: Vec<(u64, u64, u64)> = Vec::with_capacity(ticks.len());
        ticks.retain(|t| {
            let key = (
                t.timestamp_ns,
                t.price.to_bits(),
                t.size.to_bits(),
            );
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        });
    }
}

impl Default for TickNormalizer {
    fn default() -> Self {
        Self::new(2)
    }
}

// ---------------------------------------------------------------------------
// OhlcvBuilder
// ---------------------------------------------------------------------------

/// State for building a single OHLCV bar.
struct BarState {
    symbol: String,
    open_time_ns: u64,
    close_time_ns: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    value_sum: f64, // for VWAP = sum(price * size) / sum(size)
    trade_count: usize,
}

impl BarState {
    fn new(tick: &RawTick) -> Self {
        Self {
            symbol: tick.symbol.clone(),
            open_time_ns: tick.timestamp_ns,
            close_time_ns: tick.timestamp_ns,
            open: tick.price,
            high: tick.price,
            low: tick.price,
            close: tick.price,
            volume: tick.size,
            value_sum: tick.price * tick.size,
            trade_count: 1,
        }
    }

    fn push(&mut self, tick: &RawTick) {
        if tick.price > self.high { self.high = tick.price; }
        if tick.price < self.low { self.low = tick.price; }
        self.close = tick.price;
        self.close_time_ns = tick.timestamp_ns;
        self.volume += tick.size;
        self.value_sum += tick.price * tick.size;
        self.trade_count += 1;
    }

    fn to_bar(&self) -> OhlcvBar {
        OhlcvBar {
            symbol: self.symbol.clone(),
            open_time_ns: self.open_time_ns,
            close_time_ns: self.close_time_ns,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap: if self.volume > 0.0 { self.value_sum / self.volume } else { self.close },
            trade_count: self.trade_count,
        }
    }
}

/// Streaming OHLCV bar builder for a single symbol.
pub struct OhlcvBuilder {
    /// Symbol identifier.
    pub symbol: String,
    /// Bar duration in nanoseconds.
    pub bar_duration_ns: u64,
    /// Current in-progress bar state.
    current: Option<BarState>,
}

impl OhlcvBuilder {
    /// Create a new builder for `symbol` with bars of `bar_duration_ns` nanoseconds.
    pub fn new(symbol: impl Into<String>, bar_duration_ns: u64) -> Self {
        Self { symbol: symbol.into(), bar_duration_ns, current: None }
    }

    /// Push a tick into the builder.
    ///
    /// Returns a completed [`OhlcvBar`] when the time window for the current
    /// bar has elapsed, otherwise returns `None`.
    pub fn push(&mut self, tick: &RawTick) -> Option<OhlcvBar> {
        match &mut self.current {
            None => {
                self.current = Some(BarState::new(tick));
                None
            }
            Some(state) => {
                if tick.timestamp_ns >= state.open_time_ns + self.bar_duration_ns {
                    // Complete the current bar and start a new one.
                    let completed = state.to_bar();
                    self.current = Some(BarState::new(tick));
                    Some(completed)
                } else {
                    state.push(tick);
                    None
                }
            }
        }
    }

    /// Return a snapshot of the current partial bar without consuming it.
    pub fn current_bar(&self) -> Option<OhlcvBar> {
        self.current.as_ref().map(|s| s.to_bar())
    }

    /// Force-close the current bar and return it.
    pub fn flush(&mut self) -> Option<OhlcvBar> {
        self.current.take().map(|s| s.to_bar())
    }
}

// ---------------------------------------------------------------------------
// QualityChecker
// ---------------------------------------------------------------------------

/// Stateful tick quality checker.
pub struct QualityChecker {
    /// Number of observations to use for z-score calculation.
    pub z_score_window: usize,
    /// Sigma threshold for spike detection.
    pub spike_sigma: f64,
    /// Milliseconds without a price change before marking as stale.
    pub stale_threshold_ms: u64,
    /// Per-symbol price history for z-score computation.
    price_history: HashMap<String, VecDeque<f64>>,
    /// Per-symbol volume history.
    volume_history: HashMap<String, VecDeque<f64>>,
    /// Per-symbol last tick timestamp (ns).
    last_ts: HashMap<String, u64>,
    /// Per-symbol last price seen.
    last_price: HashMap<String, f64>,
}

impl QualityChecker {
    /// Create a new quality checker.
    pub fn new(z_score_window: usize, spike_sigma: f64, stale_threshold_ms: u64) -> Self {
        Self {
            z_score_window,
            spike_sigma,
            stale_threshold_ms,
            price_history: HashMap::new(),
            volume_history: HashMap::new(),
            last_ts: HashMap::new(),
            last_price: HashMap::new(),
        }
    }

    /// Run all quality checks on a tick and return the list of flags.
    ///
    /// If no issues are found, returns `vec![DataQualityFlag::Clean]`.
    pub fn check(&mut self, tick: &RawTick) -> Vec<DataQualityFlag> {
        let mut flags = Vec::new();

        // --- Staleness check ---
        if let Some(&last_price) = self.last_price.get(&tick.symbol) {
            if let Some(&last_ts) = self.last_ts.get(&tick.symbol) {
                if tick.price == last_price && last_ts < tick.timestamp_ns {
                    let age_ms = (tick.timestamp_ns - last_ts) / 1_000_000;
                    if age_ms >= self.stale_threshold_ms {
                        flags.push(DataQualityFlag::StalePrice { age_ms });
                    }
                }
                // Gap check
                if last_ts < tick.timestamp_ns {
                    let gap_ms = (tick.timestamp_ns - last_ts) / 1_000_000;
                    if gap_ms > self.stale_threshold_ms * 10 {
                        flags.push(DataQualityFlag::MissingData { gap_ms });
                    }
                }
            }
        }

        // --- Spike check ---
        // Clone the history snapshot so the mutable borrow on price_history ends
        // before we call check_spike (which takes &self).
        let hist_snapshot: VecDeque<f64> = self
            .price_history
            .entry(tick.symbol.clone())
            .or_default()
            .clone();
        if let Some(flag) = self.check_spike(tick.price, &hist_snapshot) {
            flags.push(flag);
        }

        // --- Volume outlier check ---
        let vol_hist = self.volume_history.entry(tick.symbol.clone()).or_default();
        if vol_hist.len() >= 5 {
            let mean: f64 = vol_hist.iter().sum::<f64>() / vol_hist.len() as f64;
            let var: f64 = vol_hist.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
                / vol_hist.len() as f64;
            let sigma = var.sqrt();
            if sigma > 0.0 && (tick.size - mean).abs() / sigma > self.spike_sigma * 2.0 {
                flags.push(DataQualityFlag::OutlierVolume);
            }
        }

        // Update state.
        let h = self.price_history.entry(tick.symbol.clone()).or_default();
        if h.len() >= self.z_score_window { h.pop_front(); }
        h.push_back(tick.price);

        let vh = self.volume_history.entry(tick.symbol.clone()).or_default();
        if vh.len() >= self.z_score_window { vh.pop_front(); }
        vh.push_back(tick.size);

        self.last_ts.insert(tick.symbol.clone(), tick.timestamp_ns);
        self.last_price.insert(tick.symbol.clone(), tick.price);

        if flags.is_empty() {
            vec![DataQualityFlag::Clean]
        } else {
            flags
        }
    }

    /// Check whether a price is a spike relative to `history`.
    ///
    /// Returns `Some(SuspiciousSpike { .. })` if the price deviates by more than
    /// `self.spike_sigma` standard deviations, `None` otherwise.
    pub fn check_spike(&self, price: f64, history: &VecDeque<f64>) -> Option<DataQualityFlag> {
        if history.len() < 5 {
            return None;
        }
        let mean = history.iter().sum::<f64>() / history.len() as f64;
        let var = history.iter().map(|p| (p - mean).powi(2)).sum::<f64>()
            / history.len() as f64;
        let sigma = var.sqrt();
        if sigma < 1e-12 {
            return None;
        }
        let deviation = (price - mean).abs() / sigma;
        if deviation > self.spike_sigma {
            Some(DataQualityFlag::SuspiciousSpike { deviation_sigma: deviation })
        } else {
            None
        }
    }

    /// Generate a quality report from a list of (tick, flags) pairs.
    pub fn generate_report(
        &self,
        flags: &[(RawTick, Vec<DataQualityFlag>)],
    ) -> QualityReport {
        if flags.is_empty() {
            return QualityReport::default();
        }

        let symbol = flags.first().map(|(t, _)| t.symbol.clone()).unwrap_or_default();
        let total_ticks = flags.len();
        let mut clean_ticks = 0usize;
        let mut flagged: HashMap<String, usize> = HashMap::new();

        for (_, tick_flags) in flags {
            if tick_flags.len() == 1 && tick_flags[0] == DataQualityFlag::Clean {
                clean_ticks += 1;
            } else {
                for flag in tick_flags {
                    let key = flag_name(flag);
                    *flagged.entry(key).or_default() += 1;
                }
            }
        }

        let data_quality_score = if total_ticks > 0 {
            clean_ticks as f64 / total_ticks as f64
        } else {
            1.0
        };

        QualityReport { symbol, total_ticks, clean_ticks, flagged, data_quality_score }
    }
}

fn flag_name(flag: &DataQualityFlag) -> String {
    match flag {
        DataQualityFlag::Clean => "Clean".to_string(),
        DataQualityFlag::StalePrice { .. } => "StalePrice".to_string(),
        DataQualityFlag::SuspiciousSpike { .. } => "SuspiciousSpike".to_string(),
        DataQualityFlag::MissingData { .. } => "MissingData".to_string(),
        DataQualityFlag::OutlierVolume => "OutlierVolume".to_string(),
        DataQualityFlag::DuplicateTick => "DuplicateTick".to_string(),
    }
}

// ---------------------------------------------------------------------------
// DataPipeline
// ---------------------------------------------------------------------------

/// End-to-end market data pipeline: normalize → quality check → build OHLCV bars.
pub struct DataPipeline {
    normalizer: TickNormalizer,
    quality_checker: QualityChecker,
    builders: HashMap<String, OhlcvBuilder>,
    /// Bar duration to use when creating new per-symbol builders.
    bar_duration_ns: u64,
    /// Per-symbol quality log: (tick, flags).
    quality_log: HashMap<String, Vec<(RawTick, Vec<DataQualityFlag>)>>,
}

impl DataPipeline {
    /// Create a new pipeline.
    ///
    /// `price_decimals`: price rounding precision.
    /// `bar_duration_ns`: OHLCV bar window size in nanoseconds.
    /// `spike_sigma`: sigma threshold for spike detection.
    /// `stale_threshold_ms`: milliseconds before a price is considered stale.
    /// `z_score_window`: rolling window size for z-score computation.
    pub fn new(
        price_decimals: u32,
        bar_duration_ns: u64,
        spike_sigma: f64,
        stale_threshold_ms: u64,
        z_score_window: usize,
    ) -> Self {
        Self {
            normalizer: TickNormalizer::new(price_decimals),
            quality_checker: QualityChecker::new(z_score_window, spike_sigma, stale_threshold_ms),
            builders: HashMap::new(),
            bar_duration_ns,
            quality_log: HashMap::new(),
        }
    }

    /// Process a single raw tick through the full pipeline.
    ///
    /// Returns `(quality_flags, Option<completed_bar>)`.
    pub fn process(&mut self, tick: RawTick) -> (Vec<DataQualityFlag>, Option<OhlcvBar>) {
        let tick = self.normalizer.normalize(tick);
        let flags = self.quality_checker.check(&tick);

        // Log tick + flags for reporting.
        self.quality_log
            .entry(tick.symbol.clone())
            .or_default()
            .push((tick.clone(), flags.clone()));

        // Only feed clean-ish ticks into bar builder (skip duplicate ticks).
        let is_duplicate = flags.contains(&DataQualityFlag::DuplicateTick);
        let bar = if !is_duplicate {
            let sym = tick.symbol.clone();
            let dur = self.bar_duration_ns;
            let builder = self.builders.entry(sym).or_insert_with(|| {
                OhlcvBuilder::new(tick.symbol.clone(), dur)
            });
            builder.push(&tick)
        } else {
            None
        };

        (flags, bar)
    }

    /// Generate per-symbol quality reports from the accumulated quality log.
    pub fn pipeline_stats(&self) -> HashMap<String, QualityReport> {
        self.quality_log.iter().map(|(sym, log)| {
            let report = self.quality_checker.generate_report(log);
            (sym.clone(), report)
        }).collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(sym: &str, ts_ns: u64, price: f64, size: f64) -> RawTick {
        RawTick {
            symbol: sym.to_string(),
            timestamp_ns: ts_ns,
            price,
            size,
            exchange: "NYSE".to_string(),
            conditions: vec![],
        }
    }

    #[test]
    fn tick_normalizer_rounds_price() {
        let norm = TickNormalizer::new(2);
        let tick = make_tick("AAPL", 0, 150.1234, 100.0);
        let normalized = norm.normalize(tick);
        assert!((normalized.price - 150.12).abs() < 1e-9);
    }

    #[test]
    fn tick_normalizer_filter_conditions() {
        let tick = RawTick {
            conditions: vec!["@".to_string(), "T".to_string()],
            ..make_tick("X", 0, 10.0, 1.0)
        };
        assert!(!TickNormalizer::filter_conditions(&tick, &["T"]));
        assert!(TickNormalizer::filter_conditions(&tick, &["Z"]));
    }

    #[test]
    fn deduplicate_removes_exact_dups() {
        let mut ticks = vec![
            make_tick("A", 1000, 10.0, 5.0),
            make_tick("A", 1000, 10.0, 5.0),
            make_tick("A", 2000, 10.1, 5.0),
        ];
        TickNormalizer::deduplicate(&mut ticks);
        assert_eq!(ticks.len(), 2);
    }

    #[test]
    fn ohlcv_builder_returns_bar_on_period_end() {
        let ns_per_min: u64 = 60 * 1_000_000_000;
        let mut builder = OhlcvBuilder::new("SPY", ns_per_min);
        assert!(builder.push(&make_tick("SPY", 0, 400.0, 100.0)).is_none());
        assert!(builder.push(&make_tick("SPY", 30_000_000_000, 401.0, 50.0)).is_none());
        let bar = builder.push(&make_tick("SPY", ns_per_min + 1, 402.0, 200.0));
        assert!(bar.is_some());
        let bar = bar.unwrap();
        assert!((bar.open - 400.0).abs() < 1e-9);
        assert!((bar.high - 401.0).abs() < 1e-9);
    }

    #[test]
    fn quality_checker_clean_tick() {
        let mut checker = QualityChecker::new(20, 3.0, 5000);
        let tick = make_tick("MSFT", 1_000_000_000, 300.0, 100.0);
        let flags = checker.check(&tick);
        assert_eq!(flags, vec![DataQualityFlag::Clean]);
    }

    #[test]
    fn quality_checker_spike_detection() {
        let mut checker = QualityChecker::new(20, 2.0, 5000);
        // Warm up with normal prices.
        for i in 0..10 {
            checker.check(&make_tick("X", i as u64 * 1_000_000_000, 100.0, 10.0));
        }
        // Feed a spike.
        let flags = checker.check(&make_tick("X", 11_000_000_000, 500.0, 10.0));
        let has_spike = flags.iter().any(|f| matches!(f, DataQualityFlag::SuspiciousSpike { .. }));
        assert!(has_spike);
    }

    #[test]
    fn pipeline_end_to_end() {
        let ns_per_min: u64 = 60 * 1_000_000_000;
        let mut pipeline = DataPipeline::new(2, ns_per_min, 3.0, 5000, 20);
        let (flags, bar) = pipeline.process(make_tick("AAPL", 0, 150.0, 100.0));
        assert_eq!(flags, vec![DataQualityFlag::Clean]);
        assert!(bar.is_none());
        // Push a tick past the bar window.
        let (_, bar2) = pipeline.process(make_tick("AAPL", ns_per_min + 1, 151.0, 50.0));
        assert!(bar2.is_some());
        let stats = pipeline.pipeline_stats();
        assert!(stats.contains_key("AAPL"));
    }
}
