//! # Module: candle
//!
//! OHLCV candle builder supporting multiple timeframes with streaming tick input.
//!
//! ## Key Types
//!
//! - [`Candle`] — completed OHLCV bar with VWAP and trade count
//! - [`Timeframe`] — time bucket definition (second, minute, hour, day)
//! - [`CandleBuilder`] — stateful builder for a single candle
//! - [`MultiTimeframeCandler`] — parallel builders for multiple timeframes
//! - [`CandleIndicators`] — stateless candle analysis helpers

use std::collections::HashMap;

// ─────────────────────────────────────────
//  Candle
// ─────────────────────────────────────────

/// A completed OHLCV candlestick bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Candle {
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
    /// Volume-weighted average price.
    pub vwap: f64,
    /// Number of individual trades.
    pub num_trades: u64,
    /// Unix millisecond timestamp of the bar open.
    pub open_time_ms: u64,
    /// Unix millisecond timestamp of the bar close.
    pub close_time_ms: u64,
}

// ─────────────────────────────────────────
//  Timeframe
// ─────────────────────────────────────────

/// Candle time bucket granularity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Timeframe {
    /// N-second bars (e.g. `Second(5)` for 5-second bars).
    Second(u32),
    /// N-minute bars.
    Minute(u32),
    /// N-hour bars.
    Hour(u32),
    /// Daily bars.
    Day,
}

impl Timeframe {
    /// Duration of this timeframe in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match self {
            Timeframe::Second(n) => *n as u64 * 1_000,
            Timeframe::Minute(n) => *n as u64 * 60_000,
            Timeframe::Hour(n) => *n as u64 * 3_600_000,
            Timeframe::Day => 86_400_000,
        }
    }
}

// ─────────────────────────────────────────
//  CandleBuilder  (internal state)
// ─────────────────────────────────────────

struct CandleState {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    pv_sum: f64, // sum(price * volume) for VWAP
    num_trades: u64,
    open_time_ms: u64,
    bar_end_ms: u64,
}

impl CandleState {
    fn new(price: f64, volume: f64, timestamp_ms: u64, duration_ms: u64) -> Self {
        // Align bar start to a multiple of duration_ms.
        let bar_start = (timestamp_ms / duration_ms) * duration_ms;
        let bar_end = bar_start + duration_ms;
        Self {
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
            pv_sum: price * volume,
            num_trades: 1,
            open_time_ms: bar_start,
            bar_end_ms: bar_end,
        }
    }

    fn update(&mut self, price: f64, volume: f64) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume += volume;
        self.pv_sum += price * volume;
        self.num_trades += 1;
    }

    fn to_candle(&self) -> Candle {
        let vwap = if self.volume == 0.0 {
            (self.open + self.close) / 2.0
        } else {
            self.pv_sum / self.volume
        };
        Candle {
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap,
            num_trades: self.num_trades,
            open_time_ms: self.open_time_ms,
            close_time_ms: self.bar_end_ms,
        }
    }
}

/// Stateful builder that accumulates ticks into a single [`Candle`].
pub struct CandleBuilder {
    timeframe: Timeframe,
    state: Option<CandleState>,
}

impl CandleBuilder {
    /// Create a new builder for the given timeframe.
    pub fn new(timeframe: Timeframe) -> Self {
        Self { timeframe, state: None }
    }

    /// Add a tick. Returns a completed [`Candle`] when the current bar's
    /// timeframe window elapses (and the tick opens a new bar).
    pub fn add_tick(
        &mut self,
        price: f64,
        volume: f64,
        timestamp_ms: u64,
    ) -> Option<Candle> {
        let dur = self.timeframe.duration_ms();

        match &mut self.state {
            None => {
                self.state = Some(CandleState::new(price, volume, timestamp_ms, dur));
                None
            }
            Some(s) => {
                if timestamp_ms < s.bar_end_ms {
                    // Same bar — accumulate.
                    s.update(price, volume);
                    None
                } else {
                    // Bar boundary crossed — emit current candle, start new.
                    let completed = s.to_candle();
                    self.state = Some(CandleState::new(price, volume, timestamp_ms, dur));
                    Some(completed)
                }
            }
        }
    }

    /// Return the current partial (incomplete) candle, if any tick has been seen.
    pub fn current_partial(&self) -> Option<Candle> {
        self.state.as_ref().map(|s| s.to_candle())
    }

    /// Return `true` if the current bar's window has elapsed as of `now_ms`.
    pub fn is_complete(&self, now_ms: u64) -> bool {
        self.state
            .as_ref()
            .map_or(false, |s| now_ms >= s.bar_end_ms)
    }
}

// ─────────────────────────────────────────
//  MultiTimeframeCandler
// ─────────────────────────────────────────

/// Maintains independent [`CandleBuilder`]s for multiple timeframes and
/// stores completed candles in a per-timeframe history.
pub struct MultiTimeframeCandler {
    builders: HashMap<Timeframe, CandleBuilder>,
    history: HashMap<Timeframe, Vec<Candle>>,
}

impl MultiTimeframeCandler {
    /// Create a candler tracking the given set of timeframes.
    pub fn new(timeframes: Vec<Timeframe>) -> Self {
        let mut builders = HashMap::new();
        let mut history = HashMap::new();
        for tf in timeframes {
            builders.insert(tf.clone(), CandleBuilder::new(tf.clone()));
            history.insert(tf, Vec::new());
        }
        Self { builders, history }
    }

    /// Feed a tick to all registered timeframe builders.
    ///
    /// Returns a list of `(timeframe, completed_candle)` pairs for every bar
    /// that closed as a result of this tick.
    pub fn add_tick(
        &mut self,
        price: f64,
        volume: f64,
        timestamp_ms: u64,
    ) -> Vec<(Timeframe, Candle)> {
        let mut completed = Vec::new();
        for (tf, builder) in &mut self.builders {
            if let Some(candle) = builder.add_tick(price, volume, timestamp_ms) {
                completed.push((tf.clone(), candle.clone()));
                self.history
                    .entry(tf.clone())
                    .or_default()
                    .push(candle);
            }
        }
        completed
    }

    /// Historical completed candles for `timeframe`.
    pub fn get_candles(&self, timeframe: &Timeframe) -> &[Candle] {
        self.history
            .get(timeframe)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Most recently completed candle for `timeframe`.
    pub fn latest(&self, timeframe: &Timeframe) -> Option<&Candle> {
        self.history.get(timeframe)?.last()
    }
}

// ─────────────────────────────────────────
//  CandleIndicators
// ─────────────────────────────────────────

/// Stateless candlestick analysis functions.
pub struct CandleIndicators;

impl CandleIndicators {
    /// Typical price: `(high + low + close) / 3`.
    pub fn typical_price(c: &Candle) -> f64 {
        (c.high + c.low + c.close) / 3.0
    }

    /// True range: `max(high - low, |high - prev_close|, |low - prev_close|)`.
    pub fn true_range(c: &Candle, prev_close: f64) -> f64 {
        let hl = c.high - c.low;
        let hc = (c.high - prev_close).abs();
        let lc = (c.low - prev_close).abs();
        hl.max(hc).max(lc)
    }

    /// Body percentage: `|close - open| / (high - low)`.
    ///
    /// Returns `0.0` for doji where `high == low`.
    pub fn body_pct(c: &Candle) -> f64 {
        let range = c.high - c.low;
        if range == 0.0 {
            return 0.0;
        }
        (c.close - c.open).abs() / range
    }

    /// Returns `true` if the candle body is smaller than `threshold` fraction
    /// of the total range (classic doji definition).
    pub fn is_doji(c: &Candle, threshold: f64) -> bool {
        Self::body_pct(c) <= threshold
    }

    /// Detect bullish (+1) or bearish (-1) engulfing pattern.
    ///
    /// Returns `None` if neither pattern is detected.
    pub fn is_engulfing(prev: &Candle, curr: &Candle) -> Option<i8> {
        let prev_bull = prev.close > prev.open;
        let curr_bull = curr.close > curr.open;

        // Bullish engulfing: prev bearish, curr bullish, curr body engulfs prev body
        if !prev_bull
            && curr_bull
            && curr.open < prev.close
            && curr.close > prev.open
        {
            return Some(1);
        }

        // Bearish engulfing: prev bullish, curr bearish, curr body engulfs prev body
        if prev_bull
            && !curr_bull
            && curr.open > prev.close
            && curr.close < prev.open
        {
            return Some(-1);
        }

        None
    }
}

// ─────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_candle(open: f64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            open,
            high,
            low,
            close,
            volume: 100.0,
            vwap: (high + low + close) / 3.0,
            num_trades: 5,
            open_time_ms: 0,
            close_time_ms: 60_000,
        }
    }

    // ── Timeframe ─────────────────────────────────────────────────────────

    #[test]
    fn timeframe_duration_second() {
        assert_eq!(Timeframe::Second(5).duration_ms(), 5_000);
    }

    #[test]
    fn timeframe_duration_minute() {
        assert_eq!(Timeframe::Minute(1).duration_ms(), 60_000);
    }

    #[test]
    fn timeframe_duration_hour() {
        assert_eq!(Timeframe::Hour(1).duration_ms(), 3_600_000);
    }

    #[test]
    fn timeframe_duration_day() {
        assert_eq!(Timeframe::Day.duration_ms(), 86_400_000);
    }

    // ── CandleBuilder ─────────────────────────────────────────────────────

    #[test]
    fn builder_no_candle_within_window() {
        let mut b = CandleBuilder::new(Timeframe::Minute(1));
        assert!(b.add_tick(100.0, 10.0, 0).is_none());
        assert!(b.add_tick(101.0, 5.0, 30_000).is_none());
    }

    #[test]
    fn builder_emits_candle_on_new_bar() {
        let mut b = CandleBuilder::new(Timeframe::Minute(1));
        b.add_tick(100.0, 10.0, 0);
        b.add_tick(105.0, 5.0, 30_000);
        let completed = b.add_tick(102.0, 8.0, 60_001);
        let c = completed.expect("should complete a candle");
        assert!((c.open - 100.0).abs() < 1e-9);
        assert!((c.high - 105.0).abs() < 1e-9);
        assert!((c.low - 100.0).abs() < 1e-9);
        assert!((c.close - 105.0).abs() < 1e-9);
        assert!((c.volume - 15.0).abs() < 1e-9);
        assert_eq!(c.num_trades, 2);
    }

    #[test]
    fn builder_vwap_correct() {
        let mut b = CandleBuilder::new(Timeframe::Minute(1));
        b.add_tick(100.0, 10.0, 0);
        b.add_tick(200.0, 10.0, 30_000);
        // Trigger close
        let c = b.add_tick(150.0, 5.0, 60_001).expect("candle");
        // VWAP = (100*10 + 200*10) / 20 = 3000/20 = 150
        assert!((c.vwap - 150.0).abs() < 1e-9);
    }

    #[test]
    fn builder_current_partial() {
        let mut b = CandleBuilder::new(Timeframe::Minute(1));
        assert!(b.current_partial().is_none());
        b.add_tick(100.0, 1.0, 0);
        let p = b.current_partial().unwrap();
        assert!((p.open - 100.0).abs() < 1e-9);
    }

    #[test]
    fn builder_is_complete() {
        let mut b = CandleBuilder::new(Timeframe::Minute(1));
        b.add_tick(100.0, 1.0, 0);
        assert!(!b.is_complete(30_000));
        assert!(b.is_complete(60_001));
    }

    // ── MultiTimeframeCandler ─────────────────────────────────────────────

    #[test]
    fn multi_timeframe_no_history_initially() {
        let m = MultiTimeframeCandler::new(vec![Timeframe::Minute(1), Timeframe::Hour(1)]);
        assert!(m.get_candles(&Timeframe::Minute(1)).is_empty());
        assert!(m.latest(&Timeframe::Minute(1)).is_none());
    }

    #[test]
    fn multi_timeframe_completes_candle() {
        let mut m = MultiTimeframeCandler::new(vec![Timeframe::Minute(1)]);
        m.add_tick(100.0, 10.0, 0);
        m.add_tick(105.0, 5.0, 30_000);
        let completed = m.add_tick(102.0, 8.0, 60_001);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, Timeframe::Minute(1));
        assert_eq!(m.get_candles(&Timeframe::Minute(1)).len(), 1);
        assert!(m.latest(&Timeframe::Minute(1)).is_some());
    }

    // ── CandleIndicators ──────────────────────────────────────────────────

    #[test]
    fn typical_price() {
        let c = simple_candle(100.0, 110.0, 90.0, 105.0);
        let tp = CandleIndicators::typical_price(&c);
        assert!((tp - (110.0 + 90.0 + 105.0) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn true_range_simple() {
        let c = simple_candle(100.0, 115.0, 95.0, 110.0);
        let tr = CandleIndicators::true_range(&c, 105.0);
        // hl=20, hc=10, lc=10 → 20
        assert!((tr - 20.0).abs() < 1e-9);
    }

    #[test]
    fn true_range_gap_up() {
        let c = simple_candle(120.0, 125.0, 118.0, 122.0);
        // prev_close=100; hc=25, lc=18, hl=7 → 25
        let tr = CandleIndicators::true_range(&c, 100.0);
        assert!((tr - 25.0).abs() < 1e-9);
    }

    #[test]
    fn body_pct_normal() {
        let c = simple_candle(100.0, 110.0, 90.0, 110.0); // body=10, range=20 → 0.5
        let bp = CandleIndicators::body_pct(&c);
        assert!((bp - 0.5).abs() < 1e-9);
    }

    #[test]
    fn body_pct_doji_zero_range() {
        let c = simple_candle(100.0, 100.0, 100.0, 100.0);
        assert_eq!(CandleIndicators::body_pct(&c), 0.0);
    }

    #[test]
    fn is_doji_true() {
        let c = simple_candle(100.0, 101.0, 99.0, 100.1); // body=0.1, range=2 → 0.05
        assert!(CandleIndicators::is_doji(&c, 0.1));
    }

    #[test]
    fn is_doji_false() {
        let c = simple_candle(100.0, 110.0, 90.0, 110.0); // body pct = 0.5
        assert!(!CandleIndicators::is_doji(&c, 0.1));
    }

    #[test]
    fn engulfing_bullish() {
        let prev = simple_candle(105.0, 106.0, 99.0, 100.0); // bearish
        let curr = simple_candle(98.0, 112.0, 98.0, 108.0);  // bullish, engulfs
        assert_eq!(CandleIndicators::is_engulfing(&prev, &curr), Some(1));
    }

    #[test]
    fn engulfing_bearish() {
        let prev = simple_candle(100.0, 108.0, 100.0, 105.0); // bullish
        let curr = simple_candle(107.0, 110.0, 95.0, 98.0);   // bearish, engulfs
        assert_eq!(CandleIndicators::is_engulfing(&prev, &curr), Some(-1));
    }

    #[test]
    fn engulfing_none() {
        let prev = simple_candle(100.0, 105.0, 99.0, 104.0);
        let curr = simple_candle(103.0, 106.0, 102.0, 105.0);
        assert_eq!(CandleIndicators::is_engulfing(&prev, &curr), None);
    }
}
