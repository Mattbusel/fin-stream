//! Bar Aggregator
//!
//! Aggregates tick/trade events into OHLCV bars at configurable periods.
//! Supports three aggregation modes:
//!
//! | Mode | Description |
//! |------|-------------|
//! | [`AggregationMode::TimeBased`] | Close a bar every N seconds, aligned to epoch |
//! | [`AggregationMode::TickBased`] | Close a bar after every N trades |
//! | [`AggregationMode::VolumeBased`] | Close a bar when cumulative volume crosses a threshold |
//!
//! ## Usage
//!
//! ```rust
//! use fin_stream::aggregator::{AggregationMode, BarAggregator};
//! use fin_stream::protocol::TradeEvent;
//!
//! let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 100 });
//! let trade = TradeEvent {
//!     symbol: "BTCUSDT".to_owned(),
//!     price: 30_000.0,
//!     size: 0.5,
//!     side: None,
//!     timestamp_ns: 1_700_000_000_000_000_000,
//!     trade_id: None,
//!     exchange: "Binance".to_owned(),
//! };
//! let bar = agg.process_trade(&trade); // Some(BarEvent) when bar completes
//! drop(bar);
//! ```

/// Tick-to-Bar aggregator: time, tick, volume, and dollar bars from [`NormalizedTick`] streams.
///
/// [`NormalizedTick`]: crate::tick::NormalizedTick
pub mod bars;

use std::collections::HashMap;

use crate::protocol::{BarEvent, TradeEvent};

// ─── AggregationMode ─────────────────────────────────────────────────────────

/// Determines when an open bar is closed and emitted.
#[derive(Debug, Clone)]
pub enum AggregationMode {
    /// Close a bar at regular wall-clock intervals aligned to Unix epoch.
    ///
    /// For example, `period_secs: 60` produces bars whose open time is always
    /// a multiple of 60 s since epoch (00:01:00, 00:02:00, …).
    TimeBased {
        /// Bar duration in seconds. Must be > 0.
        period_secs: u32,
    },
    /// Close a bar after a fixed number of individual trade events.
    TickBased {
        /// Number of trades per bar. Must be > 0.
        tick_count: u32,
    },
    /// Close a bar when the cumulative traded volume crosses a threshold.
    VolumeBased {
        /// Volume threshold in native units. Must be > 0.
        volume_threshold: f64,
    },
}

// ─── BarBuilder ──────────────────────────────────────────────────────────────

/// Accumulates trades for a single symbol until a bar boundary is reached.
#[derive(Debug, Clone, Default)]
pub struct BarBuilder {
    /// Ticker symbol this builder is tracking.
    pub symbol: String,
    /// First price seen; `None` until at least one trade is processed.
    pub open: Option<f64>,
    /// Running maximum price.
    pub high: f64,
    /// Running minimum price.
    pub low: f64,
    /// Most recent price (becomes the close on completion).
    pub close: f64,
    /// Cumulative traded volume.
    pub volume: f64,
    /// Numerator of the VWAP calculation: Σ(price × size).
    pub vwap_numerator: f64,
    /// Number of trades accumulated in this bar.
    pub trade_count: u32,
    /// Nanosecond timestamp of the first trade in this bar.
    pub start_ns: u64,
}

impl BarBuilder {
    /// Creates a new, empty [`BarBuilder`] for `symbol`.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self { symbol: symbol.into(), ..Default::default() }
    }

    /// Updates the builder with a new trade's `price`, `size`, and timestamp.
    ///
    /// The first call to `update` sets the open price and bar start time.
    /// Subsequent calls update high/low/close and accumulate volume.
    pub fn update(&mut self, price: f64, size: f64, timestamp_ns: u64) {
        if self.open.is_none() {
            self.open = Some(price);
            self.high = price;
            self.low = price;
            self.start_ns = timestamp_ns;
        } else {
            if price > self.high {
                self.high = price;
            }
            if price < self.low {
                self.low = price;
            }
        }
        self.close = price;
        self.volume += size;
        self.vwap_numerator += price * size;
        self.trade_count += 1;
    }

    /// Returns `true` if no trades have been accumulated yet.
    pub fn is_empty(&self) -> bool {
        self.open.is_none()
    }

    /// Constructs a completed [`BarEvent`] from the accumulated state.
    ///
    /// `end_ns` is the timestamp at which the bar was closed (used only for
    /// informational purposes; `timestamp_ns` in the returned event is the bar
    /// *open* time, i.e. `self.start_ns`).
    ///
    /// The VWAP is `None` when `volume` is zero (to avoid division by zero).
    pub fn build(&self, _end_ns: u64, period_secs: u32, exchange: impl Into<String>) -> BarEvent {
        let open = self.open.unwrap_or(self.close);
        let vwap = if self.volume > 0.0 {
            Some(self.vwap_numerator / self.volume)
        } else {
            None
        };
        BarEvent {
            symbol: self.symbol.clone(),
            open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap,
            trade_count: Some(self.trade_count),
            timestamp_ns: self.start_ns,
            period_secs,
            exchange: exchange.into(),
        }
    }

    /// Resets the builder back to an empty state, ready for the next bar.
    fn reset(&mut self) {
        self.open = None;
        self.high = 0.0;
        self.low = 0.0;
        self.close = 0.0;
        self.volume = 0.0;
        self.vwap_numerator = 0.0;
        self.trade_count = 0;
        self.start_ns = 0;
    }
}

// ─── BarAggregator ────────────────────────────────────────────────────────────

/// Aggregates [`TradeEvent`]s into [`BarEvent`]s using a chosen [`AggregationMode`].
///
/// Maintains one [`BarBuilder`] per symbol.  When a bar boundary is detected,
/// [`process_trade`] emits the completed bar and starts a fresh one.
///
/// [`process_trade`]: BarAggregator::process_trade
pub struct BarAggregator {
    mode: AggregationMode,
    builders: HashMap<String, BarBuilder>,
    /// Per-symbol tick counters for [`AggregationMode::TickBased`].
    tick_counts: HashMap<String, u32>,
    /// Bars that have been completed but not yet consumed by the caller.
    ///
    /// [`flush`] drains all remaining open bars into this vec.
    ///
    /// [`flush`]: BarAggregator::flush
    completed_bars: Vec<BarEvent>,
}

impl BarAggregator {
    /// Creates a new [`BarAggregator`] using `mode` to determine bar boundaries.
    pub fn new(mode: AggregationMode) -> Self {
        Self {
            mode,
            builders: HashMap::new(),
            tick_counts: HashMap::new(),
            completed_bars: Vec::new(),
        }
    }

    /// Processes one trade event and returns a completed bar if a boundary was crossed.
    ///
    /// Returns `Some(BarEvent)` when the incoming trade causes the current bar to close.
    /// The bar is closed *before* the new trade is recorded, so the returned bar does
    /// not include the triggering trade — the trade opens the next bar instead.
    ///
    /// For [`AggregationMode::TimeBased`], the bar boundary is determined by the trade's
    /// timestamp, not by wall-clock time, so the aggregator works correctly for both
    /// live feeds and historical replay.
    pub fn process_trade(&mut self, trade: &TradeEvent) -> Option<BarEvent> {
        let price = trade.price;
        let size = trade.size;
        let ts = trade.timestamp_ns;
        let symbol = trade.symbol.as_str();

        let should_close = self.should_close_bar(symbol, price, size, ts);

        if should_close {
            // Compute period_secs before taking the mutable borrow on builders.
            let period_secs = self.period_secs_for_mode();
            // Close the current bar if it has any data.
            let completed = if let Some(builder) = self.builders.get_mut(symbol) {
                if !builder.is_empty() {
                    let bar = builder.build(ts, period_secs, &trade.exchange);
                    builder.reset();
                    Some(bar)
                } else {
                    None
                }
            } else {
                None
            };

            // Reset per-symbol counters.
            self.tick_counts.insert(symbol.to_owned(), 0);

            // Start the new bar with the current trade.
            let entry = self
                .builders
                .entry(symbol.to_owned())
                .or_insert_with(|| BarBuilder::new(symbol));
            entry.update(price, size, ts);
            *self.tick_counts.entry(symbol.to_owned()).or_insert(0) += 1;

            return completed;
        }

        // Normal accumulation path.
        let entry = self
            .builders
            .entry(symbol.to_owned())
            .or_insert_with(|| BarBuilder::new(symbol));
        entry.update(price, size, ts);

        let count = self.tick_counts.entry(symbol.to_owned()).or_insert(0);
        *count += 1;

        None
    }

    /// Force-closes all open bars and returns them.
    ///
    /// Useful at end-of-session to emit any partial bars that have accumulated
    /// data but have not yet reached their configured boundary.  Bars with no
    /// data (symbols that were registered but never received a trade) are
    /// silently skipped.
    pub fn flush(&mut self) -> Vec<BarEvent> {
        let period_secs = self.period_secs_for_mode();
        let mut result = Vec::new();
        for builder in self.builders.values_mut() {
            if !builder.is_empty() {
                let bar = builder.build(0, period_secs, "");
                builder.reset();
                result.push(bar);
            }
        }
        self.tick_counts.clear();
        result
    }

    /// Returns a sorted list of symbols that currently have at least one trade
    /// accumulated in an open bar.
    pub fn open_symbols(&self) -> Vec<String> {
        let mut symbols: Vec<String> = self
            .builders
            .iter()
            .filter(|(_, b)| !b.is_empty())
            .map(|(s, _)| s.clone())
            .collect();
        symbols.sort();
        symbols
    }

    // ── private helpers ──────────────────────────────────────────────────────

    /// Determines whether the current bar for `symbol` should be closed before
    /// recording the incoming trade.
    fn should_close_bar(&self, symbol: &str, _price: f64, size: f64, ts: u64) -> bool {
        match &self.mode {
            AggregationMode::TimeBased { period_secs } => {
                let period_ns = u64::from(*period_secs) * 1_000_000_000;
                if period_ns == 0 {
                    return false;
                }
                let new_bucket = ts / period_ns;
                if let Some(builder) = self.builders.get(symbol) {
                    if !builder.is_empty() {
                        let open_bucket = builder.start_ns / period_ns;
                        return new_bucket > open_bucket;
                    }
                }
                false
            }
            AggregationMode::TickBased { tick_count } => {
                if *tick_count == 0 {
                    return false;
                }
                let current = self.tick_counts.get(symbol).copied().unwrap_or(0);
                // Close when we've already accumulated tick_count trades.
                current >= *tick_count
            }
            AggregationMode::VolumeBased { volume_threshold } => {
                if *volume_threshold <= 0.0 {
                    return false;
                }
                if let Some(builder) = self.builders.get(symbol) {
                    if !builder.is_empty() {
                        return builder.volume + size > *volume_threshold;
                    }
                }
                false
            }
        }
    }

    /// Returns the `period_secs` value to embed in completed bars.
    ///
    /// For tick-based and volume-based modes this is 0 (no fixed wall-clock
    /// period applies).
    fn period_secs_for_mode(&self) -> u32 {
        match &self.mode {
            AggregationMode::TimeBased { period_secs } => *period_secs,
            AggregationMode::TickBased { .. } | AggregationMode::VolumeBased { .. } => 0,
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TradeSide, TradeEvent};

    fn make_trade(symbol: &str, price: f64, size: f64, ts_ns: u64) -> TradeEvent {
        TradeEvent {
            symbol: symbol.to_owned(),
            price,
            size,
            side: Some(TradeSide::Buy),
            timestamp_ns: ts_ns,
            trade_id: None,
            exchange: "TEST".to_owned(),
        }
    }

    // ── BarBuilder tests ──────────────────────────────────────────────────────

    #[test]
    fn bar_builder_is_empty_initially() {
        let b = BarBuilder::new("AAPL");
        assert!(b.is_empty());
    }

    #[test]
    fn bar_builder_tracks_ohlcv() {
        let mut b = BarBuilder::new("AAPL");
        b.update(100.0, 10.0, 1_000);
        b.update(110.0, 5.0, 2_000);
        b.update(95.0, 8.0, 3_000);
        b.update(105.0, 2.0, 4_000);

        assert!(!b.is_empty());
        let bar = b.build(5_000, 60, "NYSE");
        assert!((bar.open - 100.0).abs() < 1e-9);
        assert!((bar.high - 110.0).abs() < 1e-9);
        assert!((bar.low - 95.0).abs() < 1e-9);
        assert!((bar.close - 105.0).abs() < 1e-9);
        assert!((bar.volume - 25.0).abs() < 1e-9);
        assert_eq!(bar.trade_count, Some(4));
    }

    #[test]
    fn bar_builder_vwap_correct() {
        let mut b = BarBuilder::new("X");
        // 100 × 1 + 200 × 1 = 300, volume = 2, VWAP = 150
        b.update(100.0, 1.0, 0);
        b.update(200.0, 1.0, 1);
        let bar = b.build(2, 60, "X");
        let vwap = bar.vwap.expect("vwap present");
        assert!((vwap - 150.0).abs() < 1e-9);
    }

    #[test]
    fn bar_builder_reset_clears_state() {
        let mut b = BarBuilder::new("X");
        b.update(100.0, 1.0, 0);
        b.reset();
        assert!(b.is_empty());
        assert_eq!(b.trade_count, 0);
        assert!((b.volume).abs() < 1e-12);
    }

    // ── BarAggregator: tick-based ─────────────────────────────────────────────

    #[test]
    fn tick_based_emits_bar_after_n_ticks() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 3 });
        let t = |p: f64, ts: u64| make_trade("BTC", p, 1.0, ts);

        assert!(agg.process_trade(&t(100.0, 1)).is_none());
        assert!(agg.process_trade(&t(101.0, 2)).is_none());
        assert!(agg.process_trade(&t(102.0, 3)).is_none());
        // 4th trade crosses boundary — bar of the first 3 is emitted
        let bar = agg.process_trade(&t(103.0, 4)).expect("bar after 3 ticks");
        assert!((bar.open - 100.0).abs() < 1e-9);
        assert!((bar.close - 102.0).abs() < 1e-9);
        assert_eq!(bar.trade_count, Some(3));
    }

    #[test]
    fn tick_based_continues_after_close() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 2 });
        let t = |p: f64, ts: u64| make_trade("BTC", p, 1.0, ts);

        agg.process_trade(&t(10.0, 1));
        agg.process_trade(&t(11.0, 2));
        let bar1 = agg.process_trade(&t(12.0, 3)).expect("first bar");
        assert!((bar1.open - 10.0).abs() < 1e-9);

        agg.process_trade(&t(13.0, 4));
        let bar2 = agg.process_trade(&t(14.0, 5)).expect("second bar");
        assert!((bar2.open - 12.0).abs() < 1e-9);
    }

    // ── BarAggregator: time-based ─────────────────────────────────────────────

    #[test]
    fn time_based_emits_bar_on_new_period() {
        // 60-second bars
        let period_ns: u64 = 60 * 1_000_000_000;
        let mut agg = BarAggregator::new(AggregationMode::TimeBased { period_secs: 60 });

        // Trades within bar 0 (t=0..59s)
        let t0 = make_trade("SPY", 440.0, 100.0, 0);
        let t1 = make_trade("SPY", 441.0, 50.0, 30 * 1_000_000_000);
        assert!(agg.process_trade(&t0).is_none());
        assert!(agg.process_trade(&t1).is_none());

        // Trade in bar 1 (t=60s) — should close bar 0
        let t2 = make_trade("SPY", 442.0, 75.0, period_ns);
        let bar = agg.process_trade(&t2).expect("bar 0 closes");
        assert_eq!(bar.period_secs, 60);
        assert!((bar.open - 440.0).abs() < 1e-9);
        assert!((bar.close - 441.0).abs() < 1e-9);
        assert!((bar.volume - 150.0).abs() < 1e-9);
    }

    #[test]
    fn time_based_first_trade_does_not_close() {
        let mut agg = BarAggregator::new(AggregationMode::TimeBased { period_secs: 60 });
        let t = make_trade("X", 1.0, 1.0, 0);
        assert!(agg.process_trade(&t).is_none());
    }

    // ── BarAggregator: volume-based ───────────────────────────────────────────

    #[test]
    fn volume_based_emits_bar_when_threshold_crossed() {
        let mut agg = BarAggregator::new(AggregationMode::VolumeBased { volume_threshold: 10.0 });
        let t = |p: f64, s: f64, ts: u64| make_trade("ETH", p, s, ts);

        assert!(agg.process_trade(&t(1000.0, 4.0, 1)).is_none());
        assert!(agg.process_trade(&t(1001.0, 4.0, 2)).is_none());
        // Adding 4.0 more pushes total to 12 > 10 → bar closes
        let bar = agg.process_trade(&t(1002.0, 4.0, 3)).expect("volume bar");
        assert_eq!(bar.trade_count, Some(2)); // first two trades only
        assert!((bar.volume - 8.0).abs() < 1e-9);
    }

    // ── BarAggregator: multi-symbol ───────────────────────────────────────────

    #[test]
    fn independent_bars_per_symbol() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 2 });
        let btc = |p: f64, ts: u64| make_trade("BTC", p, 1.0, ts);
        let eth = |p: f64, ts: u64| make_trade("ETH", p, 1.0, ts);

        agg.process_trade(&btc(100.0, 1));
        agg.process_trade(&eth(10.0, 1));
        agg.process_trade(&btc(101.0, 2));
        // BTC boundary: 3rd BTC trade closes BTC bar
        let btc_bar = agg.process_trade(&btc(102.0, 3)).expect("BTC bar");
        assert_eq!(btc_bar.symbol, "BTC");

        // ETH still open
        assert!(agg.open_symbols().contains(&"ETH".to_owned()));
    }

    // ── BarAggregator: flush ──────────────────────────────────────────────────

    #[test]
    fn flush_emits_partial_bars() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 100 });
        agg.process_trade(&make_trade("MSFT", 300.0, 1.0, 1));
        agg.process_trade(&make_trade("MSFT", 301.0, 1.0, 2));

        let bars = agg.flush();
        assert_eq!(bars.len(), 1);
        assert!((bars[0].open - 300.0).abs() < 1e-9);
    }

    #[test]
    fn flush_clears_open_symbols() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 100 });
        agg.process_trade(&make_trade("NVDA", 400.0, 1.0, 1));
        assert!(!agg.open_symbols().is_empty());
        let _ = agg.flush();
        assert!(agg.open_symbols().is_empty());
    }

    #[test]
    fn open_symbols_sorted() {
        let mut agg = BarAggregator::new(AggregationMode::TickBased { tick_count: 100 });
        agg.process_trade(&make_trade("ZZZ", 1.0, 1.0, 1));
        agg.process_trade(&make_trade("AAA", 1.0, 1.0, 1));
        agg.process_trade(&make_trade("MMM", 1.0, 1.0, 1));
        let symbols = agg.open_symbols();
        assert_eq!(symbols, vec!["AAA", "MMM", "ZZZ"]);
    }
}

// ─── OHLCV Candle Aggregation (tick-stream, time-period based) ────────────────

/// Timeframe for candle aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFrame {
    /// N-second bars.
    Seconds(u64),
    /// N-minute bars.
    Minutes(u64),
    /// N-hour bars.
    Hours(u64),
}

impl TimeFrame {
    /// Timeframe duration in milliseconds.
    pub fn as_millis(&self) -> u64 {
        match self {
            TimeFrame::Seconds(n) => n * 1_000,
            TimeFrame::Minutes(n) => n * 60 * 1_000,
            TimeFrame::Hours(n) => n * 3_600 * 1_000,
        }
    }
}

/// A completed OHLCV candle with VWAP and metadata.
#[derive(Debug, Clone)]
pub struct OhlcvCandle {
    /// Instrument symbol.
    pub symbol: String,
    /// Opening price.
    pub open: f64,
    /// Highest price during the period.
    pub high: f64,
    /// Lowest price during the period.
    pub low: f64,
    /// Closing price.
    pub close: f64,
    /// Total traded volume.
    pub volume: f64,
    /// Volume-weighted average price: Σ(price × volume) / Σ(volume).
    pub vwap: f64,
    /// Number of ticks in this candle.
    pub tick_count: u32,
    /// Millisecond timestamp of the candle open (first tick).
    pub open_time_ms: u64,
    /// Millisecond timestamp of the candle close (last tick).
    pub close_time_ms: u64,
}

/// Accumulates ticks for a single symbol within one time period.
pub struct CandleBuilder {
    symbol: String,
    timeframe_ms: u64,
    open: Option<f64>,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    /// Σ(price × volume) for VWAP numerator.
    pv_sum: f64,
    tick_count: u32,
    open_time_ms: u64,
    close_time_ms: u64,
    /// Epoch-aligned period bucket of the current open.
    period_bucket: Option<u64>,
}

impl CandleBuilder {
    /// Create a new builder for `symbol` with `timeframe_ms` milliseconds per period.
    pub fn new(symbol: impl Into<String>, timeframe_ms: u64) -> Self {
        Self {
            symbol: symbol.into(),
            timeframe_ms,
            open: None,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0.0,
            pv_sum: 0.0,
            tick_count: 0,
            open_time_ms: 0,
            close_time_ms: 0,
            period_bucket: None,
        }
    }

    /// Returns `true` if no ticks have been accumulated.
    pub fn is_empty(&self) -> bool {
        self.open.is_none()
    }

    /// Process one tick. Returns a completed candle if the period rolled over.
    ///
    /// The triggering tick is NOT included in the returned candle — it opens
    /// the next period instead.
    pub fn push(&mut self, price: f64, volume: f64, timestamp_ms: u64) -> Option<OhlcvCandle> {
        let bucket = if self.timeframe_ms > 0 {
            timestamp_ms / self.timeframe_ms
        } else {
            0
        };

        let mut completed: Option<OhlcvCandle> = None;

        // Detect period rollover
        if let Some(open_bucket) = self.period_bucket {
            if bucket > open_bucket && self.open.is_some() {
                completed = Some(self.build());
                self.reset();
            }
        }

        // Accumulate
        if self.open.is_none() {
            self.open = Some(price);
            self.high = price;
            self.low = price;
            self.open_time_ms = timestamp_ms;
            self.period_bucket = Some(bucket);
        } else {
            if price > self.high { self.high = price; }
            if price < self.low { self.low = price; }
        }
        self.close = price;
        self.close_time_ms = timestamp_ms;
        self.volume += volume;
        self.pv_sum += price * volume;
        self.tick_count += 1;

        completed
    }

    /// Force-flush any in-progress candle. Returns `None` if empty.
    pub fn flush(&mut self) -> Option<OhlcvCandle> {
        if self.open.is_none() {
            return None;
        }
        let candle = self.build();
        self.reset();
        Some(candle)
    }

    fn build(&self) -> OhlcvCandle {
        let vwap = if self.volume > 0.0 {
            self.pv_sum / self.volume
        } else {
            self.open.unwrap_or(self.close)
        };
        OhlcvCandle {
            symbol: self.symbol.clone(),
            open: self.open.unwrap_or(self.close),
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap,
            tick_count: self.tick_count,
            open_time_ms: self.open_time_ms,
            close_time_ms: self.close_time_ms,
        }
    }

    fn reset(&mut self) {
        self.open = None;
        self.high = f64::NEG_INFINITY;
        self.low = f64::INFINITY;
        self.close = 0.0;
        self.volume = 0.0;
        self.pv_sum = 0.0;
        self.tick_count = 0;
        self.open_time_ms = 0;
        self.close_time_ms = 0;
        self.period_bucket = None;
    }
}

/// Multi-symbol OHLCV candle aggregator driven by raw (price, volume, ts) ticks.
pub struct TickAggregator {
    timeframe: TimeFrame,
    builders: HashMap<String, CandleBuilder>,
    completed: Vec<OhlcvCandle>,
}

impl TickAggregator {
    /// Create a new aggregator with the given timeframe.
    pub fn new(timeframe: TimeFrame) -> Self {
        Self {
            timeframe,
            builders: HashMap::new(),
            completed: Vec::new(),
        }
    }

    /// Process a single tick.
    ///
    /// Returns `Some(OhlcvCandle)` when the tick causes the previous period's
    /// candle to close.
    pub fn process_tick(
        &mut self,
        symbol: &str,
        price: f64,
        volume: f64,
        timestamp_ms: u64,
    ) -> Option<OhlcvCandle> {
        let tf_ms = self.timeframe.as_millis();
        let builder = self
            .builders
            .entry(symbol.to_owned())
            .or_insert_with(|| CandleBuilder::new(symbol, tf_ms));
        let completed = builder.push(price, volume, timestamp_ms);
        if let Some(ref c) = completed {
            self.completed.push(c.clone());
        }
        completed
    }

    /// Force-close all in-progress candles and return them.
    pub fn flush_all(&mut self) -> Vec<OhlcvCandle> {
        let mut result = Vec::new();
        for builder in self.builders.values_mut() {
            if let Some(candle) = builder.flush() {
                result.push(candle);
            }
        }
        result
    }

    /// List symbols that currently have an open candle.
    pub fn active_symbols(&self) -> Vec<String> {
        self.builders
            .iter()
            .filter(|(_, b)| !b.is_empty())
            .map(|(s, _)| s.clone())
            .collect()
    }
}

// ─── TickAggregator tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tick_aggregator_tests {
    use super::*;

    #[test]
    fn single_symbol_ohlcv_within_period() {
        // Three ticks within the first 1-minute period
        let mut agg = TickAggregator::new(TimeFrame::Minutes(1));
        // ts_ms: 0, 10_000, 59_000 — all in bucket 0 (0..60_000)
        assert!(agg.process_tick("BTC", 100.0, 1.0, 0).is_none());
        assert!(agg.process_tick("BTC", 110.0, 2.0, 10_000).is_none());
        assert!(agg.process_tick("BTC", 95.0, 3.0, 59_000).is_none());

        // Flush to get partial candle
        let candles = agg.flush_all();
        assert_eq!(candles.len(), 1);
        let c = &candles[0];
        assert_eq!(c.symbol, "BTC");
        assert!((c.open - 100.0).abs() < 1e-9, "open={}", c.open);
        assert!((c.high - 110.0).abs() < 1e-9, "high={}", c.high);
        assert!((c.low - 95.0).abs() < 1e-9, "low={}", c.low);
        assert!((c.close - 95.0).abs() < 1e-9, "close={}", c.close);
        assert!((c.volume - 6.0).abs() < 1e-9, "volume={}", c.volume);
        assert_eq!(c.tick_count, 3);
    }

    #[test]
    fn vwap_calculation() {
        // VWAP = (100*1 + 200*3) / 4 = 700/4 = 175
        let mut agg = TickAggregator::new(TimeFrame::Minutes(1));
        agg.process_tick("X", 100.0, 1.0, 0);
        agg.process_tick("X", 200.0, 3.0, 1_000);
        let candles = agg.flush_all();
        let vwap = candles[0].vwap;
        assert!((vwap - 175.0).abs() < 1e-9, "VWAP should be 175, got {vwap}");
    }

    #[test]
    fn period_rollover_produces_candle() {
        let mut agg = TickAggregator::new(TimeFrame::Seconds(60));
        // Period 0: [0, 60_000)
        agg.process_tick("ETH", 1000.0, 1.0, 0);
        agg.process_tick("ETH", 1010.0, 1.0, 30_000);
        // Period 1: tick at 60_000 rolls over
        let completed = agg.process_tick("ETH", 1020.0, 1.0, 60_000);
        assert!(completed.is_some(), "period rollover should produce a candle");
        let c = completed.unwrap();
        assert!((c.open - 1000.0).abs() < 1e-9);
        assert!((c.close - 1010.0).abs() < 1e-9);
        assert_eq!(c.tick_count, 2);
    }

    #[test]
    fn high_low_tracking() {
        let mut agg = TickAggregator::new(TimeFrame::Hours(1));
        let prices = [100.0, 150.0, 80.0, 120.0, 90.0];
        for (i, &p) in prices.iter().enumerate() {
            agg.process_tick("AAPL", p, 1.0, i as u64 * 1_000);
        }
        let candles = agg.flush_all();
        assert!((candles[0].high - 150.0).abs() < 1e-9);
        assert!((candles[0].low - 80.0).abs() < 1e-9);
    }

    #[test]
    fn timeframe_as_millis() {
        assert_eq!(TimeFrame::Seconds(30).as_millis(), 30_000);
        assert_eq!(TimeFrame::Minutes(5).as_millis(), 300_000);
        assert_eq!(TimeFrame::Hours(1).as_millis(), 3_600_000);
    }
}
