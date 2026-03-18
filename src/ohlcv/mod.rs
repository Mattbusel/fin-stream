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
}

impl OhlcvAggregator {
    /// Create a new aggregator for `symbol` at `timeframe`.
    ///
    /// Returns an error if `timeframe.duration_ms()` is zero, which would make
    /// bar boundary alignment undefined.
    pub fn new(symbol: impl Into<String>, timeframe: Timeframe) -> Result<Self, StreamError> {
        let tf_dur = timeframe.duration_ms();
        if tf_dur == 0 {
            return Err(StreamError::ParseError {
                exchange: "OhlcvAggregator".into(),
                reason: "timeframe duration must be > 0".into(),
            });
        }
        Ok(Self {
            symbol: symbol.into(),
            timeframe,
            current_bar: None,
            emit_empty_bars: false,
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
    pub fn feed(&mut self, tick: &NormalizedTick) -> Result<Vec<OhlcvBar>, StreamError> {
        if tick.symbol != self.symbol {
            return Err(StreamError::ParseError {
                exchange: tick.exchange.to_string(),
                reason: format!(
                    "tick symbol '{}' does not match aggregator '{}'",
                    tick.symbol, self.symbol
                ),
            });
        }
        let bar_start = self.timeframe.bar_start_ms(tick.received_at_ms);
        let mut emitted: Vec<OhlcvBar> = Vec::new();

        if let Some(prev) = &self.current_bar {
            if prev.bar_start_ms != bar_start {
                // Complete the previous bar.
                let mut completed = prev.clone();
                completed.is_complete = true;
                let prev_close = completed.close;
                let prev_start = completed.bar_start_ms;
                emitted.push(completed);
                self.current_bar = None;

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
                        });
                        gap_start += dur;
                    }
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
                });
            }
        }
        Ok(emitted)
    }

    /// Current partial bar (if any).
    pub fn current_bar(&self) -> Option<&OhlcvBar> {
        self.current_bar.as_ref()
    }

    /// Flush the current partial bar as complete.
    pub fn flush(&mut self) -> Option<OhlcvBar> {
        let mut bar = self.current_bar.take()?;
        bar.is_complete = true;
        Some(bar)
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
        assert!(matches!(result, Err(StreamError::ParseError { .. })));
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
}
