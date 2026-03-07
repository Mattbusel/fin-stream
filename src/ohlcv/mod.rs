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
    Seconds(u64),
    Minutes(u64),
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
    pub symbol: String,
    pub timeframe: Timeframe,
    pub bar_start_ms: u64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub trade_count: u64,
    pub is_complete: bool,
}

/// Aggregates ticks into OHLCV bars.
pub struct OhlcvAggregator {
    symbol: String,
    timeframe: Timeframe,
    current_bar: Option<OhlcvBar>,
}

impl OhlcvAggregator {
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
        })
    }

    /// Feed a tick. Returns a completed bar if this tick closed the previous bar.
    pub fn feed(&mut self, tick: &NormalizedTick) -> Result<Option<OhlcvBar>, StreamError> {
        if tick.symbol != self.symbol {
            return Err(StreamError::ParseError {
                exchange: tick.exchange.to_string(),
                reason: format!("tick symbol '{}' does not match aggregator '{}'", tick.symbol, self.symbol),
            });
        }
        let bar_start = self.timeframe.bar_start_ms(tick.received_at_ms);
        let completed = match &self.current_bar {
            Some(bar) if bar.bar_start_ms != bar_start => {
                // New bar window — complete the old one
                let mut completed = bar.clone();
                completed.is_complete = true;
                Some(completed)
            }
            _ => None,
        };
        if completed.is_some() {
            self.current_bar = None;
        }
        match &mut self.current_bar {
            Some(bar) => {
                if tick.price > bar.high { bar.high = tick.price; }
                if tick.price < bar.low { bar.low = tick.price; }
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
        Ok(completed)
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

    pub fn symbol(&self) -> &str { &self.symbol }
    pub fn timeframe(&self) -> Timeframe { self.timeframe }
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
        assert!(result.is_none()); // no completed bar yet
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
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_100)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(49500), dec!(1), 60_200)).unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.high, dec!(51000));
        assert_eq!(bar.low, dec!(49500));
        assert_eq!(bar.close, dec!(49500));
        assert_eq!(bar.trade_count, 3);
    }

    #[test]
    fn test_ohlcv_aggregator_bar_completes_on_new_window() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(2), 60_500)).unwrap();
        // Tick in next minute window closes previous bar
        let completed = agg.feed(&make_tick("BTC-USD", dec!(50200), dec!(1), 120_000)).unwrap();
        let bar = completed.unwrap();
        assert!(bar.is_complete);
        assert_eq!(bar.open, dec!(50000));
        assert_eq!(bar.close, dec!(50100));
        assert_eq!(bar.volume, dec!(3));
        assert_eq!(bar.bar_start_ms, 60_000);
    }

    #[test]
    fn test_ohlcv_aggregator_new_bar_started_after_completion() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50200), dec!(1), 120_000)).unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.open, dec!(50200));
        assert_eq!(bar.bar_start_ms, 120_000);
    }

    #[test]
    fn test_ohlcv_aggregator_flush_marks_complete() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
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
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1.5), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(2.5), 60_100)).unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.volume, dec!(4));
    }

    #[test]
    fn test_ohlcv_bar_symbol_and_timeframe() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(5));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 300_000)).unwrap();
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
}
