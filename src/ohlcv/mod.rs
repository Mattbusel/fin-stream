//! Real-time tick-to-OHLCV aggregation at arbitrary timeframes.
//!
//! ## Responsibility
//! Aggregate incoming NormalizedTicks into OHLCV bars at configurable
//! timeframes. Handles bar completion detection and partial-bar access.
//!
//! ## Guarantees
//! - Non-panicking: all operations return Result or Option
//! - Thread-safe: OhlcvAggregator is Send + Sync

use crate::error::StreamError;
use crate::tick::NormalizedTick;
use rust_decimal::Decimal;

/// Supported bar timeframes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Timeframe {
    /// Bar duration measured in seconds.
    Seconds(u64),
    /// Bar duration measured in minutes.
    Minutes(u64),
    /// Bar duration measured in hours.
    Hours(u64),
}

impl Timeframe {
    /// Duration in milliseconds.
    pub fn duration_ms(self) -> u64 {
        match self {
            Timeframe::Seconds(s) => s * 1_000,
            Timeframe::Minutes(m) => m * 60 * 1_000,
            Timeframe::Hours(h) => h * 3600 * 1_000,
        }
    }

    /// Bar start timestamp for a given ms timestamp.
    pub fn bar_start_ms(self, ts_ms: u64) -> u64 {
        let dur = self.duration_ms();
        (ts_ms / dur) * dur
    }
}

impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Timeframe::Seconds(s) => write!(f, "{s}s"),
            Timeframe::Minutes(m) => write!(f, "{m}m"),
            Timeframe::Hours(h) => write!(f, "{h}h"),
        }
    }
}

/// A completed or partial OHLCV bar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OhlcvBar {
    /// Instrument symbol (e.g. `"BTC-USD"`).
    pub symbol: String,
    /// Timeframe of this bar.
    pub timeframe: Timeframe,
    /// UTC millisecond timestamp of the bar's open boundary.
    pub bar_start_ms: u64,
    /// Opening price (first tick's price in the bar window).
    pub open: Decimal,
    /// Highest price seen in the bar window.
    pub high: Decimal,
    /// Lowest price seen in the bar window.
    pub low: Decimal,
    /// Closing price (most recent tick's price in the bar window).
    pub close: Decimal,
    /// Total traded volume in this bar.
    pub volume: Decimal,
    /// Number of ticks contributing to this bar.
    pub trade_count: u64,
    /// `true` once the bar's time window has been closed by a tick in a later window.
    pub is_complete: bool,
    /// `true` if this bar was synthesized to fill a gap — no real ticks were received
    /// during its window. Gap-fill bars have `trade_count == 0` and all OHLC fields set
    /// to the last known close price. Callers may use this flag to filter synthetic bars
    /// out of indicator calculations or storage.
    pub is_gap_fill: bool,
}

impl OhlcvBar {
    /// Price range of the bar: `high - low`.
    pub fn range(&self) -> Decimal {
        self.high - self.low
    }

    /// Candle body size: `(close - open).abs()`.
    ///
    /// Direction-independent; use `close > open` to determine bullish/bearish.
    pub fn body(&self) -> Decimal {
        (self.close - self.open).abs()
    }
}

impl std::fmt::Display for OhlcvBar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} [{}/{}/{}/{}  v={}]",
            self.symbol, self.timeframe, self.open, self.high, self.low, self.close, self.volume
        )
    }
}

/// Aggregates ticks into OHLCV bars.
pub struct OhlcvAggregator {
    symbol: String,
    timeframe: Timeframe,
    current_bar: Option<OhlcvBar>,
    /// When true, `feed` returns synthetic zero-volume bars for any bar windows
    /// that were skipped between the previous tick and the current one.
    /// The synthetic bars use the last known close price for all OHLC fields.
    emit_empty_bars: bool,
    /// Total number of completed bars emitted by this aggregator.
    bars_emitted: u64,
}

impl OhlcvAggregator {
    /// Create a new aggregator for `symbol` at `timeframe`.
    ///
    /// Returns an error if `timeframe.duration_ms()` is zero, which would make
    /// bar boundary alignment undefined.
    pub fn new(symbol: impl Into<String>, timeframe: Timeframe) -> Result<Self, StreamError> {
        let tf_dur = timeframe.duration_ms();
        if tf_dur == 0 {
            return Err(StreamError::ConfigError {
                reason: "OhlcvAggregator timeframe duration must be > 0".into(),
            });
        }
        Ok(Self {
            symbol: symbol.into(),
            timeframe,
            current_bar: None,
            emit_empty_bars: false,
            bars_emitted: 0,
        })
    }

    /// Enable emission of synthetic zero-volume bars for skipped bar windows.
    pub fn with_emit_empty_bars(mut self, enabled: bool) -> Self {
        self.emit_empty_bars = enabled;
        self
    }

    /// Feed a tick. Returns completed bars (including any empty gap bars when
    /// `emit_empty_bars` is true). At most one real completed bar plus zero or
    /// more empty bars can be returned per call.
    ///
    /// Bar boundaries are aligned using the exchange-side timestamp
    /// (`exchange_ts_ms`) when available, falling back to the local system
    /// clock (`received_at_ms`). Using the exchange timestamp avoids
    /// misalignment caused by variable network latency.
    #[must_use = "completed bars are returned; ignoring them loses bar data"]
    #[inline]
    pub fn feed(&mut self, tick: &NormalizedTick) -> Result<Vec<OhlcvBar>, StreamError> {
        if tick.symbol != self.symbol {
            return Err(StreamError::AggregationError {
                reason: format!(
                    "tick symbol '{}' does not match aggregator '{}'",
                    tick.symbol, self.symbol
                ),
            });
        }

        // Prefer the authoritative exchange timestamp; fall back to local clock.
        let tick_ts = tick.exchange_ts_ms.unwrap_or(tick.received_at_ms);
        let bar_start = self.timeframe.bar_start_ms(tick_ts);
        let mut emitted: Vec<OhlcvBar> = Vec::new();

        // Check whether the incoming tick belongs to a new bar window.
        let bar_window_changed = self
            .current_bar
            .as_ref()
            .map(|b| b.bar_start_ms != bar_start)
            .unwrap_or(false);

        if bar_window_changed {
            // Take ownership — avoids cloning the current bar.
            let mut completed = self.current_bar.take().unwrap_or_else(|| unreachable!());
            completed.is_complete = true;
            let prev_close = completed.close;
            let prev_start = completed.bar_start_ms;
            emitted.push(completed);

            // Optionally fill any empty bar windows between prev_start and bar_start.
            if self.emit_empty_bars {
                let dur = self.timeframe.duration_ms();
                let mut gap_start = prev_start + dur;
                while gap_start < bar_start {
                    emitted.push(OhlcvBar {
                        symbol: self.symbol.clone(),
                        timeframe: self.timeframe,
                        bar_start_ms: gap_start,
                        open: prev_close,
                        high: prev_close,
                        low: prev_close,
                        close: prev_close,
                        volume: Decimal::ZERO,
                        trade_count: 0,
                        is_complete: true,
                        is_gap_fill: true,
                    });
                    gap_start += dur;
                }
            }
        }

        match &mut self.current_bar {
            Some(bar) => {
                if tick.price > bar.high {
                    bar.high = tick.price;
                }
                if tick.price < bar.low {
                    bar.low = tick.price;
                }
                bar.close = tick.price;
                bar.volume += tick.quantity;
                bar.trade_count += 1;
            }
            None => {
                self.current_bar = Some(OhlcvBar {
                    symbol: self.symbol.clone(),
                    timeframe: self.timeframe,
                    bar_start_ms: bar_start,
                    open: tick.price,
                    high: tick.price,
                    low: tick.price,
                    close: tick.price,
                    volume: tick.quantity,
                    trade_count: 1,
                    is_complete: false,
                    is_gap_fill: false,
                });
            }
        }
        self.bars_emitted += emitted.len() as u64;
        Ok(emitted)
    }

    /// Current partial bar (if any).
    pub fn current_bar(&self) -> Option<&OhlcvBar> {
        self.current_bar.as_ref()
    }

    /// Flush the current partial bar as complete.
    #[must_use = "the flushed bar is returned; ignoring it loses the partial bar"]
    pub fn flush(&mut self) -> Option<OhlcvBar> {
        let mut bar = self.current_bar.take()?;
        bar.is_complete = true;
        self.bars_emitted += 1;
        Some(bar)
    }

    /// Total number of completed bars emitted by this aggregator (via `feed` or `flush`).
    pub fn bar_count(&self) -> u64 {
        self.bars_emitted
    }

    /// Discard the in-progress bar and reset the bar counter to zero.
    ///
    /// Useful for backtesting rewind or when restarting aggregation from a
    /// new anchor point. Does not affect the aggregator's symbol or timeframe.
    pub fn reset(&mut self) {
        self.current_bar = None;
        self.bars_emitted = 0;
    }

    /// The symbol this aggregator tracks.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// The timeframe used for bar alignment.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::{Exchange, NormalizedTick, TradeSide};
    use rust_decimal_macros::dec;

    fn make_tick(symbol: &str, price: Decimal, qty: Decimal, ts_ms: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price,
            quantity: qty,
            side: Some(TradeSide::Buy),
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: ts_ms,
        }
    }

    fn make_tick_with_exchange_ts(
        symbol: &str,
        price: Decimal,
        qty: Decimal,
        exchange_ts_ms: u64,
        received_at_ms: u64,
    ) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price,
            quantity: qty,
            side: Some(TradeSide::Buy),
            trade_id: None,
            exchange_ts_ms: Some(exchange_ts_ms),
            received_at_ms,
        }
    }

    fn agg(symbol: &str, tf: Timeframe) -> OhlcvAggregator {
        OhlcvAggregator::new(symbol, tf).unwrap()
    }

    #[test]
    fn test_timeframe_seconds_duration_ms() {
        assert_eq!(Timeframe::Seconds(30).duration_ms(), 30_000);
    }

    #[test]
    fn test_timeframe_minutes_duration_ms() {
        assert_eq!(Timeframe::Minutes(5).duration_ms(), 300_000);
    }

    #[test]
    fn test_timeframe_hours_duration_ms() {
        assert_eq!(Timeframe::Hours(1).duration_ms(), 3_600_000);
    }

    #[test]
    fn test_timeframe_bar_start_ms_aligns() {
        let tf = Timeframe::Minutes(1);
        let ts = 61_500; // 1min 1.5sec
        assert_eq!(tf.bar_start_ms(ts), 60_000);
    }

    #[test]
    fn test_timeframe_display() {
        assert_eq!(Timeframe::Seconds(30).to_string(), "30s");
        assert_eq!(Timeframe::Minutes(5).to_string(), "5m");
        assert_eq!(Timeframe::Hours(4).to_string(), "4h");
    }

    #[test]
    fn test_ohlcv_aggregator_first_tick_sets_ohlcv() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        let tick = make_tick("BTC-USD", dec!(50000), dec!(1), 60_000);
        let result = agg.feed(&tick).unwrap();
        assert!(result.is_empty()); // no completed bar yet
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.open, dec!(50000));
        assert_eq!(bar.high, dec!(50000));
        assert_eq!(bar.low, dec!(50000));
        assert_eq!(bar.close, dec!(50000));
        assert_eq!(bar.volume, dec!(1));
        assert_eq!(bar.trade_count, 1);
    }

    #[test]
    fn test_ohlcv_aggregator_high_low_tracking() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_100))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(49500), dec!(1), 60_200))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.high, dec!(51000));
        assert_eq!(bar.low, dec!(49500));
        assert_eq!(bar.close, dec!(49500));
        assert_eq!(bar.trade_count, 3);
    }

    #[test]
    fn test_ohlcv_aggregator_bar_completes_on_new_window() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(2), 60_500))
            .unwrap();
        // Tick in next minute window closes previous bar
        let mut bars = agg
            .feed(&make_tick("BTC-USD", dec!(50200), dec!(1), 120_000))
            .unwrap();
        assert_eq!(bars.len(), 1);
        let bar = bars.remove(0);
        assert!(bar.is_complete);
        assert_eq!(bar.open, dec!(50000));
        assert_eq!(bar.close, dec!(50100));
        assert_eq!(bar.volume, dec!(3));
        assert_eq!(bar.bar_start_ms, 60_000);
    }

    #[test]
    fn test_ohlcv_aggregator_new_bar_started_after_completion() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50200), dec!(1), 120_000))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.open, dec!(50200));
        assert_eq!(bar.bar_start_ms, 120_000);
    }

    #[test]
    fn test_ohlcv_aggregator_flush_marks_complete() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let flushed = agg.flush().unwrap();
        assert!(flushed.is_complete);
        assert!(agg.current_bar().is_none());
    }

    #[test]
    fn test_ohlcv_aggregator_flush_empty_returns_none() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.flush().is_none());
    }

    #[test]
    fn test_ohlcv_aggregator_wrong_symbol_returns_error() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        let tick = make_tick("ETH-USD", dec!(3000), dec!(1), 60_000);
        let result = agg.feed(&tick);
        assert!(matches!(result, Err(StreamError::AggregationError { .. })));
    }

    #[test]
    fn test_ohlcv_aggregator_volume_accumulates() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1.5), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(2.5), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.volume, dec!(4));
    }

    #[test]
    fn test_ohlcv_bar_symbol_and_timeframe() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(5));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 300_000))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.symbol, "BTC-USD");
        assert_eq!(bar.timeframe, Timeframe::Minutes(5));
    }

    #[test]
    fn test_ohlcv_aggregator_symbol_accessor() {
        let agg = agg("ETH-USD", Timeframe::Hours(1));
        assert_eq!(agg.symbol(), "ETH-USD");
        assert_eq!(agg.timeframe(), Timeframe::Hours(1));
    }

    #[test]
    fn test_bar_aligned_by_exchange_ts_not_received_ts() {
        // exchange_ts_ms puts tick in minute 1 (60_000..120_000)
        // received_at_ms puts tick in minute 2 (120_000..180_000) due to latency
        // Bar should use the exchange timestamp.
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        let tick = make_tick_with_exchange_ts("BTC-USD", dec!(50000), dec!(1), 60_500, 120_100);
        agg.feed(&tick).unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.bar_start_ms, 60_000, "bar should use exchange_ts_ms");
    }

    #[test]
    fn test_bar_falls_back_to_received_ts_when_no_exchange_ts() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        let tick = make_tick("BTC-USD", dec!(50000), dec!(1), 75_000);
        agg.feed(&tick).unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.bar_start_ms, 60_000);
    }

    // --- emit_empty_bars tests ---

    #[test]
    fn test_emit_empty_bars_no_gap_no_empties() {
        // Consecutive bars — no gap — should not produce empty bars.
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
            .unwrap()
            .with_emit_empty_bars(true);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bars = agg
            .feed(&make_tick("BTC-USD", dec!(50100), dec!(1), 120_000))
            .unwrap();
        // Only the completed bar for the first minute; no empties.
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].bar_start_ms, 60_000);
        assert_eq!(bars[0].volume, dec!(1));
    }

    #[test]
    fn test_emit_empty_bars_two_skipped_windows() {
        // Gap of 3 minutes: complete bar at 60s, then two empty bars at 120s and 180s,
        // then the 240s tick starts a new bar.
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
            .unwrap()
            .with_emit_empty_bars(true);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bars = agg
            .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 240_000))
            .unwrap();
        // 1 real completed bar + 2 empty gap bars (120_000, 180_000)
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].bar_start_ms, 60_000);
        assert!(!bars[0].volume.is_zero()); // real bar
        assert_eq!(bars[1].bar_start_ms, 120_000);
        assert!(bars[1].volume.is_zero()); // empty
        assert_eq!(bars[1].trade_count, 0);
        assert_eq!(bars[1].open, dec!(50000)); // last close carried forward
        assert_eq!(bars[2].bar_start_ms, 180_000);
        assert!(bars[2].volume.is_zero()); // empty
    }

    #[test]
    fn test_emit_empty_bars_disabled_no_empties_on_gap() {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
            .unwrap()
            .with_emit_empty_bars(false);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bars = agg
            .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 240_000))
            .unwrap();
        assert_eq!(bars.len(), 1); // only real completed bar, no empties
    }

    #[test]
    fn test_emit_empty_bars_is_complete_true() {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
            .unwrap()
            .with_emit_empty_bars(true);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bars = agg
            .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 240_000))
            .unwrap();
        for bar in &bars {
            assert!(bar.is_complete, "all emitted bars must be marked complete");
        }
    }

    #[test]
    fn test_ohlcv_bar_display() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        let s = bar.to_string();
        assert!(s.contains("BTC-USD"));
        assert!(s.contains("1m"));
        assert!(s.contains("50000"));
    }

    #[test]
    fn test_bar_count_increments_on_feed() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert_eq!(agg.bar_count(), 0);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(1), 120_000))
            .unwrap();
        assert_eq!(agg.bar_count(), 1);
    }

    #[test]
    fn test_bar_count_increments_on_flush() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.flush().unwrap();
        assert_eq!(agg.bar_count(), 1);
    }

    #[test]
    fn test_ohlcv_bar_range() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_100))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(49500), dec!(1), 60_200))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.range(), dec!(1500)); // 51000 - 49500
    }

    #[test]
    fn test_ohlcv_bar_body_bullish() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50500), dec!(1), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        // open=50000, close=50500 → body = 500
        assert_eq!(bar.body(), dec!(500));
    }

    #[test]
    fn test_ohlcv_bar_body_bearish() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50500), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        // open=50500, close=50000 → body = 500 (abs)
        assert_eq!(bar.body(), dec!(500));
    }

    #[test]
    fn test_aggregator_reset_clears_bar_and_count() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(1), 120_000))
            .unwrap();
        assert_eq!(agg.bar_count(), 1);
        assert!(agg.current_bar().is_some());
        agg.reset();
        assert_eq!(agg.bar_count(), 0);
        assert!(agg.current_bar().is_none());
    }

    #[test]
    fn test_aggregator_reset_allows_fresh_start() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.reset();
        agg.feed(&make_tick("BTC-USD", dec!(99999), dec!(2), 60_000))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.open, dec!(99999));
    }
}
