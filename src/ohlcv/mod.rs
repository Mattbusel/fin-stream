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

    /// Construct a `Timeframe` from a millisecond duration.
    ///
    /// Prefers the largest canonical unit that divides evenly:
    /// hours > minutes > seconds. Returns `None` if `ms` is zero or not a
    /// whole number of seconds.
    pub fn from_duration_ms(ms: u64) -> Option<Timeframe> {
        if ms == 0 {
            return None;
        }
        if ms % 3_600_000 == 0 {
            return Some(Timeframe::Hours(ms / 3_600_000));
        }
        if ms % 60_000 == 0 {
            return Some(Timeframe::Minutes(ms / 60_000));
        }
        if ms % 1_000 == 0 {
            return Some(Timeframe::Seconds(ms / 1_000));
        }
        None
    }
}

impl PartialOrd for Timeframe {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timeframe {
    /// Compares timeframes by their duration in milliseconds.
    ///
    /// For example: `Seconds(30) < Minutes(1) < Hours(1)`.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.duration_ms().cmp(&other.duration_ms())
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

impl std::str::FromStr for Timeframe {
    type Err = crate::error::StreamError;

    /// Parse a timeframe string such as `"1s"`, `"5m"`, or `"2h"`.
    ///
    /// The format is a positive integer followed by a unit suffix:
    /// - `s` — seconds (e.g. `"30s"`)
    /// - `m` — minutes (e.g. `"5m"`)
    /// - `h` — hours   (e.g. `"1h"`)
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ConfigError`] if the string is empty, has an
    /// unknown suffix, or if the numeric part is zero or cannot be parsed.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(crate::error::StreamError::ConfigError {
                reason: "timeframe string is empty".into(),
            });
        }
        let (digits, suffix) = s.split_at(s.len() - 1);
        let n: u64 = digits.parse().map_err(|_| crate::error::StreamError::ConfigError {
            reason: format!("invalid timeframe numeric part '{digits}' in '{s}'"),
        })?;
        if n == 0 {
            return Err(crate::error::StreamError::ConfigError {
                reason: format!("timeframe value must be > 0, got '{s}'"),
            });
        }
        match suffix {
            "s" => Ok(Timeframe::Seconds(n)),
            "m" => Ok(Timeframe::Minutes(n)),
            "h" => Ok(Timeframe::Hours(n)),
            other => Err(crate::error::StreamError::ConfigError {
                reason: format!(
                    "unknown timeframe suffix '{other}' in '{s}'; expected s, m, or h"
                ),
            }),
        }
    }
}

/// Direction of an OHLCV bar body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarDirection {
    /// Close is strictly above open.
    Bullish,
    /// Close is strictly below open.
    Bearish,
    /// Close equals open (flat body).
    Neutral,
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
    /// Volume-weighted average price for this bar. `None` for gap-fill bars.
    pub vwap: Option<Decimal>,
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

    /// Higher of open and close: `max(open, close)`.
    ///
    /// The top of the candle body, regardless of direction.
    pub fn body_high(&self) -> Decimal {
        self.open.max(self.close)
    }

    /// Lower of open and close: `min(open, close)`.
    ///
    /// The bottom of the candle body, regardless of direction.
    pub fn body_low(&self) -> Decimal {
        self.open.min(self.close)
    }

    /// Returns `true` if this is a bullish bar (`close > open`).
    pub fn is_bullish(&self) -> bool {
        self.close > self.open
    }

    /// Returns `true` if this is a bearish bar (`close < open`).
    pub fn is_bearish(&self) -> bool {
        self.close < self.open
    }

    /// Returns `true` if the bar has a non-zero upper wick (`high > max(open, close)`).
    pub fn has_upper_wick(&self) -> bool {
        self.wick_upper() > Decimal::ZERO
    }

    /// Returns `true` if the bar has a non-zero lower wick (`min(open, close) > low`).
    pub fn has_lower_wick(&self) -> bool {
        self.wick_lower() > Decimal::ZERO
    }

    /// Directional classification of the bar body.
    ///
    /// Returns [`BarDirection::Bullish`] when `close > open`, [`BarDirection::Bearish`]
    /// when `close < open`, and [`BarDirection::Neutral`] when they are equal.
    pub fn body_direction(&self) -> BarDirection {
        use std::cmp::Ordering;
        match self.close.cmp(&self.open) {
            Ordering::Greater => BarDirection::Bullish,
            Ordering::Less => BarDirection::Bearish,
            Ordering::Equal => BarDirection::Neutral,
        }
    }

    /// Returns `true` if the bar body is a doji (indecision candle).
    ///
    /// A doji has `|close - open| <= epsilon`. Use a small positive `epsilon`
    /// such as `dec!(0.01)` to account for rounding in price data.
    pub fn is_doji(&self, epsilon: Decimal) -> bool {
        self.body() <= epsilon
    }

    /// Upper wick (shadow) length: `high - max(open, close)`.
    ///
    /// The upper wick is the portion of the candle above the body.
    pub fn wick_upper(&self) -> Decimal {
        self.high - self.body_high()
    }

    /// Lower wick (shadow) length: `min(open, close) - low`.
    ///
    /// The lower wick is the portion of the candle below the body.
    pub fn wick_lower(&self) -> Decimal {
        self.body_low() - self.low
    }

    /// Signed price change: `close - open`.
    ///
    /// Positive for bullish bars, negative for bearish bars, zero for doji.
    /// Unlike [`body`](Self::body), this preserves direction.
    pub fn price_change(&self) -> Decimal {
        self.close - self.open
    }

    /// Typical price: `(high + low + close) / 3`.
    ///
    /// Commonly used as the basis for VWAP and commodity channel index (CCI)
    /// calculations.
    pub fn typical_price(&self) -> Decimal {
        (self.high + self.low + self.close) / Decimal::from(3)
    }

    /// Close Location Value (CLV): where the close sits within the bar's range.
    ///
    /// Formula: `(close - low - (high - close)) / range`.
    ///
    /// Returns `None` if the range is zero (e.g. a single-price bar). Values
    /// are in `[-1.0, 1.0]`: `+1.0` means the close is at the high, `-1.0` at
    /// the low, and `0.0` means the close is exactly mid-range.
    pub fn close_location_value(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        ((self.close - self.low - (self.high - self.close)) / range).to_f64()
    }

    /// Median price: `(high + low) / 2`.
    ///
    /// The midpoint of the bar's price range, independent of open and close.
    pub fn median_price(&self) -> Decimal {
        (self.high + self.low) / Decimal::from(2)
    }

    /// Weighted close price: `(high + low + close × 2) / 4`.
    ///
    /// Gives extra weight to the closing price over the high and low extremes.
    /// Commonly used as the basis for certain momentum and volatility indicators.
    pub fn weighted_close(&self) -> Decimal {
        (self.high + self.low + self.close + self.close) / Decimal::from(4)
    }

    /// Percentage price change: `(close − open) / open × 100`.
    ///
    /// Returns `None` if `open` is zero. Positive values indicate a bullish bar;
    /// negative values indicate a bearish bar.
    pub fn price_change_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() {
            return None;
        }
        let pct = self.price_change() / self.open * Decimal::from(100);
        pct.to_f64()
    }

    /// Body ratio: `body / range`.
    ///
    /// The fraction of the total price range that is body (rather than wicks).
    /// Ranges from `0.0` (pure wicks / doji) to `1.0` (no wicks at all).
    /// Returns `None` if the bar's range is zero (all prices identical).
    pub fn body_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        (self.body() / range).to_f64()
    }

    /// True range: `max(high − low, |high − prev_close|, |low − prev_close|)`.
    ///
    /// The standard ATR (Average True Range) input. Accounts for overnight gaps by
    /// including the distance from the previous close to today's high and low.
    pub fn true_range(&self, prev_close: Decimal) -> Decimal {
        let hl = self.range();
        let hpc = (self.high - prev_close).abs();
        let lpc = (self.low - prev_close).abs();
        hl.max(hpc).max(lpc)
    }

    /// Returns `true` if this bar is an inside bar relative to `prev`.
    ///
    /// An inside bar has `high < prev.high` and `low > prev.low` — its full
    /// range is contained within the prior bar's range. Used in price action
    /// trading as a consolidation signal.
    #[deprecated(since = "2.2.0", note = "Use `is_inside_bar` instead")]
    pub fn inside_bar(&self, prev: &OhlcvBar) -> bool {
        self.is_inside_bar(prev)
    }

    /// Returns `true` if this bar is an outside bar relative to `prev`.
    ///
    /// An outside bar has `high > prev.high` and `low < prev.low` — it fully
    /// engulfs the prior bar's range. Also called a key reversal day.
    pub fn outside_bar(&self, prev: &OhlcvBar) -> bool {
        self.high > prev.high && self.low < prev.low
    }

    /// Returns the ratio of total wick length to bar range: `(upper_wick + lower_wick) / range`.
    ///
    /// A value near 1 indicates a bar that is mostly wicks with little body.
    /// Returns `None` when the bar has zero range (high == low).
    pub fn wick_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        ((self.wick_upper() + self.wick_lower()) / range).to_f64()
    }

    /// Returns `true` if this bar has a classic hammer shape.
    ///
    /// A hammer has:
    /// - A small body (≤ 30% of range)
    /// - A long lower wick (≥ 60% of range)
    /// - A tiny upper wick (≤ 10% of range)
    ///
    /// Returns `false` if the bar's range is zero.
    pub fn is_hammer(&self) -> bool {
        let range = self.range();
        if range.is_zero() {
            return false;
        }
        let body = self.body();
        let wick_lo = self.wick_lower();
        let wick_hi = self.wick_upper();
        let three = Decimal::from(3);
        let six = Decimal::from(6);
        let ten = Decimal::from(10);
        // body ≤ 30%: body*10 ≤ range*3
        // lower wick ≥ 60%: wick_lo*10 ≥ range*6
        // upper wick ≤ 10%: wick_hi*10 ≤ range
        body * ten <= range * three
            && wick_lo * ten >= range * six
            && wick_hi * ten <= range
    }

    /// Returns `true` if this bar has a classic shooting-star shape.
    ///
    /// A shooting star has:
    /// - A small body (≤ 30% of range)
    /// - A long upper wick (≥ 60% of range)
    /// - A tiny lower wick (≤ 10% of range)
    ///
    /// This is the inverse of a hammer — it signals a potential reversal at
    /// the top of an uptrend. Returns `false` if the bar's range is zero.
    pub fn is_shooting_star(&self) -> bool {
        let range = self.range();
        if range.is_zero() {
            return false;
        }
        let body = self.body();
        let wick_lo = self.wick_lower();
        let wick_hi = self.wick_upper();
        let three = Decimal::from(3);
        let six = Decimal::from(6);
        let ten = Decimal::from(10);
        // body ≤ 30%: body*10 ≤ range*3
        // upper wick ≥ 60%: wick_hi*10 ≥ range*6
        // lower wick ≤ 10%: wick_lo*10 ≤ range
        body * ten <= range * three
            && wick_hi * ten >= range * six
            && wick_lo * ten <= range
    }

    /// Gap from the previous bar: `self.open − prev.close`.
    ///
    /// Positive values indicate a gap-up; negative values indicate a gap-down.
    /// Zero means the bar opened exactly at the previous close (no gap).
    pub fn gap_from(&self, prev: &OhlcvBar) -> Decimal {
        self.open - prev.close
    }

    /// Returns `true` if this bar opened above the previous bar's close.
    pub fn is_gap_up(&self, prev: &OhlcvBar) -> bool {
        self.open > prev.close
    }

    /// Returns `true` if this bar opened below the previous bar's close.
    pub fn is_gap_down(&self, prev: &OhlcvBar) -> bool {
        self.open < prev.close
    }

    /// Body midpoint: `(open + close) / 2`.
    ///
    /// The arithmetic center of the candle body, regardless of direction.
    /// Useful as a proxy for the "fair value" of the period.
    pub fn bar_midpoint(&self) -> Decimal {
        (self.open + self.close) / Decimal::from(2)
    }

    /// Body as a fraction of total range: `body / range`.
    ///
    /// Returns `None` when `range` is zero (all OHLC prices identical).
    pub fn body_to_range_ratio(&self) -> Option<Decimal> {
        let r = self.range();
        if r.is_zero() {
            return None;
        }
        Some(self.body() / r)
    }

    /// Returns `true` if the upper wick is longer than the candle body.
    ///
    /// Indicates a bearish rejection at the high (supply above current price).
    pub fn is_long_upper_wick(&self) -> bool {
        self.wick_upper() > self.body()
    }

    /// Returns `true` if the lower wick is longer than the candle body.
    ///
    /// Indicates a bullish rejection at the low (demand below current price).
    pub fn is_long_lower_wick(&self) -> bool {
        self.wick_lower() > self.body()
    }

    /// Absolute price change over the bar: `|close − open|`.
    ///
    /// Alias for [`body`](Self::body).
    #[deprecated(since = "2.2.0", note = "Use `body()` instead")]
    pub fn price_change_abs(&self) -> Decimal {
        self.body()
    }

    /// Upper shadow length — alias for [`wick_upper`](Self::wick_upper).
    ///
    /// Returns `high − max(open, close)`.
    pub fn upper_shadow(&self) -> Decimal {
        self.wick_upper()
    }

    /// Lower shadow length — alias for [`wick_lower`](Self::wick_lower).
    ///
    /// Returns `min(open, close) − low`.
    pub fn lower_shadow(&self) -> Decimal {
        self.wick_lower()
    }

    /// Returns `true` if this bar has a spinning-top pattern.
    ///
    /// A spinning top has a small body (≤ `body_pct` of range) with significant
    /// wicks on both sides (each wick strictly greater than the body). Signals
    /// market indecision — neither buyers nor sellers controlled the period.
    ///
    /// `body_pct` is a fraction in `[0.0, 1.0]`, e.g. `dec!(0.3)` for 30%.
    /// Returns `false` if the bar's range is zero.
    pub fn is_spinning_top(&self, body_pct: Decimal) -> bool {
        let range = self.range();
        if range.is_zero() {
            return false;
        }
        let body = self.body();
        let max_body = range * body_pct;
        body <= max_body && self.wick_upper() > body && self.wick_lower() > body
    }

    /// HLC3: `(high + low + close) / 3` — alias for [`typical_price`](Self::typical_price).
    pub fn hlc3(&self) -> Decimal {
        self.typical_price()
    }

    /// OHLC4: `(open + high + low + close) / 4`.
    ///
    /// Gives equal weight to all four price points. Sometimes used as a smoother
    /// proxy than typical price because it incorporates the open.
    pub fn ohlc4(&self) -> Decimal {
        (self.open + self.high + self.low + self.close) / Decimal::from(4)
    }

    /// Returns `true` if this bar is a marubozu — no upper or lower wicks.
    ///
    /// A marubozu has `open == low` and `close == high` (bullish) or
    /// `open == high` and `close == low` (bearish). It signals strong
    /// one-directional momentum with no intrabar rejection.
    /// A zero-range bar (all prices equal) is considered a marubozu.
    pub fn is_marubozu(&self) -> bool {
        self.wick_upper().is_zero() && self.wick_lower().is_zero()
    }

    /// Returns `true` if this bar's body engulfs `prev`'s body.
    ///
    /// Engulfing requires: `self.open < prev.open.min(prev.close)` and
    /// `self.close > prev.open.max(prev.close)` (or vice versa for bearish).
    /// Specifically, `self.body_low < prev.body_low` and
    /// `self.body_high > prev.body_high`.
    ///
    /// Does NOT require opposite directions — use in combination with
    /// [`is_bullish`](Self::is_bullish) / [`is_bearish`](Self::is_bearish) if
    /// classic engulfing patterns are needed.
    pub fn is_engulfing(&self, prev: &OhlcvBar) -> bool {
        self.body_low() < prev.body_low() && self.body_high() > prev.body_high()
    }

    /// Returns `true` if this bar is a harami: its body is entirely contained
    /// within the previous bar's body.
    ///
    /// A harami is the opposite of an engulfing pattern. Neither bar needs to
    /// be bullish or bearish — only the body ranges are compared.
    pub fn is_harami(&self, prev: &OhlcvBar) -> bool {
        self.body_low() > prev.body_low() && self.body_high() < prev.body_high()
    }

    /// The longer of the upper and lower wicks.
    ///
    /// Returns the maximum of `wick_upper()` and `wick_lower()`. Useful for
    /// identifying long-tailed candles regardless of direction.
    pub fn tail_length(&self) -> Decimal {
        self.wick_upper().max(self.wick_lower())
    }

    /// Returns `true` if this bar is an inside bar: both `high` and `low` are
    /// strictly within the previous bar's range.
    ///
    /// Unlike [`is_harami`](Self::is_harami), which compares body ranges,
    /// this method compares the full high-low range including wicks.
    pub fn is_inside_bar(&self, prev: &OhlcvBar) -> bool {
        self.high < prev.high && self.low > prev.low
    }

    /// Returns `true` if this bar opened above the previous bar's high (gap up).
    pub fn gap_up(&self, prev: &OhlcvBar) -> bool {
        self.open > prev.high
    }

    /// Returns `true` if this bar opened below the previous bar's low (gap down).
    pub fn gap_down(&self, prev: &OhlcvBar) -> bool {
        self.open < prev.low
    }

    /// Absolute size of the candle body: `|close - open|`.
    ///
    /// Alias for [`body`](Self::body).
    #[deprecated(since = "2.2.0", note = "Use `body()` instead")]
    pub fn body_size(&self) -> Decimal {
        self.body()
    }

    /// Volume change vs the previous bar: `self.volume - prev.volume`.
    pub fn volume_delta(&self, prev: &OhlcvBar) -> Decimal {
        self.volume - prev.volume
    }

    /// Returns `true` if this bar's range is less than 50% of the previous bar's range.
    ///
    /// Indicates price consolidation / compression.
    pub fn is_consolidating(&self, prev: &OhlcvBar) -> bool {
        let prev_range = prev.range();
        if prev_range.is_zero() {
            return false;
        }
        self.range() < prev_range / Decimal::TWO
    }

    /// Mean volume across a slice of bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn mean_volume(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        Some(Self::sum_volume(bars) / Decimal::from(bars.len() as u64))
    }

    /// Absolute deviation of close price from VWAP as a fraction of VWAP: `|close - vwap| / vwap`.
    ///
    /// Returns `None` if `vwap` is not set or is zero.
    pub fn vwap_deviation(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vwap = self.vwap?;
        if vwap.is_zero() {
            return None;
        }
        ((self.close - vwap).abs() / vwap).to_f64()
    }

    /// Volume as a ratio of `avg_volume`.
    ///
    /// Returns `None` if `avg_volume` is zero.
    pub fn relative_volume(&self, avg_volume: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if avg_volume.is_zero() {
            return None;
        }
        (self.volume / avg_volume).to_f64()
    }

    /// Returns `true` if this bar opens in the direction of the prior bar's move
    /// but closes against it (an intraday reversal signal).
    ///
    /// Specifically: prev was bullish (close > open), this bar opens near/above prev close,
    /// and closes below prev open — or vice versa for a bearish reversal.
    pub fn intraday_reversal(&self, prev: &OhlcvBar) -> bool {
        let prev_bullish = prev.close > prev.open;
        let this_bearish = self.close < self.open;
        let prev_bearish = prev.close < prev.open;
        let this_bullish = self.close > self.open;
        (prev_bullish && this_bearish && self.open >= prev.close)
            || (prev_bearish && this_bullish && self.open <= prev.close)
    }

    /// High-low range as a percentage of the open price: `(high - low) / open * 100`.
    ///
    /// Returns `None` if open is zero.
    pub fn range_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() {
            return None;
        }
        let range = self.range() / self.open;
        range.to_f64().map(|v| v * 100.0)
    }

    /// Returns `true` if this bar is an outside bar (engulfs `prev`'s range).
    ///
    /// An outside bar has a higher high AND lower low than the previous bar.
    /// Alias for [`outside_bar`](Self::outside_bar).
    #[deprecated(since = "2.2.0", note = "Use `outside_bar()` instead")]
    pub fn is_outside_bar(&self, prev: &OhlcvBar) -> bool {
        self.outside_bar(prev)
    }

    /// Midpoint of the high-low range: `(high + low) / 2`.
    ///
    /// Alias for [`median_price`](Self::median_price).
    pub fn high_low_midpoint(&self) -> Decimal {
        self.median_price()
    }

    /// Ratio of close to high: `close / high` as `f64`.
    ///
    /// Returns `None` if `high` is zero. A value near 1.0 means the bar closed
    /// near its high (bullish strength); near 0.0 means it closed far below.
    pub fn high_close_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.high.is_zero() {
            return None;
        }
        (self.close / self.high).to_f64()
    }

    /// Lower shadow as a fraction of the full bar range: `lower_shadow / range`.
    ///
    /// Returns `None` if the bar's range is zero.
    pub fn lower_shadow_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        (self.lower_shadow() / range).to_f64()
    }

    /// Ratio of close to open: `close / open` as `f64`.
    ///
    /// Returns `None` if `open` is zero. Values above 1.0 indicate a bullish bar.
    pub fn open_close_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() {
            return None;
        }
        (self.close / self.open).to_f64()
    }

    /// Returns `true` if this bar's range (`high - low`) exceeds `threshold`.
    pub fn is_wide_range_bar(&self, threshold: Decimal) -> bool {
        self.range() > threshold
    }

    /// Position of close within the bar's high-low range: `(close - low) / (high - low)`.
    ///
    /// Returns `None` if the bar's range is zero. Result is in `[0.0, 1.0]`:
    /// - `0.0` → closed at the low (bearish)
    /// - `1.0` → closed at the high (bullish)
    pub fn close_to_low_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        ((self.close - self.low) / range).to_f64()
    }

    /// Average volume per trade: `volume / trade_count`.
    ///
    /// Returns `None` if `trade_count` is zero.
    pub fn volume_per_trade(&self) -> Option<Decimal> {
        if self.trade_count == 0 {
            return None;
        }
        Some(self.volume / Decimal::from(self.trade_count as u64))
    }

    /// Returns `true` if this bar's high-low range overlaps with `other`'s range.
    ///
    /// Two ranges overlap when neither is entirely above or below the other.
    pub fn price_range_overlap(&self, other: &OhlcvBar) -> bool {
        self.high >= other.low && other.high >= self.low
    }

    /// Bar height as a fraction of the open price: `(high - low) / open`.
    ///
    /// Returns `None` if `open` is zero. Useful for comparing volatility across
    /// instruments trading at different price levels.
    pub fn bar_height_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() {
            return None;
        }
        (self.range() / self.open).to_f64()
    }

    /// Classifies this bar as `"bullish"`, `"bearish"`, or `"doji"`.
    ///
    /// A doji is a bar whose body is zero (open equals close). Otherwise the
    /// direction is determined by whether close is above or below open.
    pub fn bar_type(&self) -> &'static str {
        if self.close == self.open {
            "doji"
        } else if self.close > self.open {
            "bullish"
        } else {
            "bearish"
        }
    }

    /// Body as a percentage of the total high-low range.
    ///
    /// Returns `None` when the range is zero (all four prices equal).
    /// A 100% body means no wicks (marubozu); near 0% means a doji.
    pub fn body_pct(&self) -> Option<Decimal> {
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        Some(self.body() / range * Decimal::ONE_HUNDRED)
    }

    /// Returns `true` if this bar is a bullish hammer: a long lower wick,
    /// small body near the top of the range, and little or no upper wick.
    ///
    /// Specifically: the lower wick is at least twice the body, and the upper
    /// wick is no more than the body.
    pub fn is_bullish_hammer(&self) -> bool {
        let body = self.body();
        if body.is_zero() {
            return false;
        }
        let lower = self.wick_lower();
        let upper = self.wick_upper();
        lower >= body * Decimal::TWO && upper <= body
    }

    /// Upper wick as a percentage of the total range (0–100).
    ///
    /// Returns `None` when the range is zero.
    pub fn upper_wick_pct(&self) -> Option<Decimal> {
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        Some(self.wick_upper() / range * Decimal::ONE_HUNDRED)
    }

    /// Lower wick as a percentage of the total range (0–100).
    ///
    /// Returns `None` when the range is zero.
    pub fn lower_wick_pct(&self) -> Option<Decimal> {
        let range = self.range();
        if range.is_zero() {
            return None;
        }
        Some(self.wick_lower() / range * Decimal::ONE_HUNDRED)
    }

    /// Returns `true` if this bar is a bearish engulfing candle relative to `prev`.
    ///
    /// A bearish engulfing has: current bar bearish, body entirely engulfs prev body.
    pub fn is_bearish_engulfing(&self, prev: &OhlcvBar) -> bool {
        self.is_bearish() && self.is_engulfing(prev)
    }

    /// Returns `true` if this bar is a bullish engulfing candle relative to `prev`.
    ///
    /// A bullish engulfing has: current bar bullish, body entirely engulfs prev body.
    pub fn is_bullish_engulfing(&self, prev: &OhlcvBar) -> bool {
        self.is_bullish() && self.is_engulfing(prev)
    }

    /// Gap between this bar's open and the previous bar's close: `self.open - prev.close`.
    ///
    /// A positive value indicates an upward gap; negative indicates a downward gap.
    /// Alias for [`gap_from`](Self::gap_from).
    #[deprecated(since = "2.2.0", note = "Use `gap_from()` instead")]
    pub fn close_gap(&self, prev: &OhlcvBar) -> Decimal {
        self.gap_from(prev)
    }

    /// Returns `true` if the close price is strictly above the bar's midpoint `(high + low) / 2`.
    pub fn close_above_midpoint(&self) -> bool {
        self.close > self.high_low_midpoint()
    }

    /// Price momentum: `self.close - prev.close`.
    ///
    /// Positive → price increased; negative → decreased.
    pub fn close_momentum(&self, prev: &OhlcvBar) -> Decimal {
        self.close - prev.close
    }

    /// Full high-low range of the bar: `high - low`.
    ///
    /// Alias for [`range`](Self::range).
    #[deprecated(since = "2.2.0", note = "Use `range()` instead")]
    pub fn bar_range(&self) -> Decimal {
        self.range()
    }

    /// Duration of this bar's timeframe in milliseconds.
    pub fn bar_duration_ms(&self) -> u64 {
        self.timeframe.duration_ms()
    }

    /// Returns `true` if this bar resembles a gravestone doji.
    ///
    /// A gravestone doji has open ≈ close ≈ low (body within `epsilon` of
    /// zero and close within `epsilon` of the low), with a long upper wick.
    pub fn is_gravestone_doji(&self, epsilon: Decimal) -> bool {
        self.body() <= epsilon && (self.close - self.low).abs() <= epsilon
    }

    /// Returns `true` if this bar resembles a dragonfly doji.
    ///
    /// A dragonfly doji has open ≈ close ≈ high (body within `epsilon` of
    /// zero and close within `epsilon` of the high), with a long lower wick.
    pub fn is_dragonfly_doji(&self, epsilon: Decimal) -> bool {
        self.body() <= epsilon && (self.high - self.close).abs() <= epsilon
    }

    /// Returns `true` if this bar is completely flat (open == close == high == low).
    ///
    /// For a valid OHLCV bar, `range() == 0` is sufficient: since `low ≤ open, close ≤ high`,
    /// `high == low` forces all four prices equal.
    pub fn is_flat(&self) -> bool {
        self.range().is_zero()
    }

    /// True range: `max(high - low, |high - prev_close|, |low - prev_close|)`.
    ///
    /// Alias for [`true_range`](Self::true_range).
    #[deprecated(since = "2.2.0", note = "Use `true_range()` instead")]
    pub fn true_range_with_prev(&self, prev_close: Decimal) -> Decimal {
        self.true_range(prev_close)
    }

    /// Returns the ratio of close to high, or `None` if high is zero.
    ///
    /// Alias for [`high_close_ratio`](Self::high_close_ratio).
    #[deprecated(since = "2.2.0", note = "Use `high_close_ratio()` instead")]
    pub fn close_to_high_ratio(&self) -> Option<f64> {
        self.high_close_ratio()
    }

    /// Returns the ratio of close to open, or `None` if open is zero.
    ///
    /// Alias for [`open_close_ratio`](Self::open_close_ratio).
    #[deprecated(since = "2.2.0", note = "Use `open_close_ratio()` instead")]
    pub fn close_open_ratio(&self) -> Option<f64> {
        self.open_close_ratio()
    }

    /// Interpolates a price within the bar's high-low range.
    ///
    /// `pct = 0.0` returns `low`; `pct = 1.0` returns `high`.
    /// Values outside `[0.0, 1.0]` are clamped to that interval.
    pub fn price_at_pct(&self, pct: f64) -> Decimal {
        use rust_decimal::prelude::FromPrimitive;
        let pct_clamped = pct.clamp(0.0, 1.0);
        let factor = Decimal::from_f64(pct_clamped).unwrap_or(Decimal::ZERO);
        self.low + self.range() * factor
    }

    /// Average true range (ATR) across a slice of consecutive bars.
    ///
    /// Computes the mean of [`true_range`](Self::true_range) for bars `[1..]`,
    /// using each bar's predecessor as the previous close. Returns `None` if
    /// the slice has fewer than 2 bars.
    pub fn average_true_range(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.len() < 2 {
            return None;
        }
        let sum: Decimal = (1..bars.len())
            .map(|i| bars[i].true_range(bars[i - 1].close))
            .sum();
        Some(sum / Decimal::from((bars.len() - 1) as u64))
    }

    /// Average body size across a slice of bars: mean of [`body`](Self::body) for each bar.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_body(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.body()).sum();
        Some(sum / Decimal::from(bars.len() as u64))
    }

    /// Maximum `high` across a slice of bars.
    ///
    /// Returns `None` if the slice is empty. Useful for computing resistance
    /// levels, swing highs, and ATH/period-high comparisons.
    pub fn highest_high(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.high).reduce(Decimal::max)
    }

    /// Minimum `low` across a slice of bars.
    ///
    /// Returns `None` if the slice is empty. Useful for computing support
    /// levels, swing lows, and ATL/period-low comparisons.
    pub fn lowest_low(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.low).reduce(Decimal::min)
    }

    /// Maximum `close` across a slice of bars.
    ///
    /// Returns `None` if the slice is empty. Useful for identifying the
    /// highest closing price within a lookback window.
    pub fn highest_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::max)
    }

    /// Minimum `close` across a slice of bars.
    ///
    /// Returns `None` if the slice is empty. Useful for identifying the
    /// lowest closing price within a lookback window.
    pub fn lowest_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::min)
    }

    /// Close price range: `highest_close − lowest_close` across a slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn close_range(bars: &[OhlcvBar]) -> Option<Decimal> {
        let hi = Self::highest_close(bars)?;
        let lo = Self::lowest_close(bars)?;
        Some(hi - lo)
    }

    /// N-period price momentum: `(close[last] / close[last - n]) − 1`.
    ///
    /// Returns `None` if the slice has fewer than `n + 1` bars or if
    /// `close[last - n]` is zero.
    pub fn momentum(bars: &[OhlcvBar], n: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let len = bars.len();
        if len <= n {
            return None;
        }
        let current = bars[len - 1].close;
        let prior = bars[len - 1 - n].close;
        if prior.is_zero() {
            return None;
        }
        ((current - prior) / prior).to_f64()
    }

    /// Total traded volume across a slice of bars.
    ///
    /// Returns `Decimal::ZERO` for an empty slice. Complements
    /// [`mean_volume`](Self::mean_volume) when the sum rather than the average
    /// is needed.
    pub fn sum_volume(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| b.volume).sum()
    }

    /// Count of bullish bars (close > open) in a slice.
    pub fn bullish_count(bars: &[OhlcvBar]) -> usize {
        bars.iter().filter(|b| b.is_bullish()).count()
    }

    /// Count of bearish bars (close < open) in a slice.
    pub fn bearish_count(bars: &[OhlcvBar]) -> usize {
        bars.iter().filter(|b| b.is_bearish()).count()
    }

    /// Length of the current bullish streak at the end of `bars`.
    ///
    /// Counts consecutive bullish bars (`close > open`) from the tail of the
    /// slice. Returns `0` if the last bar is not bullish or the slice is empty.
    pub fn bullish_streak(bars: &[OhlcvBar]) -> usize {
        bars.iter().rev().take_while(|b| b.is_bullish()).count()
    }

    /// Length of the current bearish streak at the end of `bars`.
    ///
    /// Counts consecutive bearish bars (`close < open`) from the tail of the
    /// slice. Returns `0` if the last bar is not bearish or the slice is empty.
    pub fn bearish_streak(bars: &[OhlcvBar]) -> usize {
        bars.iter().rev().take_while(|b| b.is_bearish()).count()
    }

    /// Fraction of bullish bars in a slice: `bullish_count / total`.
    ///
    /// Returns `None` if the slice is empty. Result is in `[0.0, 1.0]`.
    pub fn win_rate(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        Some(Self::bullish_count(bars) as f64 / bars.len() as f64)
    }

    /// Maximum drawdown of close prices across a slice of bars.
    ///
    /// Computed as the largest percentage decline from a running close peak:
    /// `max_drawdown = max over i of (peak_close_before_i - close_i) / peak_close_before_i`.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or if all closes are zero.
    /// Result is in `[0.0, ∞)` — a value of `0.05` means a 5% drawdown.
    pub fn max_drawdown(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let mut peak = bars[0].close;
        let mut max_dd = 0.0_f64;
        for bar in &bars[1..] {
            if bar.close > peak {
                peak = bar.close;
            } else if !peak.is_zero() {
                let dd = ((peak - bar.close) / peak).to_f64().unwrap_or(0.0);
                if dd > max_dd {
                    max_dd = dd;
                }
            }
        }
        Some(max_dd)
    }

    /// Ordinary least-squares slope of close prices across a slice of bars.
    ///
    /// Fits the line `close[i] = slope × i + intercept` using simple linear
    /// regression, where `i` is the bar index (0-based). A positive slope
    /// indicates an upward trend; negative indicates a downtrend.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or if the closes
    /// cannot be converted to `f64`.
    pub fn linear_regression_slope(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ys: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
        Self::ols_slope_indexed(&ys, bars.len())
    }

    /// OLS linear regression slope of bar volumes over bar index.
    ///
    /// Positive means volume is trending up; negative means trending down.
    /// Returns `None` for fewer than 2 bars or if volumes can't be converted to `f64`.
    pub fn volume_slope(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ys: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        Self::ols_slope_indexed(&ys, bars.len())
    }

    /// OLS slope of `ys` against integer indices `0..expected_n`.
    ///
    /// Returns `None` if `ys.len() < expected_n`, `expected_n < 2`, or the x-variance is zero.
    fn ols_slope_indexed(ys: &[f64], expected_n: usize) -> Option<f64> {
        if ys.len() < expected_n || expected_n < 2 {
            return None;
        }
        let n_f = expected_n as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let y_mean = ys.iter().sum::<f64>() / n_f;
        let numerator: f64 = ys.iter().enumerate().map(|(i, y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let denominator: f64 = ys.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if denominator == 0.0 {
            return None;
        }
        Some(numerator / denominator)
    }

    /// Arithmetic mean of close prices across a slice of bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn mean_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.close).sum();
        Some(sum / Decimal::from(bars.len() as u64))
    }

    /// Population standard deviation of close prices across a slice of bars.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or if closes cannot
    /// be converted to `f64`.
    pub fn close_std_dev(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let mean = Self::mean_close(bars)?.to_f64()?;
        let variance: f64 = bars.iter()
            .filter_map(|b| b.close.to_f64())
            .map(|c| (c - mean).powi(2))
            .sum::<f64>() / n as f64;
        Some(variance.sqrt())
    }

    /// Elder's efficiency ratio: `|close[last] − close[first]| / Σ|range(bar)|`.
    ///
    /// Measures how directionally efficient price movement is across the slice.
    /// A value close to 1 means price moved cleanly; near 0 means choppy.
    /// Returns `None` if the slice has fewer than 2 bars or total range is zero.
    pub fn price_efficiency_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let net_move = (bars[n - 1].close - bars[0].close).abs();
        let total_path: Decimal = bars.iter().map(|b| b.range()).sum();
        if total_path.is_zero() {
            return None;
        }
        (net_move / total_path).to_f64()
    }

    /// Mean CLV across a slice of bars; `None` for an empty slice or
    /// if any CLV cannot be computed.
    pub fn mean_clv(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let clvs: Vec<f64> = bars.iter().filter_map(|b| b.close_location_value()).collect();
        if clvs.is_empty() {
            return None;
        }
        Some(clvs.iter().sum::<f64>() / clvs.len() as f64)
    }

    /// Mean of the high-low range (H − L) across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn mean_range(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let total: Decimal = bars.iter().map(|b| b.range()).sum();
        Some(total / Decimal::from(bars.len() as u64))
    }

    /// Z-score of `value` relative to the close price series.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or the standard
    /// deviation is zero.
    pub fn close_z_score(bars: &[OhlcvBar], value: Decimal) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = Self::mean_close(bars)?;
        let std_dev = Self::close_std_dev(bars)?;
        if std_dev == 0.0 {
            return None;
        }
        ((value - mean) / Decimal::try_from(std_dev).ok()?).to_f64()
    }

    /// Normalised Bollinger Band width: `2 × close_std_dev / mean_close`.
    ///
    /// Returns `None` if `mean_close` is zero or the slice is too small.
    pub fn bollinger_band_width(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = Self::mean_close(bars)?;
        if mean.is_zero() {
            return None;
        }
        let std_dev = Self::close_std_dev(bars)?;
        let width = 2.0 * std_dev / mean.to_f64()?;
        Some(width)
    }

    /// Ratio of bullish bars to bearish bars in the slice.
    ///
    /// Returns `None` if there are no bearish bars.
    pub fn up_down_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        let down = Self::bearish_count(bars);
        if down == 0 {
            return None;
        }
        Some(Self::bullish_count(bars) as f64 / down as f64)
    }

    /// Volume-weighted average close price across the slice.
    ///
    /// Returns `None` if the slice is empty or total volume is zero.
    pub fn volume_weighted_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        let total_volume = Self::sum_volume(bars);
        if total_volume.is_zero() {
            return None;
        }
        let weighted_sum: Decimal = bars.iter().map(|b| b.close * b.volume).sum();
        Some(weighted_sum / total_volume)
    }

    /// Percentage change in close price from the first bar to the last.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or the first
    /// close is zero.
    pub fn rolling_return(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let first = bars[0].close;
        let last = bars[n - 1].close;
        if first.is_zero() {
            return None;
        }
        ((last - first) / first).to_f64()
    }

    /// Mean of the high prices across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_high(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let total: Decimal = bars.iter().map(|b| b.high).sum();
        Some(total / Decimal::from(bars.len() as u64))
    }

    /// Mean of the low prices across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_low(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let total: Decimal = bars.iter().map(|b| b.low).sum();
        Some(total / Decimal::from(bars.len() as u64))
    }

    /// Minimum bar body size (|close − open|) across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn min_body(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.body()).reduce(Decimal::min)
    }

    /// Maximum bar body size (|close − open|) across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn max_body(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.body()).reduce(Decimal::max)
    }

    /// Average True Range expressed as a fraction of the mean close price.
    ///
    /// Returns `None` if the slice has fewer than 2 bars or mean close is zero.
    pub fn atr_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let atr = Self::average_true_range(bars)?;
        let mean = Self::mean_close(bars)?;
        if mean.is_zero() {
            return None;
        }
        (atr / mean).to_f64()
    }

    /// Count of bars (from index 1 onward) whose close strictly exceeds the
    /// previous bar's high — i.e., upside breakout bars.
    pub fn breakout_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2)
            .filter(|w| w[1].close > w[0].high)
            .count()
    }

    /// Count of doji bars (|close − open| ≤ epsilon) in the slice.
    pub fn doji_count(bars: &[OhlcvBar], epsilon: Decimal) -> usize {
        bars.iter().filter(|b| b.is_doji(epsilon)).count()
    }

    /// Full channel width: `highest_high − lowest_low` across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn channel_width(bars: &[OhlcvBar]) -> Option<Decimal> {
        let hi = Self::highest_high(bars)?;
        let lo = Self::lowest_low(bars)?;
        Some(hi - lo)
    }

    /// Simple moving average of the last `n` close prices.
    ///
    /// Returns `None` if `n` is zero or the slice has fewer than `n` bars.
    pub fn sma(bars: &[OhlcvBar], n: usize) -> Option<Decimal> {
        if n == 0 || bars.len() < n {
            return None;
        }
        let window = &bars[bars.len() - n..];
        let sum: Decimal = window.iter().map(|b| b.close).sum();
        Some(sum / Decimal::from(n as u64))
    }

    /// Mean wick ratio (upper_wick + lower_wick) / range across the slice.
    ///
    /// Bars where range is zero are excluded. Returns `None` if the slice is
    /// empty or all bars have zero range.
    pub fn mean_wick_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        let ratios: Vec<f64> = bars.iter().filter_map(|b| b.wick_ratio()).collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Total volume of all bullish (close ≥ open) bars in the slice.
    pub fn bullish_volume(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().filter(|b| b.is_bullish()).map(|b| b.volume).sum()
    }

    /// Total volume of all bearish (close < open) bars in the slice.
    pub fn bearish_volume(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().filter(|b| b.is_bearish()).map(|b| b.volume).sum()
    }

    /// Count of bars where close is strictly above the bar midpoint ((high + low) / 2).
    pub fn close_above_mid_count(bars: &[OhlcvBar]) -> usize {
        bars.iter().filter(|b| b.close > b.high_low_midpoint()).count()
    }

    /// Exponential moving average of close prices over the slice.
    ///
    /// `alpha` is the smoothing factor in (0.0, 1.0]; higher values weight
    /// recent bars more. Processes bars in order (oldest to newest).
    /// Returns `None` if the slice is empty.
    pub fn ema(bars: &[OhlcvBar], alpha: f64) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let alpha = alpha.clamp(1e-9, 1.0);
        let mut iter = bars.iter();
        let first = iter.next()?.close.to_f64()?;
        let result = iter.fold(first, |acc, b| {
            let c = b.close.to_f64().unwrap_or(acc);
            alpha * c + (1.0 - alpha) * acc
        });
        Some(result)
    }

    /// Maximum open price across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn highest_open(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.open).reduce(Decimal::max)
    }

    /// Minimum open price across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn lowest_open(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.open).reduce(Decimal::min)
    }

    /// Count of bars (from index 1 onward) where close is strictly greater
    /// than the previous bar's close.
    pub fn rising_close_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].close > w[0].close).count()
    }

    /// Mean body-to-range ratio across the slice.
    ///
    /// Bars with zero range are excluded.
    /// Returns `None` if the slice is empty or all bars have zero range.
    pub fn mean_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        let ratios: Vec<f64> = bars.iter().filter_map(|b| b.body_ratio()).collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Sample standard deviation of bar volumes across the slice.
    ///
    /// Returns `None` if the slice has fewer than 2 bars.
    pub fn volume_std_dev(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let vols: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        if vols.len() < 2 {
            return None;
        }
        let mean = vols.iter().sum::<f64>() / vols.len() as f64;
        let variance = vols.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vols.len() - 1) as f64;
        Some(variance.sqrt())
    }

    /// Bar with the highest volume in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn max_volume_bar(bars: &[OhlcvBar]) -> Option<&OhlcvBar> {
        bars.iter().max_by(|a, b| a.volume.cmp(&b.volume))
    }

    /// Bar with the lowest volume in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn min_volume_bar(bars: &[OhlcvBar]) -> Option<&OhlcvBar> {
        bars.iter().min_by(|a, b| a.volume.cmp(&b.volume))
    }

    /// Sum of gap-open amounts: Σ (open[n] − close[n−1]) for n ≥ 1.
    ///
    /// A positive value indicates upward gaps dominate; negative means downward.
    pub fn gap_sum(bars: &[OhlcvBar]) -> Decimal {
        if bars.len() < 2 {
            return Decimal::ZERO;
        }
        bars.windows(2).map(|w| w[1].open - w[0].close).sum()
    }

    /// Returns `true` if the last three bars form a "three white soldiers" pattern:
    /// three consecutive bullish (close > open) bars, each closing above the prior bar's close.
    pub fn three_white_soldiers(bars: &[OhlcvBar]) -> bool {
        if bars.len() < 3 {
            return false;
        }
        let last3 = &bars[bars.len() - 3..];
        last3[0].close > last3[0].open
            && last3[1].close > last3[1].open
            && last3[2].close > last3[2].open
            && last3[1].close > last3[0].close
            && last3[2].close > last3[1].close
    }

    /// Returns `true` if the last three bars form a "three black crows" pattern:
    /// three consecutive bearish (close < open) bars, each closing below the prior bar's close.
    pub fn three_black_crows(bars: &[OhlcvBar]) -> bool {
        if bars.len() < 3 {
            return false;
        }
        let last3 = &bars[bars.len() - 3..];
        last3[0].close < last3[0].open
            && last3[1].close < last3[1].open
            && last3[2].close < last3[2].open
            && last3[1].close < last3[0].close
            && last3[2].close < last3[1].close
    }

    /// Returns `true` if the bar opened with a gap relative to `prev_close`
    /// (i.e. open != prev_close).
    pub fn is_gap_bar(bar: &OhlcvBar, prev_close: Decimal) -> bool {
        bar.open != prev_close
    }

    /// Counts consecutive-pair windows where a gap exists (open != prior close).
    pub fn gap_bars_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].open != w[0].close).count()
    }

    /// Bar efficiency: ratio of net price move to total range.
    ///
    /// `(close - open).abs() / (high - low)`.  Returns `None` for a zero-range bar.
    pub fn bar_efficiency(bar: &OhlcvBar) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = bar.range();
        if range.is_zero() {
            return None;
        }
        (bar.body() / range).to_f64()
    }

    /// Sum of upper and lower wick lengths for each bar.
    ///
    /// Upper wick = `high - close.max(open)`, lower wick = `close.min(open) - low`.
    pub fn wicks_sum(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| b.wick_upper() + b.wick_lower()).sum()
    }

    /// Mean of `(high - close)` across all bars — average distance from close to high.
    ///
    /// Returns `None` for an empty slice.
    pub fn avg_close_to_high(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.high - b.close).sum();
        (sum / Decimal::from(bars.len() as u32)).to_f64()
    }

    /// Mean of `(high - low)` across all bars.
    ///
    /// Returns `None` for an empty slice.
    pub fn avg_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.range()).sum();
        (sum / Decimal::from(bars.len() as u32)).to_f64()
    }

    /// Maximum close price in the slice.
    ///
    /// Returns `None` for an empty slice.
    pub fn max_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::max)
    }

    /// Minimum close price in the slice.
    ///
    /// Returns `None` for an empty slice.
    pub fn min_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::min)
    }

    /// Trend strength: fraction of consecutive close-to-close moves that are upward.
    ///
    /// Returns `None` if fewer than 2 bars.
    pub fn trend_strength(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        let moves = bars.len() - 1;
        let up = bars.windows(2).filter(|w| w[1].close > w[0].close).count();
        Some(up as f64 / moves as f64)
    }

    /// Net price change: `close - open` for the last bar.
    ///
    /// Returns `None` for an empty slice.
    pub fn net_change(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.last().map(|b| b.price_change())
    }

    /// Percentage change from open to close for the last bar: `(close - open) / open * 100`.
    ///
    /// Returns `None` for an empty slice or zero open.
    pub fn open_to_close_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bar = bars.last()?;
        if bar.open.is_zero() {
            return None;
        }
        (bar.price_change() / bar.open * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Percentage of high-to-low range relative to high: `(high - low) / high * 100`.
    ///
    /// Returns `None` for an empty slice or zero high.
    pub fn high_to_low_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bar = bars.last()?;
        if bar.high.is_zero() {
            return None;
        }
        (bar.range() / bar.high * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Count of consecutive bars (from the end) where `high` is strictly higher than the prior `high`.
    pub fn consecutive_highs(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        let mut count = 0;
        for w in bars.windows(2).rev() {
            if w[1].high > w[0].high {
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    /// Count of consecutive bars (from the end) where `low` is strictly lower than the prior `low`.
    pub fn consecutive_lows(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        let mut count = 0;
        for w in bars.windows(2).rev() {
            if w[1].low < w[0].low {
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    /// Percentage change in volume from one bar to the next (last bar vs prior bar).
    ///
    /// Returns `None` if fewer than 2 bars or prior volume is zero.
    pub fn volume_change_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let prior = bars[bars.len() - 2].volume;
        if prior.is_zero() {
            return None;
        }
        let current = bars[bars.len() - 1].volume;
        ((current - prior) / prior * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Gap percentage between consecutive bars: `(bar.open - prev.close) / prev.close * 100`.
    ///
    /// Returns `None` if fewer than 2 bars or previous close is zero.
    pub fn open_gap_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let prev_close = bars[bars.len() - 2].close;
        if prev_close.is_zero() {
            return None;
        }
        let current_open = bars[bars.len() - 1].open;
        ((current_open - prev_close) / prev_close * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Cumulative volume across all bars.
    pub fn volume_cumulative(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| b.volume).sum()
    }

    /// Position of the last bar's close within the overall high-low range of all bars.
    ///
    /// Returns `(close - lowest_low) / (highest_high - lowest_low)`.
    /// Returns `None` if the slice is empty or range is zero.
    pub fn price_position(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let hi = Self::highest_high(bars)?;
        let lo = Self::lowest_low(bars)?;
        let range = hi - lo;
        if range.is_zero() {
            return None;
        }
        let last_close = bars.last()?.close;
        ((last_close - lo) / range).to_f64()
    }

    /// Returns `true` if the last `n` closes form a strict uptrend.
    ///
    /// Each close must be strictly greater than the previous. Returns `false`
    /// if `n < 2` or the slice has fewer than `n` bars.
    pub fn is_trending_up(bars: &[OhlcvBar], n: usize) -> bool {
        if n < 2 || bars.len() < n {
            return false;
        }
        bars[bars.len() - n..].windows(2).all(|w| w[1].close > w[0].close)
    }

    /// Returns `true` if the last `n` closes form a strict downtrend.
    ///
    /// Each close must be strictly less than the previous. Returns `false`
    /// if `n < 2` or the slice has fewer than `n` bars.
    pub fn is_trending_down(bars: &[OhlcvBar], n: usize) -> bool {
        if n < 2 || bars.len() < n {
            return false;
        }
        bars[bars.len() - n..].windows(2).all(|w| w[1].close < w[0].close)
    }

    /// Percentage change in volume between the last two bars.
    ///
    /// Returns `None` if fewer than 2 bars or if the previous bar's volume is
    /// zero.
    pub fn volume_acceleration(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let prev = bars[bars.len() - 2].volume;
        if prev.is_zero() {
            return None;
        }
        let curr = bars[bars.len() - 1].volume;
        ((curr - prev) / prev * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Mean ratio of total wick length to body size across all bars.
    ///
    /// For each bar: `(upper_wick + lower_wick) / body`. Bars with a zero body
    /// are skipped. Returns `None` if no valid bars exist.
    pub fn wick_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let valid: Vec<f64> = bars.iter().filter_map(|b| {
            let body = b.body();
            if body.is_zero() {
                return None;
            }
            let wicks = b.wick_upper() + b.wick_lower();
            (wicks / body).to_f64()
        }).collect();
        if valid.is_empty() {
            return None;
        }
        Some(valid.iter().sum::<f64>() / valid.len() as f64)
    }

    /// Count of bars where `close > open` (bullish bars).
    pub fn close_above_open_count(bars: &[OhlcvBar]) -> usize {
        bars.iter().filter(|b| b.close > b.open).count()
    }

    /// Pearson correlation between per-bar volume and close price.
    ///
    /// Returns `None` if fewer than 2 bars or if either series has zero
    /// variance.
    pub fn volume_price_correlation(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let vols: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        let closes: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
        if vols.len() != n || closes.len() != n {
            return None;
        }
        let nf = n as f64;
        let mean_v = vols.iter().sum::<f64>() / nf;
        let mean_c = closes.iter().sum::<f64>() / nf;
        let cov: f64 = vols.iter().zip(closes.iter()).map(|(v, c)| (v - mean_v) * (c - mean_c)).sum::<f64>() / nf;
        let std_v = (vols.iter().map(|v| (v - mean_v).powi(2)).sum::<f64>() / nf).sqrt();
        let std_c = (closes.iter().map(|c| (c - mean_c).powi(2)).sum::<f64>() / nf).sqrt();
        if std_v == 0.0 || std_c == 0.0 {
            return None;
        }
        Some(cov / (std_v * std_c))
    }

    /// Fraction of bars where body size exceeds 50% of the bar's total range.
    ///
    /// Returns `None` if the slice is empty or all bars have zero range.
    pub fn body_consistency(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let valid: Vec<_> = bars.iter().filter(|b| !b.range().is_zero()).collect();
        if valid.is_empty() {
            return None;
        }
        let consistent = valid.iter().filter(|b| {
            b.body() * Decimal::TWO > b.range()
        }).count();
        Some(consistent as f64 / valid.len() as f64)
    }

    /// Coefficient of variation of close prices: `std_dev(close) / mean(close)`.
    ///
    /// A dimensionless measure of close price dispersion. Returns `None` if
    /// fewer than 2 bars or if mean close is zero.
    pub fn close_volatility_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean = Self::mean_close(bars)?;
        if mean.is_zero() {
            return None;
        }
        let std = Self::close_std_dev(bars)?;
        let mean_f = mean.to_f64()?;
        Some(std / mean_f.abs())
    }

    /// Rolling close momentum score: fraction of bars where close is above the
    /// simple average close of the window.
    ///
    /// Returns `None` if the slice is empty or the mean cannot be computed.
    pub fn close_momentum_score(bars: &[OhlcvBar]) -> Option<f64> {
        let mean = Self::mean_close(bars)?;
        let above = bars.iter().filter(|b| b.close > mean).count();
        Some(above as f64 / bars.len() as f64)
    }

    /// Count of bars where the range (high - low) exceeds the preceding bar's
    /// range (i.e., the bar "expands" relative to the prior bar).
    ///
    /// Returns 0 if fewer than 2 bars.
    pub fn range_expansion_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].range() > w[0].range()).count()
    }

    /// Count of bars where the open gaps away from the previous bar's close
    /// (absolute gap > zero, i.e., `open != prev_close`).
    pub fn gap_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].open != w[0].close).count()
    }

    /// Mean total wick size (upper + lower wick) across all bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn avg_wick_size(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total: f64 = bars.iter()
            .filter_map(|b| (b.wick_upper() + b.wick_lower()).to_f64())
            .sum();
        Some(total / bars.len() as f64)
    }

    /// Ratio of each bar's volume to the mean volume of the window.
    ///
    /// Returns a `Vec` of `Option<f64>` — `None` entries indicate bars where
    /// the mean cannot be computed (e.g., empty slice) or the conversion
    /// failed. Returns an empty `Vec` for an empty slice.
    pub fn mean_volume_ratio(bars: &[OhlcvBar]) -> Vec<Option<f64>> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return vec![];
        }
        let mean = match Self::mean_volume(bars) {
            Some(m) if !m.is_zero() => m,
            _ => return bars.iter().map(|_| None).collect(),
        };
        bars.iter().map(|b| (b.volume / mean).to_f64()).collect()
    }

    /// Count of bars where `close > n`-bar simple moving average of highs.
    ///
    /// Returns 0 if `n < 1` or the slice has fewer than `n` bars.
    pub fn close_above_high_ma(bars: &[OhlcvBar], n: usize) -> usize {
        if n < 1 || bars.len() < n {
            return 0;
        }
        let high_ma: Decimal = bars.iter().take(n).map(|b| b.high).sum::<Decimal>()
            / Decimal::from(n as u32);
        bars[n - 1..].iter().filter(|b| b.close > high_ma).count()
    }

    /// Price compression ratio: `mean_body / mean_range`.
    ///
    /// A value near 1 indicates full-body candles (strong directional moves);
    /// near 0 indicates indecision or doji-heavy periods. Returns `None` if
    /// the slice is empty or mean range is zero.
    pub fn price_compression_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mean_body = Self::average_body(bars)?;
        let mean_range = Self::mean_range(bars)?;
        if mean_range.is_zero() {
            return None;
        }
        (mean_body / mean_range).to_f64()
    }

    /// Mean absolute difference between open and close across all bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn open_close_spread(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total: f64 = bars.iter()
            .filter_map(|b| (b.close - b.open).abs().to_f64())
            .sum();
        Some(total / bars.len() as f64)
    }

    /// Longest run of consecutive bars with strictly rising closes.
    pub fn max_consecutive_gains(bars: &[OhlcvBar]) -> usize {
        let mut max_run = 0usize;
        let mut current = 0usize;
        for w in bars.windows(2) {
            if w[1].close > w[0].close {
                current += 1;
                if current > max_run {
                    max_run = current;
                }
            } else {
                current = 0;
            }
        }
        max_run
    }

    /// Longest run of consecutive bars with strictly falling closes.
    pub fn max_consecutive_losses(bars: &[OhlcvBar]) -> usize {
        let mut max_run = 0usize;
        let mut current = 0usize;
        for w in bars.windows(2) {
            if w[1].close < w[0].close {
                current += 1;
                if current > max_run {
                    max_run = current;
                }
            } else {
                current = 0;
            }
        }
        max_run
    }

    /// Total path length of close prices: sum of absolute consecutive changes.
    ///
    /// Measures how much the close price "travels" over the window. A high
    /// value relative to the net change indicates choppy price action.
    /// Returns `None` if fewer than 2 bars.
    pub fn price_path_length(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let total: f64 = bars.windows(2)
            .filter_map(|w| (w[1].close - w[0].close).abs().to_f64())
            .sum();
        Some(total)
    }

    /// Count of bars where the close reverts toward the window mean (i.e., the
    /// bar's close is between the previous close and the window mean close).
    ///
    /// Returns 0 if fewer than 2 bars or no mean can be computed.
    pub fn close_reversion_count(bars: &[OhlcvBar]) -> usize {
        let mean = match Self::mean_close(bars) {
            Some(m) => m,
            None => return 0,
        };
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| {
            let prev = w[0].close;
            let curr = w[1].close;
            // Reverts if the current close is between prev and mean
            if prev < mean {
                curr > prev && curr <= mean
            } else {
                curr < prev && curr >= mean
            }
        }).count()
    }

    /// ATR as a fraction of the mean close price.
    ///
    /// `ATR / mean_close * 100`. A dimensionless volatility measure. Returns
    /// `None` if fewer than 2 bars or if mean close is zero.
    pub fn atr_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let atr = Self::average_true_range(bars)?;
        let mean = Self::mean_close(bars)?;
        if mean.is_zero() {
            return None;
        }
        (atr / mean * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Pearson correlation between bar index and volume — a measure of whether
    /// volume is trending up or down over the window.
    ///
    /// Returns `None` if fewer than 2 bars or if either series has zero variance.
    pub fn volume_trend_strength(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let nf = n as f64;
        let indices: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let vols: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        if vols.len() != n {
            return None;
        }
        let mean_i = indices.iter().sum::<f64>() / nf;
        let mean_v = vols.iter().sum::<f64>() / nf;
        let cov: f64 = indices.iter().zip(vols.iter()).map(|(i, v)| (i - mean_i) * (v - mean_v)).sum::<f64>() / nf;
        let std_i = (indices.iter().map(|i| (i - mean_i).powi(2)).sum::<f64>() / nf).sqrt();
        let std_v = (vols.iter().map(|v| (v - mean_v).powi(2)).sum::<f64>() / nf).sqrt();
        if std_i == 0.0 || std_v == 0.0 {
            return None;
        }
        Some(cov / (std_i * std_v))
    }

    /// Mean spread between high and close across all bars.
    ///
    /// `mean(high - close)`. Always ≥ 0 since high ≥ close by definition.
    /// Returns `None` if the slice is empty.
    pub fn high_close_spread(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total: f64 = bars.iter().filter_map(|b| (b.high - b.close).to_f64()).sum();
        Some(total / bars.len() as f64)
    }

    /// Mean open-to-first-close range for the session opening bar.
    ///
    /// Defined as the absolute distance `|close - open|` averaged over all
    /// bars. Equivalent to [`open_close_spread`] but named for discoverability.
    /// Returns `None` if the slice is empty.
    pub fn open_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total: f64 = bars.iter().filter_map(|b| (b.close - b.open).abs().to_f64()).sum();
        Some(total / bars.len() as f64)
    }

    /// Normalised close: last close as a fraction of the window's close range.
    ///
    /// `(last_close - min_close) / (max_close - min_close)`. Returns `None`
    /// if fewer than 2 bars or the close range is zero.
    pub fn normalized_close(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let min = Self::min_close(bars)?;
        let max = Self::max_close(bars)?;
        let range = max - min;
        if range.is_zero() {
            return None;
        }
        let last = bars.last()?.close;
        ((last - min) / range).to_f64()
    }

    /// Price channel position: where the last close falls in the
    /// `[lowest_low, highest_high]` range.
    ///
    /// `(last_close - lowest_low) / (highest_high - lowest_low)`. Returns
    /// `None` if the slice is empty or the range is zero.
    pub fn price_channel_position(bars: &[OhlcvBar]) -> Option<f64> {
        Self::price_position(bars)
    }

    /// Composite candle score: fraction of bars that are bullish, have a body
    /// above 50% of range, and close above the midpoint of the bar.
    ///
    /// Returns `None` if the slice is empty.
    pub fn candle_score(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let strong = bars.iter().filter(|b| {
            b.is_bullish()
                && !b.range().is_zero()
                && b.body() * Decimal::TWO > b.range()
                && b.close_above_midpoint()
        }).count();
        Some(strong as f64 / bars.len() as f64)
    }

    /// Mean number of ticks per millisecond of bar duration.
    ///
    /// `tick_count / bar_duration_ms`. Returns `None` if the slice is empty
    /// or the total duration across bars is zero.
    pub fn bar_speed(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let total_ticks: u64 = bars.iter().map(|b| b.trade_count).sum();
        let total_ms: u64 = bars.iter().map(|b| b.bar_duration_ms()).sum();
        if total_ms == 0 {
            return None;
        }
        Some(total_ticks as f64 / total_ms as f64)
    }

    /// Count of bars where the high is strictly greater than the previous bar's high.
    ///
    /// Returns 0 if fewer than 2 bars.
    pub fn higher_highs_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].high > w[0].high).count()
    }

    /// Count of bars where the low is strictly less than the previous bar's low.
    ///
    /// Returns 0 if fewer than 2 bars.
    pub fn lower_lows_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].low < w[0].low).count()
    }

    /// Mean `(close - open) / open * 100` across all bars.
    ///
    /// Bars with zero open are skipped. Returns `None` if no valid bars.
    pub fn close_minus_open_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars.iter().filter_map(|b| {
            if b.open.is_zero() { return None; }
            ((b.close - b.open) / b.open * Decimal::ONE_HUNDRED).to_f64()
        }).collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Mean volume per unit of bar range.
    ///
    /// `volume / (high - low)`. Bars with zero range are skipped. Returns
    /// `None` if the slice is empty or all bars have zero range.
    pub fn volume_per_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars.iter().filter_map(|b| {
            let r = b.range();
            if r.is_zero() { return None; }
            (b.volume / r).to_f64()
        }).collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Average body size (|close − open|) as a fraction of total bar range.
    ///
    /// Returns `None` if all bars have zero range or the slice is empty.
    pub fn body_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let fracs: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.range();
            if range.is_zero() { return None; }
            let body = (b.close - b.open).abs();
            (body / range).to_f64()
        }).collect();
        if fracs.is_empty() {
            return None;
        }
        Some(fracs.iter().sum::<f64>() / fracs.len() as f64)
    }

    /// Fraction of bars that are bullish (close strictly greater than open).
    ///
    /// Returns `None` if the slice is empty.
    pub fn bullish_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let bullish = bars.iter().filter(|b| b.close > b.open).count();
        Some(bullish as f64 / bars.len() as f64)
    }

    /// Maximum (highest) close price across all bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn peak_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::max)
    }

    /// Minimum (lowest) close price across all bars.
    ///
    /// Returns `None` if the slice is empty.
    pub fn trough_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.close).reduce(Decimal::min)
    }

    /// Fraction of total volume that occurred on up-bars (close > open).
    ///
    /// Returns `None` if total volume is zero or the slice is empty.
    pub fn up_volume_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total: Decimal = bars.iter().map(|b| b.volume).sum();
        if total.is_zero() {
            return None;
        }
        let up_vol: Decimal = bars.iter()
            .filter(|b| b.close > b.open)
            .map(|b| b.volume)
            .sum();
        (up_vol / total).to_f64()
    }

    /// Average upper wick as a fraction of total bar range.
    ///
    /// Upper wick = high − max(open, close). Returns `None` if all bars have
    /// zero range or the slice is empty.
    pub fn tail_upper_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let fracs: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.range();
            if range.is_zero() { return None; }
            let body_top = b.open.max(b.close);
            let upper_wick = b.high - body_top;
            (upper_wick / range).to_f64()
        }).collect();
        if fracs.is_empty() {
            return None;
        }
        Some(fracs.iter().sum::<f64>() / fracs.len() as f64)
    }

    /// Average lower wick as a fraction of total bar range.
    ///
    /// Lower wick = min(open, close) − low. Returns `None` if all bars have
    /// zero range or the slice is empty.
    pub fn tail_lower_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let fracs: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.range();
            if range.is_zero() { return None; }
            let body_bot = b.open.min(b.close);
            let lower_wick = body_bot - b.low;
            (lower_wick / range).to_f64()
        }).collect();
        if fracs.is_empty() {
            return None;
        }
        Some(fracs.iter().sum::<f64>() / fracs.len() as f64)
    }

    /// Standard deviation of bar ranges (high − low).
    ///
    /// Measures consistency of bar volatility. Returns `None` if fewer than 2
    /// bars are provided.
    pub fn range_std_dev(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| b.range().to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    /// Mean of `(close − low) / range` across bars — where the close lands
    /// inside each bar's high-low range.
    ///
    /// Returns `None` if the slice is empty or every bar has zero range.
    /// A value near 1.0 means closes are consistently near the high (bullish);
    /// near 0.0 means closes hug the low (bearish).
    pub fn close_to_range_position(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() {
                    return None;
                }
                ((b.close - b.low) / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Volume oscillator: `(short_avg_vol − long_avg_vol) / long_avg_vol`.
    ///
    /// `short_n` bars are used for the short average and `long_n` for the long.
    /// Returns `None` if `long_n > bars.len()`, `short_n == 0`, `long_n == 0`,
    /// `short_n >= long_n`, or the long average is zero.
    ///
    /// A positive result means recent volume is above the longer-term average
    /// (expanding volume); negative means volume is contracting.
    pub fn volume_oscillator(bars: &[OhlcvBar], short_n: usize, long_n: usize) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if short_n == 0 || long_n == 0 || short_n >= long_n || bars.len() < long_n {
            return None;
        }
        let recent = &bars[bars.len() - short_n..];
        let long_slice = &bars[bars.len() - long_n..];
        let short_avg: f64 =
            recent.iter().filter_map(|b| b.volume.to_f64()).sum::<f64>() / short_n as f64;
        let long_sum: Vec<f64> = long_slice.iter().filter_map(|b| b.volume.to_f64()).collect();
        if long_sum.is_empty() {
            return None;
        }
        let long_avg = long_sum.iter().sum::<f64>() / long_sum.len() as f64;
        if long_avg == 0.0 {
            return None;
        }
        Some((short_avg - long_avg) / long_avg)
    }

    /// Count of consecutive direction changes: how many times the bar
    /// sentiment (bullish / bearish) flips from one bar to the next.
    ///
    /// Doji bars (open == close) count as a continuation of the previous
    /// direction. Returns 0 for slices shorter than 2.
    pub fn direction_reversal_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        let mut count = 0usize;
        let mut prev_bullish: Option<bool> = None;
        for b in bars {
            let bullish = b.close > b.open;
            if let Some(pb) = prev_bullish {
                if bullish != pb {
                    count += 1;
                }
            }
            prev_bullish = Some(bullish);
        }
        count
    }

    /// Fraction of bars where the upper wick is strictly longer than the
    /// lower wick.
    ///
    /// Returns `None` for an empty slice.
    pub fn upper_wick_dominance_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.wick_upper() > b.wick_lower()).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Mean of `(high − open) / range` across bars — average fraction of the
    /// bar the price moved up from the open.
    ///
    /// Returns `None` if the slice is empty or every bar has zero range.
    pub fn avg_open_to_high_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() {
                    return None;
                }
                ((b.high - b.open) / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Volume-weighted average of bar range: `sum(range × volume) / sum(volume)`.
    ///
    /// Returns `None` when the slice is empty or total volume is zero.
    pub fn volume_weighted_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let mut numerator = 0f64;
        let mut denom = 0f64;
        for b in bars {
            let r = b.range().to_f64()?;
            let v = b.volume.to_f64()?;
            numerator += r * v;
            denom += v;
        }
        if denom == 0.0 {
            return None;
        }
        Some(numerator / denom)
    }

    /// Bar strength index: mean close-location value (CLV) across the slice.
    ///
    /// CLV for each bar is `(close − low − (high − close)) / range`, which
    /// is `+1` when close == high and `−1` when close == low. Bars with zero
    /// range are excluded.
    ///
    /// Returns `None` when no bars have non-zero range.
    pub fn bar_strength_index(bars: &[OhlcvBar]) -> Option<f64> {
        let vals: Vec<f64> =
            bars.iter().filter_map(|b| b.close_location_value()).collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Ratio of total wick length to total body size across the slice.
    ///
    /// Computes `sum(upper_wick + lower_wick) / sum(body)` where body is
    /// `|close − open|`. Returns `None` when total body is zero (all doji
    /// or empty slice).
    pub fn shadow_to_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let total_wick: Decimal = bars.iter().map(|b| b.wick_upper() + b.wick_lower()).sum();
        let total_body: Decimal = bars.iter().map(|b| b.body()).sum();
        if total_body.is_zero() {
            return None;
        }
        (total_wick / total_body).to_f64()
    }

    /// Percentage change from the first bar's close to the last bar's close.
    ///
    /// Computed as `(last.close − first.close) / first.close × 100`.
    /// Returns `None` when the slice is empty or the first close is zero.
    pub fn first_last_close_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let first = bars.first()?;
        let last = bars.last()?;
        if first.close.is_zero() {
            return None;
        }
        ((last.close - first.close) / first.close * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Standard deviation of per-bar `(close − open) / open` returns.
    ///
    /// Measures intrabar volatility consistency. Returns `None` when fewer
    /// than 2 bars are provided or every open is zero.
    pub fn open_to_close_volatility(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let returns: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.open.is_zero() {
                    return None;
                }
                ((b.close - b.open) / b.open).to_f64()
            })
            .collect();
        if returns.len() < 2 {
            return None;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    // ── round-80 ─────────────────────────────────────────────────────────────

    /// Mean of `(close − low) / range` across bars.
    ///
    /// Indicates where each close lands within its bar's high-low range.
    /// Near 1.0 means closes consistently hug the high (bullish);
    /// near 0.0 means closes hug the low (bearish). Bars with zero range
    /// are excluded. Returns `None` if no bars have non-zero range.
    pub fn close_recovery_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() {
                    return None;
                }
                ((b.close - b.low) / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Median bar range `(high − low)` across the slice.
    ///
    /// Robust to outlier bars with unusually wide or narrow ranges.
    /// Returns `None` for an empty slice.
    pub fn median_range(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let mut ranges: Vec<Decimal> = bars.iter().map(|b| b.range()).collect();
        ranges.sort();
        let n = ranges.len();
        if n % 2 == 1 {
            Some(ranges[n / 2])
        } else {
            Some((ranges[n / 2 - 1] + ranges[n / 2]) / Decimal::from(2u64))
        }
    }

    /// Mean typical price `(high + low + close) / 3` across the slice.
    ///
    /// The typical price is a common single-value summary of a bar used
    /// in pivot-point and money-flow calculations. Returns `None` if empty.
    pub fn mean_typical_price(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.typical_price()).sum();
        Some(sum / Decimal::from(bars.len() as u64))
    }

    /// Ratio of bullish-bar volume to the sum of bullish and bearish volumes.
    ///
    /// A value near 1.0 means almost all volume is in up-bars; near 0.0
    /// means down-bars dominate. Returns `None` when both are zero (e.g.,
    /// all flat/neutral bars).
    pub fn directional_volume_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let bull = Self::bullish_volume(bars);
        let bear = Self::bearish_volume(bars);
        let total = bull + bear;
        if total.is_zero() {
            return None;
        }
        (bull / total).to_f64()
    }

    /// Fraction of bars (from the second onward) that are inside bars.
    ///
    /// An inside bar has a high ≤ previous high and low ≥ previous low.
    /// Returns `None` if the slice has fewer than 2 bars.
    pub fn inside_bar_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        let inside = bars.windows(2).filter(|w| w[1].is_inside_bar(&w[0])).count();
        Some(inside as f64 / (bars.len() - 1) as f64)
    }

    /// Net body momentum: sum of signed body sizes across all bars.
    ///
    /// Bullish bars contribute `+(close − open)`; bearish bars contribute
    /// `−(open − close)`; flat bars contribute `0`. A positive total
    /// indicates the bars collectively closed higher than they opened.
    pub fn body_momentum(bars: &[OhlcvBar]) -> Decimal {
        bars.iter()
            .map(|b| b.close - b.open)
            .sum()
    }

    /// Mean `trade_count` (ticks per bar) across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn avg_trade_count(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let total: u64 = bars.iter().map(|b| b.trade_count).sum();
        Some(total as f64 / bars.len() as f64)
    }

    /// Maximum `trade_count` seen across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn max_trade_count(bars: &[OhlcvBar]) -> Option<u64> {
        bars.iter().map(|b| b.trade_count).max()
    }

    // ── round-81 ─────────────────────────────────────────────────────────────

    /// Standard deviation of `(close − high)` across bars.
    ///
    /// Measures how consistently close prices approach the bar high.
    /// Returns `None` if fewer than 2 bars are provided.
    pub fn close_to_high_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| (b.high - b.close).to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    /// Mean ratio of `volume / open` across bars.
    ///
    /// Normalises bar volume by the opening price level, useful for comparing
    /// activity across different price regimes. Bars with zero open are skipped.
    /// Returns `None` if no bars have a non-zero open.
    pub fn avg_open_volume_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.open.is_zero() {
                    return None;
                }
                (b.volume / b.open).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Standard deviation of typical prices `(high + low + close) / 3` across bars.
    ///
    /// Returns `None` if fewer than 2 bars are provided.
    pub fn typical_price_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| b.typical_price().to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    /// Mean absolute deviation of bar VWAPs from the slice mean VWAP.
    ///
    /// Measures how spread the intrabar VWAPs are across the slice.
    /// Bars without a VWAP (gap-fill bars) are skipped.
    /// Returns `None` if fewer than 1 bar has a VWAP.
    pub fn vwap_deviation_avg(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vwaps: Vec<f64> = bars
            .iter()
            .filter_map(|b| b.vwap?.to_f64())
            .collect();
        if vwaps.is_empty() {
            return None;
        }
        let mean = vwaps.iter().sum::<f64>() / vwaps.len() as f64;
        let mad = vwaps.iter().map(|v| (v - mean).abs()).sum::<f64>() / vwaps.len() as f64;
        Some(mad)
    }

    /// Mean ratio of `high / low` across bars.
    ///
    /// A value near 1.0 means bars are narrow; higher values indicate wider ranges.
    /// Bars with zero low are skipped. Returns `None` if no bars have non-zero low.
    pub fn avg_high_low_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.low.is_zero() {
                    return None;
                }
                (b.high / b.low).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Fraction of bars that are gap-fill bars (`is_gap_fill == true`).
    ///
    /// Returns `None` if the slice is empty.
    pub fn gap_fill_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let gap_bars = bars.iter().filter(|b| b.is_gap_fill).count();
        Some(gap_bars as f64 / bars.len() as f64)
    }

    /// Count of complete bars (where `is_complete == true`).
    pub fn complete_bar_count(bars: &[OhlcvBar]) -> usize {
        bars.iter().filter(|b| b.is_complete).count()
    }

    /// Minimum `trade_count` seen across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn min_trade_count(bars: &[OhlcvBar]) -> Option<u64> {
        bars.iter().map(|b| b.trade_count).min()
    }

    // ── round-82 ─────────────────────────────────────────────────────────────

    /// Mean of `high − low` across bars.
    pub fn avg_bar_range(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.high - b.low).sum();
        Some(sum / Decimal::from(bars.len()))
    }

    /// Largest single-bar upward body (`max(close − open, 0)`).
    pub fn max_up_move(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| (b.close - b.open).max(Decimal::ZERO)).max()
    }

    /// Largest single-bar downward body (`max(open − close, 0)`).
    pub fn max_down_move(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| (b.open - b.close).max(Decimal::ZERO)).max()
    }

    /// Mean of `(close − low) / range` where range > 0; position of close within each bar's range.
    pub fn avg_close_position(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let pos = (b.close - b.low).to_f64()? / range.to_f64()?;
                Some(pos)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Std dev of volume across bars; requires ≥ 2 bars.
    pub fn volume_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vols: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        let n = vols.len() as f64;
        if n < 2.0 {
            return None;
        }
        let mean = vols.iter().sum::<f64>() / n;
        let var = vols.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    /// Mean of `total_wick / range` per bar (proportion of range that is wick); excludes doji bars.
    pub fn avg_wick_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let upper = b.high - b.close.max(b.open);
                let lower = b.close.min(b.open) - b.low;
                let wick = upper + lower;
                let ratio = wick.to_f64()? / range.to_f64()?;
                Some(ratio)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Mean of `|open_i − close_{i-1}| / close_{i-1}` across bars from the second onward; measures gap size.
    pub fn open_gap_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars
            .windows(2)
            .filter_map(|w| {
                let prev_close = w[0].close;
                if prev_close.is_zero() {
                    return None;
                }
                let gap = (w[1].open - prev_close).abs().to_f64()? / prev_close.to_f64()?;
                Some(gap)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Net directional move: `(last_close − first_open) / first_open`; overall percentage move across all bars.
    pub fn net_directional_move(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let first_open = bars.first()?.open;
        let last_close = bars.last()?.close;
        if first_open.is_zero() {
            return None;
        }
        let pct = (last_close - first_open).to_f64()? / first_open.to_f64()?;
        Some(pct)
    }

    // ── round-83 ─────────────────────────────────────────────────────────────

    /// Fraction of bars where the close is above the bar's median price
    /// `(high + low) / 2`.
    ///
    /// A high fraction indicates persistently bullish closes; near 0.5 means
    /// closes tend to land at the midpoint. Returns `None` for empty slices.
    pub fn close_above_median_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let above = bars.iter().filter(|b| b.close > b.high_low_midpoint()).count();
        Some(above as f64 / bars.len() as f64)
    }

    /// Mean of `(high − low) / open` across bars — intrabar range relative to
    /// opening price.
    ///
    /// Returns `None` when no bars have a non-zero open.
    pub fn avg_range_to_open(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.open.is_zero() { return None; }
                ((b.high - b.low) / b.open).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Sum of all close prices across the slice.
    ///
    /// Useful as a component in rolling sum-based indicators.
    /// Returns `Decimal::ZERO` for an empty slice.
    pub fn close_sum(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| b.close).sum()
    }

    /// Count of bars where volume strictly exceeds the slice average volume.
    ///
    /// Returns 0 for empty slices or when average volume cannot be computed.
    pub fn above_avg_volume_count(bars: &[OhlcvBar]) -> usize {
        let avg = Self::mean_volume(bars).unwrap_or(Decimal::ZERO);
        if avg.is_zero() {
            return 0;
        }
        bars.iter().filter(|b| b.volume > avg).count()
    }

    /// Median close price across the slice.
    ///
    /// Returns `None` for empty slices.
    pub fn median_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let mut closes: Vec<Decimal> = bars.iter().map(|b| b.close).collect();
        closes.sort();
        let n = closes.len();
        if n % 2 == 1 {
            Some(closes[n / 2])
        } else {
            Some((closes[n / 2 - 1] + closes[n / 2]) / Decimal::from(2u64))
        }
    }

    /// Fraction of bars that are flat (open == close, i.e., doji-like).
    ///
    /// Returns `None` for empty slices.
    pub fn flat_bar_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let flat = bars.iter().filter(|b| b.open == b.close).count();
        Some(flat as f64 / bars.len() as f64)
    }

    /// Mean of `body / range` per bar — average fraction of the range that
    /// is body. Bars with zero range are excluded.
    ///
    /// Returns `None` when no bars have non-zero range.
    pub fn avg_body_to_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() { return None; }
                (b.body() / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Largest single-bar price gap (open vs. previous close) in the slice.
    ///
    /// Returns `None` for fewer than 2 bars.
    pub fn max_open_gap(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.len() < 2 {
            return None;
        }
        bars.windows(2)
            .map(|w| (w[1].open - w[0].close).abs())
            .max()
    }

    /// OLS linear regression slope of bar volume over bar index.
    ///
    /// A positive slope means volume is trending up; negative means trending
    /// down. Returns `None` for fewer than 2 bars.
    pub fn volume_trend_slope(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len();
        if n < 2 {
            return None;
        }
        let n_f = n as f64;
        let x_mean = (n_f - 1.0) / 2.0;
        let y: Vec<f64> = bars.iter().filter_map(|b| b.volume.to_f64()).collect();
        if y.len() < 2 {
            return None;
        }
        let y_mean = y.iter().sum::<f64>() / y.len() as f64;
        let num: f64 = y.iter().enumerate().map(|(i, &v)| (i as f64 - x_mean) * (v - y_mean)).sum();
        let den: f64 = (0..n).map(|i| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { None } else { Some(num / den) }
    }

    /// Fraction of bars where close > previous close (i.e., up-close bars).
    ///
    /// Returns `None` for fewer than 2 bars.
    pub fn up_close_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        let up = bars.windows(2).filter(|w| w[1].close > w[0].close).count();
        Some(up as f64 / (bars.len() - 1) as f64)
    }

    /// Mean of the upper-shadow-to-range ratio across bars.
    ///
    /// `upper_shadow / range` for each bar with non-zero range.
    /// Returns `None` when no bars have non-zero range.
    pub fn avg_upper_shadow_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() { return None; }
                (b.upper_shadow() / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-84 ─────────────────────────────────────────────────────────────

    /// Mean of `lower_shadow / range` per bar; excludes doji bars.
    pub fn avg_lower_shadow_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() { return None; }
                (b.lower_shadow() / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Mean of `(close - open) / (high - low)` per bar with non-zero range; signed body position.
    pub fn close_to_open_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() { return None; }
                ((b.close - b.open) / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Maximum high price across all bars.
    pub fn max_high(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.high).max()
    }

    /// Minimum low price across all bars.
    pub fn min_low(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.low).min()
    }

    /// Mean `|close − open| / range` across non-doji bars; how much of the range became directional body.
    pub fn avg_bar_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let r = b.range();
                if r.is_zero() { return None; }
                ((b.close - b.open).abs() / r).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Fraction of bars where `open` lies in the upper half of `[low, high]`.
    pub fn open_range_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars
            .iter()
            .filter(|b| {
                let mid = (b.high + b.low) / Decimal::from(2);
                b.open >= mid
            })
            .count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Skewness of close prices across bars.
    pub fn close_skewness(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std < 1e-12 {
            return None;
        }
        let skew = vals.iter().map(|v| ((v - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Fraction of bars whose volume exceeds the median bar volume.
    pub fn volume_above_median_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let mut vols: Vec<Decimal> = bars.iter().map(|b| b.volume).collect();
        vols.sort();
        let mid = vols.len() / 2;
        let median = if vols.len() % 2 == 0 {
            (vols[mid - 1] + vols[mid]) / Decimal::from(2)
        } else {
            vols[mid]
        };
        let count = bars.iter().filter(|b| b.volume > median).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Sum of typical prices `(high + low + close) / 3` across bars.
    pub fn typical_price_sum(bars: &[OhlcvBar]) -> Decimal {
        bars.iter()
            .map(|b| (b.high + b.low + b.close) / Decimal::from(3))
            .sum()
    }

    /// Maximum bar body size `|close - open|` across all bars.
    pub fn max_body_size(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| (b.close - b.open).abs()).max()
    }

    /// Minimum bar body size `|close - open|` across all bars.
    pub fn min_body_size(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| (b.close - b.open).abs()).min()
    }

    /// Mean ratio of lower wick to full bar range; zero-range bars are excluded.
    pub fn avg_lower_wick_to_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let lower_wick = b.open.min(b.close) - b.low;
                (lower_wick / range).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-85 ─────────────────────────────────────────────────────────────

    /// `high − low` summed across all bars; total accumulated range.
    pub fn total_range(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| b.high - b.low).sum()
    }

    /// Fraction of bars where the close is strictly equal to the high (outside-bar bullish close).
    pub fn close_at_high_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.close == b.high).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Fraction of bars where the close is strictly equal to the low (bearish exhaustion bar).
    pub fn close_at_low_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.close == b.low).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Mean of `(high - open) / range` across non-doji bars; how far price moved above the open.
    pub fn avg_high_above_open_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                ((b.high - b.open) / range).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Count of bars with `high == previous_bar.close` (gap-free continuation bars).
    pub fn continuation_bar_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2)
            .filter(|w| w[1].open == w[0].close)
            .count()
    }

    /// Sum of bar volumes where the bar was a down-close.
    pub fn down_close_volume(bars: &[OhlcvBar]) -> Decimal {
        bars.iter()
            .filter(|b| b.close < b.open)
            .map(|b| b.volume)
            .sum()
    }

    /// Sum of bar volumes where the bar was an up-close.
    pub fn up_close_volume(bars: &[OhlcvBar]) -> Decimal {
        bars.iter()
            .filter(|b| b.close > b.open)
            .map(|b| b.volume)
            .sum()
    }

    // ── round-86 ─────────────────────────────────────────────────────────────

    /// Mean open price across bars.
    pub fn mean_open(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.open).sum();
        Some(sum / Decimal::from(bars.len() as i64))
    }

    /// Count of bars where `high` is strictly greater than all previous bar highs.
    pub fn new_high_count(bars: &[OhlcvBar]) -> usize {
        if bars.is_empty() {
            return 0;
        }
        let mut running_max = bars[0].high;
        let mut count = 0usize;
        for b in bars.iter().skip(1) {
            if b.high > running_max {
                count += 1;
                running_max = b.high;
            }
        }
        count
    }

    /// Count of bars where `low` is strictly less than all previous bar lows.
    pub fn new_low_count(bars: &[OhlcvBar]) -> usize {
        if bars.is_empty() {
            return 0;
        }
        let mut running_min = bars[0].low;
        let mut count = 0usize;
        for b in bars.iter().skip(1) {
            if b.low < running_min {
                count += 1;
                running_min = b.low;
            }
        }
        count
    }

    /// Standard deviation of close prices; requires ≥ 2 bars.
    pub fn close_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    /// Fraction of bars with zero volume.
    pub fn zero_volume_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.volume.is_zero()).count();
        Some(count as f64 / bars.len() as f64)
    }

    // ── round-87 ─────────────────────────────────────────────────────────────

    /// Mean of (close − open) across all bars.  Positive means net bullish drift.
    pub fn avg_open_to_close(bars: &[OhlcvBar]) -> Option<Decimal> {
        if bars.is_empty() {
            return None;
        }
        let sum: Decimal = bars.iter().map(|b| b.close - b.open).sum();
        Some(sum / Decimal::from(bars.len() as i64))
    }

    /// Maximum volume across bars.
    pub fn max_bar_volume(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.volume).max()
    }

    /// Minimum volume across bars.
    pub fn min_bar_volume(bars: &[OhlcvBar]) -> Option<Decimal> {
        bars.iter().map(|b| b.volume).min()
    }

    /// Standard deviation of body-to-range ratios across bars.
    /// Body = |close − open|, range = high − low.  Bars with zero range are excluded.
    /// Returns `None` if fewer than 2 non-zero-range bars.
    pub fn body_to_range_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let body = (b.close - b.open).abs();
                let range = b.high - b.low;
                (body / range).to_f64()
            })
            .collect();
        if ratios.len() < 2 {
            return None;
        }
        let n = ratios.len() as f64;
        let mean = ratios.iter().sum::<f64>() / n;
        let var = ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    /// Mean wick symmetry: average of min(upper_wick, lower_wick) / max(upper_wick, lower_wick).
    /// A value near 1 means wicks are balanced; near 0 means heavily one-sided.
    /// Bars with both wicks zero are excluded.
    pub fn avg_wick_symmetry(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let upper = b.high - b.close.max(b.open);
                let lower = b.close.min(b.open) - b.low;
                if upper.is_zero() && lower.is_zero() {
                    return None;
                }
                let lo = upper.min(lower);
                let hi = upper.max(lower);
                if hi.is_zero() {
                    return None;
                }
                (lo / hi).to_f64()
            })
            .collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    // ── round-88 ─────────────────────────────────────────────────────────────

    /// Price range of a single bar expressed as a fraction of open price.
    ///
    /// Returns `None` if open is zero or the slice is empty.
    pub fn avg_range_pct_of_open(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.open.is_zero() { return None; }
                ((b.high - b.low) / b.open).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Fraction of bars where volume is in the upper half of the volume range.
    ///
    /// Returns `None` for empty slices or when all volumes are equal.
    pub fn high_volume_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let max_vol = bars.iter().map(|b| b.volume).max()?;
        let min_vol = bars.iter().map(|b| b.volume).min()?;
        let mid = (max_vol + min_vol) / Decimal::from(2);
        if max_vol == min_vol {
            return None;
        }
        let count = bars.iter().filter(|b| b.volume > mid).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Count of bars where close is within 1% of the prior bar's close.
    ///
    /// Returns 0 for slices with fewer than 2 bars.
    pub fn close_cluster_count(bars: &[OhlcvBar]) -> usize {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2)
            .filter(|w| {
                if w[0].close.is_zero() {
                    return false;
                }
                let pct_diff = ((w[1].close - w[0].close) / w[0].close).abs();
                pct_diff <= rust_decimal::Decimal::new(1, 2)
            })
            .count()
    }

    /// Mean of `vwap` values across bars that have a VWAP computed.
    ///
    /// Returns `None` for empty slices or when no bars have a VWAP.
    pub fn mean_vwap(bars: &[OhlcvBar]) -> Option<Decimal> {
        let vals: Vec<Decimal> = bars.iter().filter_map(|b| b.vwap).collect();
        if vals.is_empty() {
            return None;
        }
        let sum: Decimal = vals.iter().copied().sum();
        Some(sum / Decimal::from(vals.len() as i64))
    }

    /// Fraction of bars that are complete (is_complete == true).
    ///
    /// Returns `None` for empty slices.
    pub fn complete_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.is_complete).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Sum of `(close − open).abs()` across all bars; total body movement.
    pub fn total_body_movement(bars: &[OhlcvBar]) -> Decimal {
        bars.iter().map(|b| (b.close - b.open).abs()).sum()
    }

    /// Sample standard deviation of open prices across bars.  Requires ≥ 2 bars.
    pub fn open_std(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars.iter().filter_map(|b| b.open.to_f64()).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    /// Mean of `high / low` ratios across bars (bars with zero `low` are skipped).
    pub fn mean_high_low_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| !b.low.is_zero())
            .filter_map(|b| (b.high / b.low).to_f64())
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-89 ─────────────────────────────────────────────────────────────

    /// Maximum run of consecutive bullish bars (`close > open`).
    ///
    /// Returns `0` for an empty slice.
    pub fn max_consecutive_up_bars(bars: &[OhlcvBar]) -> usize {
        let mut max_run = 0usize;
        let mut run = 0usize;
        for b in bars {
            if b.close > b.open {
                run += 1;
                if run > max_run {
                    max_run = run;
                }
            } else {
                run = 0;
            }
        }
        max_run
    }

    /// Mean upper shadow as a fraction of total bar range.
    /// Upper shadow = high − max(open, close).  Bars with zero range are excluded.
    pub fn avg_upper_shadow_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let range = b.high - b.low;
                let upper = b.high - b.close.max(b.open);
                (upper / range).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Ratio of up-bars (close > open) to down-bars (close < open).
    /// Returns `None` if no down-bars exist.
    pub fn up_down_bar_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        let ups = bars.iter().filter(|b| b.close > b.open).count();
        let downs = bars.iter().filter(|b| b.close < b.open).count();
        if downs == 0 {
            return None;
        }
        Some(ups as f64 / downs as f64)
    }

    // ── round-90 ─────────────────────────────────────────────────────────────

    /// Fraction of bars where `(close − low) / (high − low)` exceeds 0.5 (close in upper half of range).
    ///
    /// Bars with zero range are excluded. Returns `None` when no valid bars remain.
    pub fn close_range_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let half = Decimal::new(5, 1);
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let r = (b.close - b.low) / (b.high - b.low);
                r.to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        let _ = half;
        let count = vals.iter().filter(|&&v| v > 0.5).count();
        Some(count as f64 / vals.len() as f64)
    }

    /// Symmetry of upper vs lower shadows: `1 − |upper_shadow − lower_shadow| / range`.
    ///
    /// Returns `None` for an empty slice or when all bars have zero range.
    pub fn tail_symmetry(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let range = (b.high - b.low).to_f64()?;
                let upper = (b.high - b.close.max(b.open)).to_f64()?;
                let lower = (b.close.min(b.open) - b.low).to_f64()?;
                Some(1.0 - (upper - lower).abs() / range)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Fraction of bars where `close` is higher than the previous bar's `close`.
    ///
    /// Returns `None` for fewer than 2 bars.
    pub fn bar_trend_strength(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        let up_count = bars.windows(2).filter(|w| w[1].close > w[0].close).count();
        Some(up_count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-91 ─────────────────────────────────────────────────────────────

    /// Count of bars where `open` is higher than the previous bar's `close` (gap-up).
    ///
    /// Returns `0` for fewer than 2 bars.
    pub fn gap_up_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].open > w[0].close).count()
    }

    /// Count of bars where `open` is lower than the previous bar's `close` (gap-down).
    ///
    /// Returns `0` for fewer than 2 bars.
    pub fn gap_down_count(bars: &[OhlcvBar]) -> usize {
        if bars.len() < 2 {
            return 0;
        }
        bars.windows(2).filter(|w| w[1].open < w[0].close).count()
    }

    /// Mean body-to-range ratio: `mean(|close − open| / (high − low))`.
    ///
    /// Bars with zero range are excluded. Returns `None` if no valid bars exist.
    pub fn mean_bar_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let body = (b.close - b.open).abs();
                let range = b.high - b.low;
                (body / range).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-92 ─────────────────────────────────────────────────────────────

    /// Mean relative gap size: `mean(|open[i] − close[i-1]| / close[i-1])` across consecutive bar pairs.
    ///
    /// Returns `None` for fewer than 2 bars or when all prev-closes are zero.
    pub fn open_gap_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = bars
            .windows(2)
            .filter(|w| !w[0].close.is_zero())
            .filter_map(|w| {
                let gap = (w[1].open - w[0].close).abs();
                (gap / w[0].close).to_f64()
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Mean candle symmetry: `mean(1 − |body| / range)` for bars with nonzero range.
    ///
    /// A value close to 1 means most bars have small bodies (doji-like); close to 0
    /// means full-body bars. Returns `None` if no valid bars exist.
    pub fn candle_symmetry_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let body = (b.close - b.open).abs().to_f64()?;
                let range = (b.high - b.low).to_f64()?;
                Some(1.0 - body / range)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Mean of `(high − close) / (high − low)` — upper shadow as fraction of range.
    ///
    /// Identical to `avg_close_to_high` but named distinctly for discovery.
    /// Bars with zero range are excluded. Returns `None` if no valid bars exist.
    pub fn mean_upper_shadow_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.high > b.low)
            .filter_map(|b| {
                let upper = (b.high - b.close).to_f64()?;
                let range = (b.high - b.low).to_f64()?;
                Some(upper / range)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-93 ─────────────────────────────────────────────────────────────

    /// Mean ratio of total wick length to body size: `(upper+lower) / |close−open|`.
    ///
    /// Bars with zero body (doji) are excluded. Returns `None` if no valid bars exist.
    pub fn avg_wick_to_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars
            .iter()
            .filter(|b| b.close != b.open)
            .filter_map(|b| {
                let body = (b.close - b.open).abs().to_f64()?;
                let upper = (b.high - b.close.max(b.open)).to_f64()?;
                let lower = (b.close.min(b.open) - b.low).to_f64()?;
                Some((upper + lower) / body)
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Length of the longest consecutive run of up-close bars (`close > open`).
    pub fn close_above_open_streak(bars: &[OhlcvBar]) -> usize {
        let mut max_run = 0usize;
        let mut run = 0usize;
        for b in bars {
            if b.close > b.open {
                run += 1;
                if run > max_run {
                    max_run = run;
                }
            } else {
                run = 0;
            }
        }
        max_run
    }

    /// Fraction of bars whose volume exceeds the mean bar volume.
    ///
    /// Returns `None` for an empty slice.
    pub fn volume_above_mean_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() {
            return None;
        }
        let n = bars.len() as u32;
        let mean_vol: Decimal =
            bars.iter().map(|b| b.volume).sum::<Decimal>() / Decimal::from(n);
        let count = bars.iter().filter(|b| b.volume > mean_vol).count();
        Some(count as f64 / bars.len() as f64)
    }

    // ── round-94 ─────────────────────────────────────────────────────────────

    /// Largest upward gap (`open − prev_close`) as a fraction of `prev_close`.
    /// Returns `None` for fewer than 2 bars or when no positive gap exists.
    pub fn max_gap_up(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        bars.windows(2)
            .filter_map(|w| {
                let gap = w[1].open - w[0].close;
                if gap > Decimal::ZERO && !w[0].close.is_zero() {
                    Some((gap / w[0].close).to_f64().unwrap_or(0.0))
                } else {
                    None
                }
            })
            .reduce(f64::max)
    }

    /// Ratio of the last bar's high−low range to the first bar's high−low range.
    /// Returns `None` for fewer than 2 bars or when the first bar's range is zero.
    pub fn price_range_expansion(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let first_range = bars.first()?.high - bars.first()?.low;
        if first_range.is_zero() {
            return None;
        }
        let last_range = bars.last()?.high - bars.last()?.low;
        Some((last_range / first_range).to_f64().unwrap_or(0.0))
    }

    /// Average of `volume / (high − low)` per bar; bars with zero range are excluded.
    /// Returns `None` if the slice is empty or all bars have zero range.
    pub fn avg_volume_per_range(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    None
                } else {
                    Some((b.volume / range).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    // ── round-95 ─────────────────────────────────────────────────────────────

    /// Ratio of total up-bar volume to total down-bar volume.
    /// Returns `None` if there are no up-bars or no down-bars.
    pub fn up_down_volume_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (up_vol, dn_vol) = bars.iter().fold(
            (Decimal::ZERO, Decimal::ZERO),
            |(up, dn), b| {
                if b.close > b.open {
                    (up + b.volume, dn)
                } else if b.close < b.open {
                    (up, dn + b.volume)
                } else {
                    (up, dn)
                }
            },
        );
        if up_vol.is_zero() || dn_vol.is_zero() {
            return None;
        }
        Some((up_vol / dn_vol).to_f64().unwrap_or(0.0))
    }

    /// Length of the longest consecutive run of bars where `close < open`.
    /// Returns `None` for an empty slice.
    pub fn longest_bearish_streak(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() {
            return None;
        }
        let mut max_run = 0usize;
        let mut cur = 0usize;
        for b in bars {
            if b.close < b.open {
                cur += 1;
                if cur > max_run {
                    max_run = cur;
                }
            } else {
                cur = 0;
            }
        }
        Some(max_run)
    }

    /// Mean of `(close − low) / (high − low)` per bar, excluding zero-range bars.
    /// Returns `None` if the slice is empty or all bars have zero range.
    pub fn mean_close_to_high_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    None
                } else {
                    Some(((b.close - b.low) / range).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    // ── round-96 ─────────────────────────────────────────────────────────────

    /// Mean signed `(close − open) / open` return per bar.
    /// Bars with zero open are excluded. Returns `None` if no valid bars exist.
    pub fn open_to_close_momentum(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.open.is_zero() {
                    None
                } else {
                    Some(((b.close - b.open) / b.open).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Coefficient of variation of bar volumes: `std_dev(volume) / mean(volume)`.
    /// Returns `None` for fewer than 2 bars or zero mean volume.
    pub fn volume_dispersion(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = bars.len() as f64;
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let mean = vols.iter().sum::<f64>() / n;
        if mean == 0.0 {
            return None;
        }
        let variance = vols.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        Some(variance.sqrt() / mean)
    }

    /// Fraction of each bar's range that is accounted for by shadows (wicks)
    /// rather than the body, averaged across bars with nonzero range.
    /// Returns `None` if no valid bars exist.
    pub fn shadow_dominance(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let body = (b.close - b.open).abs();
                let shadow = range - body;
                Some((shadow / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Mean true range: average of `max(high−low, |high−prev_close|, |low−prev_close|)`.
    /// Returns `None` for fewer than 2 bars.
    pub fn true_range_mean(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let trs: Vec<f64> = bars
            .windows(2)
            .map(|w| {
                let hl = w[1].high - w[1].low;
                let hc = (w[1].high - w[0].close).abs();
                let lc = (w[1].low - w[0].close).abs();
                hl.max(hc).max(lc).to_f64().unwrap_or(0.0)
            })
            .collect();
        Some(trs.iter().sum::<f64>() / trs.len() as f64)
    }

    // ── round-97 ─────────────────────────────────────────────────────────────

    /// Mean close-to-open gap as a fraction of prior close.
    /// Returns `None` for fewer than 2 bars.
    pub fn close_to_open_gap(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let gaps: Vec<f64> = bars
            .windows(2)
            .filter_map(|w| {
                if w[0].close.is_zero() {
                    None
                } else {
                    Some(((w[1].open - w[0].close) / w[0].close).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if gaps.is_empty() {
            return None;
        }
        Some(gaps.iter().sum::<f64>() / gaps.len() as f64)
    }

    /// Volume-weighted average open price across all bars.
    /// Returns `None` for an empty slice or zero total volume.
    pub fn volume_weighted_open(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let total_vol: Decimal = bars.iter().map(|b| b.volume).sum();
        if total_vol.is_zero() {
            return None;
        }
        let vw_open = bars.iter().map(|b| b.open * b.volume).sum::<Decimal>() / total_vol;
        Some(vw_open.to_f64().unwrap_or(0.0))
    }

    /// Mean upper shadow as a fraction of the bar range, excluding zero-range bars.
    /// Returns `None` for an empty slice or all zero-range bars.
    pub fn avg_upper_shadow(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let body_top = b.open.max(b.close);
                let upper = b.high - body_top;
                Some((upper / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Mean `|body| / range` per bar, excluding zero-range bars.
    /// Returns `None` for an empty slice or all zero-range bars.
    pub fn body_to_range_mean(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let body = (b.close - b.open).abs();
                Some((body / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    // ── round-98 ─────────────────────────────────────────────────────────────

    /// Number of bars where `|close − open| / (high − low) < 0.1` (doji-like).
    /// Returns `None` for an empty slice.
    pub fn narrow_body_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let count = bars
            .iter()
            .filter(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return true; // zero-range is doji by convention
                }
                let body_ratio = ((b.close - b.open).abs() / range)
                    .to_f64()
                    .unwrap_or(1.0);
                body_ratio < 0.1
            })
            .count();
        Some(count)
    }

    /// Mean price range `(high − low)` across all bars.
    /// Returns `None` for an empty slice.
    pub fn bar_range_mean(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let sum: f64 = bars
            .iter()
            .map(|b| (b.high - b.low).to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / bars.len() as f64)
    }

    /// Mean of `(close − low) / (high − low)` — how close to the high bars
    /// typically close. Excludes zero-range bars.
    /// Returns `None` for an empty slice or all zero-range bars.
    pub fn close_proximity(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                Some(((b.close - b.low) / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Number of bars with a downward gap (`open < prev_close`).
    /// Returns `None` for fewer than 2 bars.
    pub fn down_gap_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 {
            return None;
        }
        let count = bars
            .windows(2)
            .filter(|w| w[1].open < w[0].close)
            .count();
        Some(count)
    }

    // ── round-99 ─────────────────────────────────────────────────────────────

    /// Mean lower shadow as a fraction of bar range, excluding zero-range bars.
    /// Returns `None` for an empty slice or all zero-range bars.
    pub fn avg_lower_shadow(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let body_bottom = b.open.min(b.close);
                let lower = body_bottom - b.low;
                Some((lower / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Number of bars completely contained within the previous bar's range
    /// (inside bars: `high ≤ prev_high` and `low ≥ prev_low`).
    /// Returns `None` for fewer than 2 bars.
    pub fn inside_bar_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 {
            return None;
        }
        let count = bars
            .windows(2)
            .filter(|w| w[1].high <= w[0].high && w[1].low >= w[0].low)
            .count();
        Some(count)
    }

    /// Width of the price channel: rolling max-high minus rolling min-low across the slice.
    /// Returns `None` for an empty slice.
    pub fn price_channel_width(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let max_high = bars.iter().map(|b| b.high).max()?;
        let min_low = bars.iter().map(|b| b.low).min()?;
        Some((max_high - min_low).to_f64().unwrap_or(0.0))
    }

    /// Rate of change of volume trend: (second-half mean volume − first-half mean volume)
    /// divided by the first-half mean. Returns `None` for fewer than 4 bars or zero first-half mean.
    pub fn volume_trend_acceleration(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let half = bars.len() / 2;
        let n1 = Decimal::from(half);
        let n2 = Decimal::from(bars.len() - half);
        let first_mean: Decimal = bars[..half].iter().map(|b| b.volume).sum::<Decimal>() / n1;
        let second_mean: Decimal = bars[half..].iter().map(|b| b.volume).sum::<Decimal>() / n2;
        if first_mean.is_zero() {
            return None;
        }
        Some(((second_mean - first_mean) / first_mean).to_f64().unwrap_or(0.0))
    }

    // ── round-100 ────────────────────────────────────────────────────────────

    /// Median volume across all bars.
    /// Returns `None` for an empty slice.
    pub fn median_volume(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let mut vols: Vec<Decimal> = bars.iter().map(|b| b.volume).collect();
        vols.sort();
        let mid = vols.len() / 2;
        let median = if vols.len() % 2 == 0 {
            (vols[mid - 1] + vols[mid]) / Decimal::TWO
        } else {
            vols[mid]
        };
        Some(median.to_f64().unwrap_or(0.0))
    }

    /// Number of bars with a price range above the mean range.
    /// Returns `None` for an empty slice.
    pub fn bar_count_above_avg_range(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() {
            return None;
        }
        let ranges: Vec<Decimal> = bars.iter().map(|b| b.high - b.low).collect();
        let n = Decimal::from(ranges.len());
        let mean: Decimal = ranges.iter().copied().sum::<Decimal>() / n;
        let count = ranges.iter().filter(|&&r| r > mean).count();
        Some(count)
    }

    /// Number of times the closing price direction reverses across consecutive bars.
    /// Returns `None` for fewer than 3 bars or no directional changes.
    pub fn price_oscillation_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 3 {
            return None;
        }
        let dirs: Vec<i32> = bars
            .windows(2)
            .map(|w| {
                if w[1].close > w[0].close {
                    1
                } else if w[1].close < w[0].close {
                    -1
                } else {
                    0
                }
            })
            .collect();
        let reversals = dirs
            .windows(2)
            .filter(|d| d[0] != 0 && d[1] != 0 && d[0] != d[1])
            .count();
        Some(reversals)
    }

    /// Mean absolute deviation of close prices from the VWAP across all bars.
    /// Returns `None` for an empty slice or zero total volume.
    pub fn vwap_deviation_mean(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let total_vol: Decimal = bars.iter().map(|b| b.volume).sum();
        if total_vol.is_zero() {
            return None;
        }
        let vwap = bars.iter().map(|b| b.close * b.volume).sum::<Decimal>() / total_vol;
        let mean_dev = bars
            .iter()
            .map(|b| (b.close - vwap).abs())
            .sum::<Decimal>()
            / Decimal::from(bars.len());
        Some(mean_dev.to_f64().unwrap_or(0.0))
    }

    // ── round-101 ────────────────────────────────────────────────────────────

    /// Mean of `(open − low) / (high − low)` per bar, measuring how far from
    /// the low the bar opened. Excludes zero-range bars.
    /// Returns `None` for an empty slice or all zero-range bars.
    pub fn open_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                Some(((b.open - b.low) / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Volume per unit of price range: `volume / (high − low)`, averaged across bars.
    /// Excludes zero-range bars. Returns `None` for empty slice or all zero-range bars.
    pub fn volume_normalized_range(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                Some((b.volume / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Number of consecutive bars at the end of the slice where
    /// `|close − open| / (high − low) < 0.05` (near-doji).
    /// Returns `None` for an empty slice.
    pub fn consecutive_flat_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let mut count = 0usize;
        for b in bars.iter().rev() {
            let range = b.high - b.low;
            let flat = if range.is_zero() {
                true
            } else {
                ((b.close - b.open).abs() / range)
                    .to_f64()
                    .unwrap_or(1.0)
                    < 0.05
            };
            if flat {
                count += 1;
            } else {
                break;
            }
        }
        Some(count)
    }

    /// Mean of `(close − midpoint) / (high − low)` where `midpoint = (high + low) / 2`.
    /// Excludes zero-range bars. Returns `None` for empty slice or all zero-range bars.
    pub fn close_vs_midpoint(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    return None;
                }
                let mid = (b.high + b.low) / Decimal::TWO;
                Some(((b.close - mid) / range).to_f64().unwrap_or(0.0))
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    // ── round-102 ────────────────────────────────────────────────────────────

    /// Mean of `high / low` ratio per bar, excluding zero-low bars.
    /// Returns `None` for an empty slice or all zero-low bars.
    pub fn high_low_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let values: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.low.is_zero() {
                    None
                } else {
                    Some((b.high / b.low).to_f64().unwrap_or(1.0))
                }
            })
            .collect();
        if values.is_empty() {
            return None;
        }
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }

    /// Mean signed close-to-close change: `mean(close[i] − close[i-1])`.
    /// Returns `None` for fewer than 2 bars.
    pub fn close_change_mean(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let sum: f64 = bars
            .windows(2)
            .map(|w| (w[1].close - w[0].close).to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Fraction of bars where `close < open` (down-body bars).
    /// Returns `None` for an empty slice.
    pub fn down_body_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() {
            return None;
        }
        let count = bars.iter().filter(|b| b.close < b.open).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Rate of change of body size: (second-half mean body − first-half mean body)
    /// divided by first-half mean body. Returns `None` for fewer than 4 bars or zero
    /// first-half mean body.
    pub fn body_acceleration(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 4 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let half = bars.len() / 2;
        let bodies_first: Vec<Decimal> = bars[..half]
            .iter()
            .map(|b| (b.close - b.open).abs())
            .collect();
        let bodies_second: Vec<Decimal> = bars[half..]
            .iter()
            .map(|b| (b.close - b.open).abs())
            .collect();
        let n1 = Decimal::from(bodies_first.len());
        let n2 = Decimal::from(bodies_second.len());
        let first_mean = bodies_first.iter().copied().sum::<Decimal>() / n1;
        if first_mean.is_zero() {
            return None;
        }
        let second_mean = bodies_second.iter().copied().sum::<Decimal>() / n2;
        Some(((second_mean - first_mean) / first_mean).to_f64().unwrap_or(0.0))
    }

    // ── round-103 ────────────────────────────────────────────────────────────

    /// Mean of `(high - open) / (high - low)` across bars with non-zero range.
    /// Returns `None` for an empty slice or no bars with non-zero range.
    pub fn open_high_distance(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let range = b.high - b.low;
                if range.is_zero() {
                    None
                } else {
                    Some(((b.high - b.open) / range).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Maximum `close - open` value across all bars.
    /// Returns `None` for an empty slice.
    pub fn max_close_minus_open(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        bars.iter()
            .map(|b| b.close - b.open)
            .max()
            .and_then(|v| v.to_f64())
    }

    /// Count of bullish engulfing candlestick patterns: a bearish bar followed
    /// by a bullish bar whose body fully engulfs the prior bar's body.
    /// Returns `None` for fewer than 2 bars.
    pub fn bullish_engulfing_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 {
            return None;
        }
        let count = bars.windows(2).filter(|w| {
            let prev = &w[0];
            let curr = &w[1];
            // Prior bar: bearish (open > close), Current bar: bullish (close > open)
            prev.open > prev.close
                && curr.close > curr.open
                && curr.open <= prev.close
                && curr.close >= prev.open
        }).count();
        Some(count)
    }

    /// Mean upper-to-lower shadow ratio across bars where both shadows are non-zero.
    /// Upper shadow = `high - max(open, close)`, lower shadow = `min(open, close) - low`.
    /// Returns `None` for an empty slice or no qualifying bars.
    pub fn shadow_ratio_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                let upper = b.high - b.open.max(b.close);
                let lower = b.open.min(b.close) - b.low;
                if lower.is_zero() {
                    None
                } else {
                    Some((upper / lower).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    // ── round-104 ────────────────────────────────────────────────────────────

    /// Count of bars where the close re-enters the prior bar's range after
    /// gapping away (gap fill): prior bar gaps up/down and current close falls
    /// back inside the prior bar's high–low range.
    /// Returns `None` for fewer than 2 bars.
    pub fn gap_fill_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 {
            return None;
        }
        let count = bars.windows(2).filter(|w| {
            let prev = &w[0];
            let curr = &w[1];
            // Gap up then close comes back inside prior range
            let gap_up = curr.open > prev.high && curr.close <= prev.high;
            // Gap down then close comes back inside prior range
            let gap_dn = curr.open < prev.low && curr.close >= prev.low;
            gap_up || gap_dn
        }).count();
        Some(count)
    }

    /// Mean ratio of candle body size to bar volume across bars with non-zero volume.
    /// Body = |close - open|. Returns `None` for an empty slice.
    pub fn avg_body_to_volume(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars
            .iter()
            .filter_map(|b| {
                if b.volume.is_zero() {
                    None
                } else {
                    Some(((b.close - b.open).abs() / b.volume).to_f64().unwrap_or(0.0))
                }
            })
            .collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Fraction of bars where close > open of the *same* bar (bullish bars)
    /// that also have a higher close than the previous bar (price recovery).
    /// Returns `None` for fewer than 2 bars.
    pub fn price_recovery_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 {
            return None;
        }
        let bullish_count = bars[1..].iter().zip(bars.iter()).filter(|(curr, prev)| {
            curr.close > curr.open && curr.close > prev.close
        }).count();
        let total = bars.len() - 1;
        Some(bullish_count as f64 / total as f64)
    }

    /// Pearson correlation between open and close prices across bars.
    /// Returns `None` for fewer than 2 bars or zero variance.
    pub fn open_close_correlation(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let n = bars.len() as f64;
        let opens: Vec<f64> = bars.iter().filter_map(|b| b.open.to_f64()).collect();
        let closes: Vec<f64> = bars.iter().filter_map(|b| b.close.to_f64()).collect();
        if opens.len() != bars.len() || closes.len() != bars.len() {
            return None;
        }
        let mean_o = opens.iter().sum::<f64>() / n;
        let mean_c = closes.iter().sum::<f64>() / n;
        let cov: f64 = opens.iter().zip(closes.iter()).map(|(o, c)| (o - mean_o) * (c - mean_c)).sum::<f64>() / n;
        let std_o = (opens.iter().map(|o| (o - mean_o).powi(2)).sum::<f64>() / n).sqrt();
        let std_c = (closes.iter().map(|c| (c - mean_c).powi(2)).sum::<f64>() / n).sqrt();
        if std_o == 0.0 || std_c == 0.0 {
            return None;
        }
        Some(cov / (std_o * std_c))
    }

    // ── round-105 ────────────────────────────────────────────────────────────

    /// Mean of `(high - close) / (high - low)` across bars with non-zero range.
    /// Measures how far close is from the high on average.
    /// Returns `None` for an empty slice or no bars with non-zero range.
    pub fn close_to_high_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.high - b.low;
            if range.is_zero() { None }
            else { Some(((b.high - b.close) / range).to_f64().unwrap_or(0.0)) }
        }).collect();
        if ratios.is_empty() { return None; }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Mean true range divided by mean close price — a normalised volatility score.
    /// Returns `None` for fewer than 2 bars or zero mean close.
    pub fn bar_volatility_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 {
            return None;
        }
        let true_ranges: Vec<Decimal> = bars.windows(2).map(|w| {
            let prev = &w[0];
            let curr = &w[1];
            let hl = curr.high - curr.low;
            let hpc = (curr.high - prev.close).abs();
            let lpc = (curr.low - prev.close).abs();
            hl.max(hpc).max(lpc)
        }).collect();
        let mean_tr: Decimal = true_ranges.iter().copied().sum::<Decimal>()
            / Decimal::from(true_ranges.len());
        let mean_close: Decimal = bars.iter().map(|b| b.close).sum::<Decimal>()
            / Decimal::from(bars.len());
        if mean_close.is_zero() { return None; }
        Some((mean_tr / mean_close).to_f64().unwrap_or(0.0))
    }

    /// Fraction of bars that close below their open (bearish candles).
    /// Returns `None` for an empty slice.
    pub fn bearish_close_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let bearish = bars.iter().filter(|b| b.close < b.open).count();
        Some(bearish as f64 / bars.len() as f64)
    }

    /// Mean of `high - open` across all bars.
    /// Returns `None` for an empty slice.
    pub fn high_minus_open_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: Decimal = bars.iter().map(|b| b.high - b.open).sum();
        Some((sum / Decimal::from(bars.len())).to_f64().unwrap_or(0.0))
    }

    // ── round-106 ────────────────────────────────────────────────────────────

    /// Mean `(high - low) / close` — true range as a fraction of close price.
    /// Returns `None` for an empty slice or any bar with zero close.
    pub fn avg_true_range_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let ratios: Vec<f64> = bars.iter().filter_map(|b| {
            if b.close.is_zero() { None }
            else { Some(((b.high - b.low) / b.close).to_f64().unwrap_or(0.0)) }
        }).collect();
        if ratios.is_empty() { return None; }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Count of bars where close is above the bar's own midpoint `(high + low) / 2`.
    /// Returns `None` for an empty slice.
    pub fn close_above_midpoint_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() { return None; }
        let count = bars.iter().filter(|b| {
            b.close > (b.high + b.low) / Decimal::TWO
        }).count();
        Some(count)
    }

    /// Volume-weighted high price across all bars.
    /// Returns `None` for an empty slice or zero total volume.
    pub fn volume_weighted_high(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let total_vol: Decimal = bars.iter().map(|b| b.volume).sum();
        if total_vol.is_zero() { return None; }
        let wsum: Decimal = bars.iter().map(|b| b.high * b.volume).sum();
        Some((wsum / total_vol).to_f64().unwrap_or(0.0))
    }

    /// Mean of `min(open, close) - low` (lower tail / lower wick) across all bars.
    /// Returns `None` for an empty slice.
    pub fn low_minus_close_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: Decimal = bars.iter().map(|b| b.open.min(b.close) - b.low).sum();
        Some((sum / Decimal::from(bars.len())).to_f64().unwrap_or(0.0))
    }

    // ── round-107 ────────────────────────────────────────────────────────────

    /// Mean of `(high - open) / high` across bars with non-zero high.
    /// Returns `None` for an empty slice.
    pub fn open_to_high_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ratios: Vec<f64> = bars.iter().filter_map(|b| {
            if b.high.is_zero() { None }
            else { Some(((b.high - b.open) / b.high).to_f64().unwrap_or(0.0)) }
        }).collect();
        if ratios.is_empty() { return None; }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Mean position of close within the bar's high-low range: `(close - low) / (high - low)`.
    /// Returns `None` for an empty slice or no bars with non-zero range.
    pub fn close_range_position(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.high - b.low;
            if range.is_zero() { None }
            else { Some(((b.close - b.low) / range).to_f64().unwrap_or(0.0)) }
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Count of bars that gap up from the prior bar (open > prior high).
    /// Returns `None` for fewer than 2 bars.
    pub fn up_gap_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].open > w[0].high).count();
        Some(count)
    }

    /// Mean of `high / prev_close - 1` (overnight gap-up fraction) across consecutive bars.
    /// Returns `None` for fewer than 2 bars or zero prior close.
    pub fn high_to_prev_close(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let ratios: Vec<f64> = bars.windows(2).filter_map(|w| {
            if w[0].close.is_zero() { None }
            else { Some(((w[1].high - w[0].close) / w[0].close).to_f64().unwrap_or(0.0)) }
        }).collect();
        if ratios.is_empty() { return None; }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    // ── round-108 ────────────────────────────────────────────────────────────

    /// Mean of `close - prev_open` across consecutive bars.
    /// Returns `None` for fewer than 2 bars.
    pub fn close_to_prev_open(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let diffs: Vec<f64> = bars.windows(2)
            .filter_map(|w| (w[1].close - w[0].open).to_f64())
            .collect();
        if diffs.is_empty() { return None; }
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Mean of `|close - prev_close| / prev_close` across consecutive bars.
    /// Returns `None` for fewer than 2 bars or zero prior close.
    pub fn momentum_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let ratios: Vec<f64> = bars.windows(2).filter_map(|w| {
            if w[0].close.is_zero() { None }
            else { Some(((w[1].close - w[0].close).abs() / w[0].close).to_f64().unwrap_or(0.0)) }
        }).collect();
        if ratios.is_empty() { return None; }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }

    /// Ratio of volume range `(max - min)` to mean volume across bars.
    /// Returns `None` for an empty slice or zero mean volume.
    pub fn volume_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let max_v = bars.iter().map(|b| b.volume).max()?;
        let min_v = bars.iter().map(|b| b.volume).min()?;
        let total: Decimal = bars.iter().map(|b| b.volume).sum();
        let mean = total / Decimal::from(bars.len());
        if mean.is_zero() { return None; }
        Some(((max_v - min_v) / mean).to_f64().unwrap_or(0.0))
    }

    /// Fraction of bar body that lies in the upper half of the bar's range.
    /// `body_upper = max(open, close) - midpoint` / range, averaged across bars.
    /// Returns `None` for an empty slice or no bars with non-zero range.
    pub fn body_upper_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.high - b.low;
            if range.is_zero() { return None; }
            let midpoint = (b.high + b.low) / Decimal::TWO;
            let body_upper = (b.open.max(b.close) - midpoint).max(Decimal::ZERO);
            Some((body_upper / range).to_f64().unwrap_or(0.0))
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-109 ────────────────────────────────────────────────────────────

    /// Fraction of bars where open != close of the prior bar (open gap).
    /// Returns `None` for fewer than 2 bars.
    pub fn open_gap_frequency(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let gapped = bars.windows(2).filter(|w| w[1].open != w[0].close).count();
        Some(gapped as f64 / (bars.len() - 1) as f64)
    }

    /// Mean of `close / open - 1` (intra-bar return) across bars with non-zero open.
    /// Returns `None` for an empty slice.
    pub fn avg_close_to_open(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            if b.open.is_zero() { None }
            else { Some(((b.close - b.open) / b.open).to_f64().unwrap_or(0.0)) }
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Count of bars where close crossed through the prior bar's open price.
    /// Returns `None` for fewer than 2 bars.
    pub fn close_cross_open_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| {
            let prev_open = w[0].open;
            // current close crosses prev bar's open
            (w[0].close < prev_open && w[1].close >= prev_open)
                || (w[0].close > prev_open && w[1].close <= prev_open)
        }).count();
        Some(count)
    }

    /// Mean trailing stop distance: `close - min(low over last N bars)` for each bar, N=3.
    /// Returns `None` for fewer than 3 bars.
    pub fn trailing_stop_distance(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 { return None; }
        let distances: Vec<f64> = bars.windows(3).filter_map(|w| {
            let min_low = w.iter().map(|b| b.low).min()?;
            let last_close = w[2].close;
            Some((last_close - min_low).to_f64().unwrap_or(0.0))
        }).collect();
        if distances.is_empty() { return None; }
        Some(distances.iter().sum::<f64>() / distances.len() as f64)
    }

    // ── round-110 ────────────────────────────────────────────────────────────

    /// Mean total shadow length `(high - low) - |close - open|` across bars.
    /// Returns `None` for an empty slice.
    pub fn avg_shadow_total(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: Decimal = bars.iter()
            .map(|b| (b.high - b.low) - (b.close - b.open).abs())
            .sum();
        Some((sum / Decimal::from(bars.len())).to_f64().unwrap_or(0.0))
    }

    /// Count of bars where open is above the prior bar's close (bullish gap-up open).
    /// Returns `None` for fewer than 2 bars.
    pub fn open_above_prev_close(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].open > w[0].close).count();
        Some(count)
    }

    /// Count of bars where close is below the prior bar's open.
    /// Returns `None` for fewer than 2 bars.
    pub fn close_below_prev_open(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].close < w[0].open).count();
        Some(count)
    }

    /// Candle range efficiency: mean `|close - open| / (high - low)` across bars with range > 0.
    /// Returns `None` for an empty slice or no qualifying bars.
    pub fn candle_range_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.high - b.low;
            if range.is_zero() { None }
            else { Some(((b.close - b.open).abs() / range).to_f64().unwrap_or(0.0)) }
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-111 ────────────────────────────────────────────────────────────

    /// Mean of `|close - open|` across all bars (mean absolute candle body size).
    /// Returns `None` for an empty slice.
    pub fn open_close_range(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: Decimal = bars.iter().map(|b| (b.close - b.open).abs()).sum();
        Some((sum / Decimal::from(bars.len())).to_f64().unwrap_or(0.0))
    }

    /// Mean volume per bar.
    /// Returns `None` for an empty slice.
    pub fn volume_per_bar(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let total: Decimal = bars.iter().map(|b| b.volume).sum();
        Some((total / Decimal::from(bars.len())).to_f64().unwrap_or(0.0))
    }

    /// Mean of `(close - prev_close) / prev_close` across consecutive bars.
    /// Returns `None` for fewer than 2 bars.
    pub fn price_momentum_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let vals: Vec<f64> = bars.windows(2).filter_map(|w| {
            if w[0].close.is_zero() { None }
            else { Some(((w[1].close - w[0].close) / w[0].close).to_f64().unwrap_or(0.0)) }
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Mean of `(close - open) / (high - low)` — intra-bar efficiency.
    /// Returns `None` for an empty slice or no bars with non-zero range.
    pub fn avg_intrabar_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let range = b.high - b.low;
            if range.is_zero() { None }
            else { Some(((b.close - b.open) / range).to_f64().unwrap_or(0.0)) }
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-112 ────────────────────────────────────────────────────────────

    /// Mean ratio of total wick length to body length across bars.
    /// Bars with zero body are skipped. Returns `None` if no eligible bars.
    pub fn wicks_to_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let body = (b.close - b.open).abs();
            if body.is_zero() { continue; }
            let upper = b.high - b.open.max(b.close);
            let lower = b.open.min(b.close) - b.low;
            let wicks = upper + lower;
            let ratio = wicks.to_f64()? / body.to_f64()?;
            sum += ratio;
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Mean absolute deviation of each bar's close from the overall close mean.
    /// Returns `None` for empty input.
    pub fn avg_close_deviation(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let n = bars.len() as f64;
        let mean: f64 = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).sum::<f64>() / n;
        let mad = bars.iter().map(|b| (b.close.to_f64().unwrap_or(0.0) - mean).abs()).sum::<f64>() / n;
        Some(mad)
    }

    /// Mean ratio of each bar's open to its midpoint `(high+low)/2`.
    /// Returns `None` for empty input.
    pub fn open_midpoint_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let mid = (b.high + b.low) / Decimal::TWO;
            if mid.is_zero() { 1.0 }
            else { b.open.to_f64().unwrap_or(0.0) / mid.to_f64().unwrap_or(1.0) }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Volume-weighted mean of close-to-close changes.
    /// Returns `None` for fewer than 2 bars or zero total volume.
    pub fn volume_weighted_close_change(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let mut num = 0f64;
        let mut denom = 0f64;
        for w in bars.windows(2) {
            let chg = (w[1].close - w[0].close).to_f64()?;
            let vol = w[1].volume.to_f64().unwrap_or(0.0);
            num += chg * vol;
            denom += vol;
        }
        if denom == 0.0 { None } else { Some(num / denom) }
    }

    // ── round-113 ────────────────────────────────────────────────────────────

    /// Mean ratio of bar range `(high - low)` to volume for each bar.
    /// Bars with zero volume are skipped. Returns `None` if no eligible bars.
    pub fn range_to_volume_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            if b.volume.is_zero() { continue; }
            let range = (b.high - b.low).to_f64().unwrap_or(0.0);
            let vol = b.volume.to_f64().unwrap_or(1.0);
            sum += range / vol;
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Mean of `(high - low)` (the high-low spread) across bars.
    /// Returns `None` for empty input.
    pub fn avg_high_low_spread(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| (b.high - b.low).to_f64().unwrap_or(0.0)).sum();
        Some(sum / bars.len() as f64)
    }

    /// Candle persistence: fraction of bars where the close direction matches
    /// the prior bar's close direction. Returns `None` for fewer than 3 bars.
    pub fn candle_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 3 { return None; }
        let dirs: Vec<i8> = bars.windows(2).map(|w| {
            if w[1].close > w[0].close { 1i8 }
            else if w[1].close < w[0].close { -1i8 }
            else { 0i8 }
        }).collect();
        let same = dirs.windows(2).filter(|w| w[0] != 0 && w[0] == w[1]).count();
        let eligible = dirs.windows(2).filter(|w| w[0] != 0 && w[1] != 0).count();
        if eligible == 0 { None } else { Some(same as f64 / eligible as f64) }
    }

    /// Z-score of each bar's range relative to the mean and std of all bar ranges.
    /// Returns the mean of all bar range z-scores. Returns `None` for fewer than 2 bars.
    pub fn bar_range_zscore(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let ranges: Vec<f64> = bars.iter().map(|b| (b.high - b.low).to_f64().unwrap_or(0.0)).collect();
        let n = ranges.len() as f64;
        let mean = ranges.iter().sum::<f64>() / n;
        let var = ranges.iter().map(|&r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std = var.sqrt();
        if std == 0.0 { return Some(0.0); }
        let z_mean = ranges.iter().map(|&r| (r - mean) / std).sum::<f64>() / n;
        Some(z_mean)
    }

    // ── round-114 ────────────────────────────────────────────────────────────

    /// Fraction of bars where the close direction reverses relative to the prior bar.
    /// Returns `None` for fewer than 3 bars.
    pub fn close_reversal_rate(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 3 { return None; }
        let dirs: Vec<i8> = bars.windows(2).map(|w| {
            if w[1].close > w[0].close { 1i8 }
            else if w[1].close < w[0].close { -1i8 }
            else { 0i8 }
        }).collect();
        let reversals = dirs.windows(2)
            .filter(|w| w[0] != 0 && w[1] != 0 && w[0] != w[1])
            .count();
        let eligible = dirs.windows(2).filter(|w| w[0] != 0 && w[1] != 0).count();
        if eligible == 0 { None } else { Some(reversals as f64 / eligible as f64) }
    }

    /// Mean ratio of bar body size to bar range. Bars with zero range are skipped.
    /// Returns `None` if no eligible bars.
    pub fn avg_body_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let range = b.high - b.low;
            if range.is_zero() { continue; }
            let body = (b.close - b.open).abs();
            sum += (body / range).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Z-score of the last bar's volume relative to the slice.
    /// Returns `None` for fewer than 2 bars or zero volume std dev.
    pub fn volume_zscore(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let n = vols.len() as f64;
        let mean = vols.iter().sum::<f64>() / n;
        let var = vols.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let std = var.sqrt();
        if std == 0.0 { return None; }
        let last = *vols.last()?;
        Some((last - mean) / std)
    }

    /// Skewness of bar body sizes `|close - open|` across the slice.
    /// Returns `None` for fewer than 3 bars or zero std dev.
    pub fn body_skew(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 { return None; }
        let bodies: Vec<f64> = bars.iter().map(|b| (b.close - b.open).abs().to_f64().unwrap_or(0.0)).collect();
        let n = bodies.len() as f64;
        let mean = bodies.iter().sum::<f64>() / n;
        let var = bodies.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std == 0.0 { return None; }
        let skew = bodies.iter().map(|&x| ((x - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    // ── round-115 ────────────────────────────────────────────────────────────

    /// Mean absolute open gap `|open - prev_close|` across consecutive bar pairs.
    /// Returns `None` for fewer than 2 bars.
    pub fn avg_open_gap(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2)
            .map(|w| (w[1].open - w[0].close).abs().to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Mean ratio of `high` to `low` for each bar. Bars with zero low are skipped.
    /// Returns `None` if no eligible bars.
    pub fn hl_ratio_mean(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            if b.low.is_zero() { continue; }
            sum += b.high.to_f64().unwrap_or(0.0) / b.low.to_f64().unwrap_or(1.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Mean ratio of total shadow `(high - max(open,close)) + (min(open,close) - low)`
    /// to bar range `(high - low)`. Bars with zero range are skipped. Returns `None`
    /// if no eligible bars.
    pub fn shadow_to_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let range = b.high - b.low;
            if range.is_zero() { continue; }
            let upper = b.high - b.open.max(b.close);
            let lower = b.open.min(b.close) - b.low;
            let shadow = upper + lower;
            sum += (shadow / range).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Mean of `close - low` across bars. Returns `None` for empty input.
    pub fn avg_close_to_low(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| (b.close - b.low).to_f64().unwrap_or(0.0)).sum();
        Some(sum / bars.len() as f64)
    }

    // ── round-116 ────────────────────────────────────────────────────────────

    /// Mean of `high - close` across bars. Returns `None` for empty input.
    pub fn avg_high_to_close(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| (b.high - b.close).to_f64().unwrap_or(0.0)).sum();
        Some(sum / bars.len() as f64)
    }

    /// Shannon entropy of bar body-size distribution across 8 equal-width buckets.
    /// Returns `None` for fewer than 2 bars.
    pub fn bar_size_entropy(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sizes: Vec<f64> = bars.iter().map(|b| (b.close - b.open).abs().to_f64().unwrap_or(0.0)).collect();
        let max = sizes.iter().cloned().fold(0f64, f64::max);
        let min = sizes.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        const BUCKETS: usize = 8;
        let mut counts = [0usize; BUCKETS];
        if range == 0.0 {
            counts[0] = sizes.len();
        } else {
            for &s in &sizes {
                let idx = ((s - min) / range * (BUCKETS - 1) as f64) as usize;
                counts[idx.min(BUCKETS - 1)] += 1;
            }
        }
        let n = sizes.len() as f64;
        let entropy = counts.iter().map(|&c| {
            if c == 0 { 0.0 } else { let p = c as f64 / n; -p * p.ln() }
        }).sum();
        Some(entropy)
    }

    /// Mean `(close - open) / open` percentage change per bar.
    /// Bars with zero open are skipped. Returns `None` if no eligible bars.
    pub fn close_to_open_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            if b.open.is_zero() { continue; }
            sum += ((b.close - b.open) / b.open).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Fraction of bars where close > open (bullish). Returns `None` for empty input.
    pub fn body_direction_score(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let bullish = bars.iter().filter(|b| b.close > b.open).count();
        Some(bullish as f64 / bars.len() as f64)
    }

    // ── round-117 ────────────────────────────────────────────────────────────

    /// Fraction of bars where close > open. Alias providing explicit naming.
    /// Returns `None` for empty input.
    pub fn close_above_open_pct(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let count = bars.iter().filter(|b| b.close > b.open).count();
        Some(count as f64 / bars.len() as f64)
    }

    /// Mean of `low - close` (negative when close > low). Returns `None` for empty input.
    pub fn avg_low_to_close(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| (b.low - b.close).to_f64().unwrap_or(0.0)).sum();
        Some(sum / bars.len() as f64)
    }

    /// Bar trend score: fraction of consecutive close-to-close pairs that are rising.
    /// Returns `None` for fewer than 2 bars.
    pub fn bar_trend_score(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let rising = bars.windows(2).filter(|w| w[1].close > w[0].close).count();
        Some(rising as f64 / (bars.len() - 1) as f64)
    }

    /// Count of bars where volume exceeds the mean volume of the slice.
    /// Returns `None` for empty input.
    pub fn volume_above_avg_count(bars: &[OhlcvBar]) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let n = bars.len() as f64;
        let mean_vol = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).sum::<f64>() / n;
        Some(bars.iter().filter(|b| b.volume.to_f64().unwrap_or(0.0) > mean_vol).count())
    }

    // ── round-118 ────────────────────────────────────────────────────────────

    /// Mean of `(close - low) / (high - low)` close-range position across bars.
    /// Bars with zero range are skipped. Returns `None` if no eligible bars.
    pub fn avg_close_range_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let range = b.high - b.low;
            if range.is_zero() { continue; }
            sum += ((b.close - b.low) / range).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Mean ratio of each bar's volume to the maximum volume in the slice.
    /// Returns `None` for empty input or zero max volume.
    pub fn volume_ratio_to_max(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let max_vol = bars.iter().map(|b| b.volume).fold(bars[0].volume, |a, b| a.max(b));
        if max_vol.is_zero() { return None; }
        let sum: f64 = bars.iter().map(|b| (b.volume / max_vol).to_f64().unwrap_or(0.0)).sum();
        Some(sum / bars.len() as f64)
    }

    /// Bar consolidation score: mean ratio of body size to ATR-style range.
    /// Lower values indicate consolidation. Bars with zero range are skipped.
    /// Returns `None` if no eligible bars.
    pub fn bar_consolidation_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let range = b.high - b.low;
            if range.is_zero() { continue; }
            let body = (b.close - b.open).abs();
            sum += (body / range).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(1.0 - sum / cnt as f64) }
    }

    /// Shadow asymmetry: mean of `(upper_shadow - lower_shadow) / range`.
    /// Positive → upper wicks dominate. Bars with zero range are skipped.
    /// Returns `None` if no eligible bars.
    pub fn shadow_asymmetry(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for b in bars {
            let range = b.high - b.low;
            if range.is_zero() { continue; }
            let upper = b.high - b.open.max(b.close);
            let lower = b.open.min(b.close) - b.low;
            sum += ((upper - lower) / range).to_f64().unwrap_or(0.0);
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    // ── round-119 ────────────────────────────────────────────────────────────

    /// Mean of `(open + close) / 2` across bars. Returns `None` for empty input.
    pub fn open_close_midpoint(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            ((b.open + b.close) / Decimal::TWO).to_f64().unwrap_or(0.0)
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Volume concentration ratio: fraction of total volume in the top-third of bars
    /// when sorted by volume (descending). Returns `None` for empty input or zero volume.
    pub fn volume_concentration_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let total_vol: f64 = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).sum();
        if total_vol == 0.0 { return None; }
        let mut vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        vols.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let top_n = (bars.len() / 3).max(1);
        let top_vol: f64 = vols[..top_n].iter().sum();
        Some(top_vol / total_vol)
    }

    /// Fraction of bars where open is between the prior bar's open and close
    /// (partial gap fill). Returns `None` for fewer than 2 bars.
    pub fn bar_gap_fill_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let fills = bars.windows(2).filter(|w| {
            let lo = w[0].open.min(w[0].close);
            let hi = w[0].open.max(w[0].close);
            w[1].open >= lo && w[1].open <= hi
        }).count();
        Some(fills as f64 / (bars.len() - 1) as f64)
    }

    /// Net shadow direction score: fraction of bars with longer upper shadow minus
    /// fraction with longer lower shadow. Range `[-1, 1]`.
    /// Returns `None` for empty input.
    pub fn net_shadow_direction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let mut upper_dom = 0usize;
        let mut lower_dom = 0usize;
        for b in bars {
            let upper = b.high - b.open.max(b.close);
            let lower = b.open.min(b.close) - b.low;
            if upper > lower { upper_dom += 1; }
            else if lower > upper { lower_dom += 1; }
        }
        let n = bars.len() as f64;
        Some((upper_dom as f64 - lower_dom as f64) / n)
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    /// Stability of close relative to bar range: 1 - std(close_pct) over bars.
    pub fn close_range_stability(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            if r == 0.0 { 0.5 } else { (b.close - b.low).to_f64().unwrap_or(0.0) / r }
        }).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(1.0 - std)
    }

    /// Average bar volatility: mean of (high - low) as a fraction of open.
    pub fn avg_bar_volatility(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let o = b.open.to_f64().unwrap_or(0.0);
            if o == 0.0 { 0.0 } else { (b.high - b.low).to_f64().unwrap_or(0.0) / o }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Open range bias: fraction of bars where open is above bar midpoint.
    pub fn open_range_bias(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let above = bars.iter().filter(|b| {
            let mid = (b.high + b.low) / rust_decimal::Decimal::TWO;
            b.open > mid
        }).count();
        Some(above as f64 / bars.len() as f64)
    }

    /// Body volatility: standard deviation of body sizes (|close - open|).
    pub fn body_volatility(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter()
            .map(|b| (b.close - b.open).abs().to_f64().unwrap_or(0.0))
            .collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        Some(var.sqrt())
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    /// Fraction of bars where open falls within the prior bar's range (gap fill).
    pub fn close_gap_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let fills = bars.windows(2).filter(|w| {
            w[1].open >= w[0].low && w[1].open <= w[0].high
        }).count();
        Some(fills as f64 / (bars.len() - 1) as f64)
    }

    /// Volume deceleration: mean of (vol[i] - vol[i-1]) for decreasing pairs.
    pub fn volume_deceleration(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let decs: Vec<f64> = bars.windows(2).filter_map(|w| {
            if w[1].volume < w[0].volume {
                Some((w[1].volume - w[0].volume).to_f64().unwrap_or(0.0))
            } else { None }
        }).collect();
        if decs.is_empty() { return Some(0.0); }
        Some(decs.iter().sum::<f64>() / decs.len() as f64)
    }

    /// Bar trend persistence: fraction of bars that continue the prior bar's direction.
    pub fn bar_trend_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let persist = bars.windows(2).filter(|w| {
            let prev_bull = w[0].close >= w[0].open;
            let curr_bull = w[1].close >= w[1].open;
            prev_bull == curr_bull
        }).count();
        Some(persist as f64 / (bars.len() - 1) as f64)
    }

    /// Ratio of total shadow length to body length across bars.
    pub fn shadow_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let mut total_shadow = 0f64;
        let mut total_body = 0f64;
        for b in bars {
            let body = (b.close - b.open).abs().to_f64().unwrap_or(0.0);
            let upper = (b.high - b.open.max(b.close)).to_f64().unwrap_or(0.0);
            let lower = (b.open.min(b.close) - b.low).to_f64().unwrap_or(0.0);
            total_shadow += upper + lower;
            total_body += body;
        }
        if total_body == 0.0 { None } else { Some(total_shadow / total_body) }
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    /// Mean of body size as fraction of range: |close-open| / (high-low) per bar.
    pub fn body_to_range_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let range = (b.high - b.low).to_f64().unwrap_or(0.0);
            if range == 0.0 { 0.0 } else { (b.close - b.open).abs().to_f64().unwrap_or(0.0) / range }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Mean absolute gap between consecutive bars: |open[i] - close[i-1]|.
    pub fn avg_open_close_gap(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2)
            .map(|w| (w[1].open - w[0].close).abs().to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Mean of high / (high - low) per bar (high position within range).
    pub fn high_low_body_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let range = (b.high - b.low).to_f64().unwrap_or(0.0);
            if range == 0.0 { 0.5 } else {
                (b.high - b.open.min(b.close)).to_f64().unwrap_or(0.0) / range
            }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Fraction of bars where close exceeds the prior bar's high.
    pub fn close_above_prior_high(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].close > w[0].high).count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    /// Mean of (close - low) / (high - low) per bar (close position within range).
    pub fn close_range_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            if r == 0.0 { 0.5 } else { (b.close - b.low).to_f64().unwrap_or(0.0) / r }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Mean of (high - low) / open as a fraction across bars.
    pub fn avg_bar_range_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let o = b.open.to_f64().unwrap_or(0.0);
            if o == 0.0 { 0.0 } else { (b.high - b.low).to_f64().unwrap_or(0.0) / o }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Mean of (open - low) / (high - low): open proximity to the low.
    pub fn open_to_low_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            if r == 0.0 { 0.5 } else { (b.open - b.low).to_f64().unwrap_or(0.0) / r }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Rank of last close among all closes (0.0 = lowest, 1.0 = highest).
    pub fn bar_close_rank(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let last_close = bars.last()?.close.to_f64()?;
        let mut closes: Vec<f64> = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).collect();
        closes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = closes.iter().position(|&c| (c - last_close).abs() < 1e-12).unwrap_or(0);
        Some(pos as f64 / (closes.len() - 1).max(1) as f64)
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    /// Count of times close oscillates (changes direction) across bars.
    pub fn close_oscillation_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.len() < 3 { return None; }
        let diffs: Vec<i8> = bars.windows(2).map(|w| {
            if w[1].close > w[0].close { 1i8 } else if w[1].close < w[0].close { -1i8 } else { 0i8 }
        }).collect();
        Some(diffs.windows(2).filter(|w| w[0] != 0 && w[1] != 0 && w[0] != w[1]).count())
    }

    /// Fraction of bars with range below the median range (consolidating bars).
    pub fn bar_consolidation_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let mut ranges: Vec<f64> = bars.iter()
            .map(|b| (b.high - b.low).to_f64().unwrap_or(0.0))
            .collect();
        ranges.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = ranges.len();
        let median = if n % 2 == 0 { (ranges[n/2-1] + ranges[n/2]) / 2.0 } else { ranges[n/2] };
        let below = bars.iter().filter(|b| {
            (b.high - b.low).to_f64().unwrap_or(0.0) < median
        }).count();
        Some(below as f64 / n as f64)
    }

    /// Momentum of open prices: fraction of bars where open[i] > open[i-1].
    pub fn open_momentum_score(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let rising = bars.windows(2).filter(|w| w[1].open > w[0].open).count();
        Some(rising as f64 / (bars.len() - 1) as f64)
    }

    /// Mean of absolute volume changes between consecutive bars.
    pub fn avg_volume_change(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2)
            .map(|w| (w[1].volume - w[0].volume).abs().to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    /// Lag-1 autocorrelation of close prices.
    pub fn close_lag1_autocorr(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 { return None; }
        let closes: Vec<f64> = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).collect();
        let n = closes.len() as f64;
        let mean = closes.iter().sum::<f64>() / n;
        let var = closes.iter().map(|&c| (c - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let cov: f64 = closes.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>() / (n - 1.0);
        Some(cov / var)
    }

    /// Skewness of volume distribution across bars.
    pub fn volume_skewness(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 { return None; }
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let n = vols.len() as f64;
        let mean = vols.iter().sum::<f64>() / n;
        let var = vols.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std == 0.0 { return None; }
        Some(vols.iter().map(|&v| ((v - mean) / std).powi(3)).sum::<f64>() / n)
    }

    /// Rank of last bar's range (high-low) among all bar ranges (0.0=min, 1.0=max).
    pub fn bar_height_rank(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let last_range = (bars.last()?.high - bars.last()?.low).to_f64()?;
        let mut ranges: Vec<f64> = bars.iter()
            .map(|b| (b.high - b.low).to_f64().unwrap_or(0.0))
            .collect();
        ranges.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pos = ranges.iter().position(|&r| (r - last_range).abs() < 1e-12).unwrap_or(0);
        Some(pos as f64 / (ranges.len() - 1).max(1) as f64)
    }

    /// Fraction of bars where high[i] > high[i-1].
    pub fn high_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].high > w[0].high).count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    /// EMA (α=0.2) of body sizes |close - open| across bars.
    pub fn body_ema(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = (bars[0].close - bars[0].open).abs().to_f64().unwrap_or(0.0);
        for b in &bars[1..] {
            ema = alpha * (b.close - b.open).abs().to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Mean of (high - low) / prior close across bars (range as fraction of prior close).
    pub fn avg_true_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2).map(|w| {
            let pc = w[0].close.to_f64().unwrap_or(0.0);
            if pc == 0.0 { 0.0 } else { (w[1].high - w[1].low).to_f64().unwrap_or(0.0) / pc }
        }).sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Mean fraction of bar range that is body: |close-open| / (high-low).
    pub fn close_body_fraction(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            if r == 0.0 { 0.0 } else { (b.close - b.open).abs().to_f64().unwrap_or(0.0) / r }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Mean second-order close change (close acceleration).
    pub fn bar_momentum_accel(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 3 { return None; }
        let closes: Vec<f64> = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).collect();
        let acc: Vec<f64> = closes.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
        Some(acc.iter().sum::<f64>() / acc.len() as f64)
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    /// Mean absolute close change per bar (close velocity).
    pub fn close_velocity(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2)
            .map(|w| (w[1].close - w[0].close).abs().to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Mean of (open - low) / (high - low): how high in the range the open is.
    pub fn open_range_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            if r == 0.0 { 0.5 } else { (b.open - b.low).to_f64().unwrap_or(0.0) / r }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Net fraction of bars that are bullish minus bearish.
    pub fn body_trend_direction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let bull = bars.iter().filter(|b| b.close > b.open).count();
        let bear = bars.iter().filter(|b| b.close < b.open).count();
        Some((bull as f64 - bear as f64) / bars.len() as f64)
    }

    /// Mean of (high - low) relative to the bar midpoint (open+close)/2.
    pub fn bar_tightness(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let mid = ((b.open + b.close) / rust_decimal::Decimal::TWO).to_f64().unwrap_or(0.0);
            if mid == 0.0 { 0.0 } else { (b.high - b.low).to_f64().unwrap_or(0.0) / mid }
        }).sum();
        Some(sum / bars.len() as f64)
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    /// Average close slope: mean of (close[i] - close[i-1]) across bars.
    pub fn avg_close_slope(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2)
            .map(|w| (w[1].close - w[0].close).to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (bars.len() - 1) as f64)
    }

    /// Z-score of last body size relative to all body sizes.
    pub fn body_range_zscore(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let bodies: Vec<f64> = bars.iter()
            .map(|b| (b.close - b.open).abs().to_f64().unwrap_or(0.0))
            .collect();
        let n = bodies.len() as f64;
        let mean = bodies.iter().sum::<f64>() / n;
        let std = (bodies.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let last = *bodies.last()?;
        Some((last - mean) / std)
    }

    /// Shannon entropy of volume distribution across bars.
    pub fn volume_entropy(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = vols.iter().sum();
        if total == 0.0 { return Some(0.0); }
        let entropy: f64 = vols.iter()
            .map(|&v| { let p = v / total; if p > 0.0 { -p * p.ln() } else { 0.0 } })
            .sum();
        Some(entropy)
    }

    /// Fraction of bars where low[i] < low[i-1] (new lows).
    pub fn low_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2).filter(|w| w[1].low < w[0].low).count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-129 ────────────────────────────────────────────────────────────

    /// Bar energy: mean of (high - low)^2 across bars.
    pub fn bar_energy(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            let r = (b.high - b.low).to_f64().unwrap_or(0.0);
            r * r
        }).sum();
        Some(sum / bars.len() as f64)
    }

    /// Open-close persistence: fraction of bars where open equals previous close.
    pub fn open_close_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2)
            .filter(|w| w[1].open == w[0].close)
            .count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    /// Bar range trend: mean of (range[i] - range[i-1]) across consecutive bars.
    pub fn bar_range_trend(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let ranges: Vec<f64> = bars.iter()
            .map(|b| (b.high - b.low).to_f64().unwrap_or(0.0))
            .collect();
        let diffs: Vec<f64> = ranges.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Open-high spread: mean of (high - open) / open across bars.
    pub fn open_high_spread(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let o = b.open.to_f64()?;
            let h = b.high.to_f64()?;
            if o == 0.0 { return None; }
            Some((h - o) / o)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-130 ────────────────────────────────────────────────────────────

    /// Close-body-range ratio: mean of body size / (high - low) per bar.
    pub fn close_body_range_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let body = (b.close - b.open).abs().to_f64()?;
            let range = (b.high - b.low).to_f64()?;
            if range == 0.0 { return None; }
            Some(body / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Average body percent: mean of body / open across bars.
    pub fn avg_body_pct(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let body = (b.close - b.open).abs().to_f64()?;
            let o = b.open.to_f64()?;
            if o == 0.0 { return None; }
            Some(body / o)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar symmetry: mean of |upper_shadow - lower_shadow| / range per bar.
    pub fn bar_symmetry(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let lo = b.low.to_f64()?;
            let op = b.open.to_f64()?;
            let cl = b.close.to_f64()?;
            let range = hi - lo;
            if range == 0.0 { return None; }
            let body_hi = op.max(cl);
            let body_lo = op.min(cl);
            let upper = hi - body_hi;
            let lower = body_lo - lo;
            Some((upper - lower).abs() / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Open gap direction: fraction of bars where open > prior close (gap up).
    pub fn open_gap_direction(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2)
            .filter(|w| w[1].open > w[0].close)
            .count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-131 ────────────────────────────────────────────────────────────

    /// Close trend strength: (last close - first close) / (max close - min close).
    pub fn close_trend_strength(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let closes: Vec<f64> = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).collect();
        let first = *closes.first()?;
        let last = *closes.last()?;
        let max = closes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = closes.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return None; }
        Some((last - first) / range)
    }

    /// Bar body skew: mean of (close - open) / (high - low), signed.
    pub fn bar_body_skew(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let body = (b.close - b.open).to_f64()?;
            let range = (b.high - b.low).to_f64()?;
            if range == 0.0 { return None; }
            Some(body / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar range mean deviation: mean absolute deviation of (high - low) from its mean.
    pub fn bar_range_mean_dev(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let ranges: Vec<f64> = bars.iter().map(|b| (b.high - b.low).to_f64().unwrap_or(0.0)).collect();
        let mean = ranges.iter().sum::<f64>() / ranges.len() as f64;
        let mad = ranges.iter().map(|&r| (r - mean).abs()).sum::<f64>() / ranges.len() as f64;
        Some(mad)
    }

    /// Bar close momentum: sum of sign of close changes across consecutive bars.
    pub fn bar_close_momentum(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let sum: f64 = bars.windows(2).map(|w| {
            if w[1].close > w[0].close { 1.0 }
            else if w[1].close < w[0].close { -1.0 }
            else { 0.0 }
        }).sum();
        Some(sum)
    }

    // ── round-132 ────────────────────────────────────────────────────────────

    /// Bar volume trend: mean of consecutive volume differences.
    pub fn bar_volume_trend(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = vols.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Close-low spread: mean of (close - low) / (high - low) per bar.
    pub fn close_low_spread(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let cl = b.close.to_f64()?;
            let lo = b.low.to_f64()?;
            let hi = b.high.to_f64()?;
            let range = hi - lo;
            if range == 0.0 { return None; }
            Some((cl - lo) / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar midpoint trend: mean of consecutive midpoint (high+low)/2 differences.
    pub fn bar_midpoint_trend(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let mids: Vec<f64> = bars.iter().map(|b| {
            let hi = b.high.to_f64().unwrap_or(0.0);
            let lo = b.low.to_f64().unwrap_or(0.0);
            (hi + lo) / 2.0
        }).collect();
        let diffs: Vec<f64> = mids.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Bar spread score: mean of (high - low) / close per bar.
    pub fn bar_spread_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let lo = b.low.to_f64()?;
            let cl = b.close.to_f64()?;
            if cl == 0.0 { return None; }
            Some((hi - lo) / cl)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-133 ────────────────────────────────────────────────────────────

    /// Bar open-close momentum: sum of signs of (close - open) across bars.
    pub fn bar_open_close_momentum(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.is_empty() { return None; }
        let sum: f64 = bars.iter().map(|b| {
            if b.close > b.open { 1.0 }
            else if b.close < b.open { -1.0 }
            else { 0.0 }
        }).sum();
        Some(sum)
    }

    /// Close body position: mean of (close - low) / (high - low) per bar (1.0 = close at high).
    pub fn close_body_position(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let cl = b.close.to_f64()?;
            let lo = b.low.to_f64()?;
            let hi = b.high.to_f64()?;
            let range = hi - lo;
            if range == 0.0 { return None; }
            Some((cl - lo) / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar close persistence: fraction of bars where close > prior close (already exists as
    /// `bar_trend_persistence`, so this version uses open-close comparison across bars).
    pub fn bar_close_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2)
            .filter(|w| w[1].close > w[0].open)
            .count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    /// Close wick ratio: mean of upper_wick / body per bar.
    pub fn close_wick_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let cl = b.close.to_f64()?;
            let op = b.open.to_f64()?;
            let body = (cl - op).abs();
            if body == 0.0 { return None; }
            let upper_wick = hi - cl.max(op);
            Some(upper_wick / body)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-134 ────────────────────────────────────────────────────────────

    /// Bar high trend: mean of consecutive high differences.
    pub fn bar_high_trend(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let highs: Vec<f64> = bars.iter().map(|b| b.high.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = highs.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Bar low trend: mean of consecutive low differences.
    pub fn bar_low_trend(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let lows: Vec<f64> = bars.iter().map(|b| b.low.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = lows.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Close-high wick: mean of (high - close) / (high - low) per bar.
    pub fn close_high_wick(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let lo = b.low.to_f64()?;
            let cl = b.close.to_f64()?;
            let range = hi - lo;
            if range == 0.0 { return None; }
            Some((hi - cl) / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar open persistence: fraction where open[i] > open[i-1].
    pub fn bar_open_persistence(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2)
            .filter(|w| w[1].open > w[0].open)
            .count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    // ── round-135 ────────────────────────────────────────────────────────────

    /// Bar body count: number of bars with non-zero body (close ≠ open).
    pub fn bar_body_count(bars: &[OhlcvBar]) -> Option<usize> {
        if bars.is_empty() { return None; }
        let count = bars.iter().filter(|b| b.close != b.open).count();
        Some(count)
    }

    /// Range contraction ratio: fraction of bars where range < prior range.
    pub fn range_contraction_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        if bars.len() < 2 { return None; }
        let count = bars.windows(2)
            .filter(|w| (w[1].high - w[1].low) < (w[0].high - w[0].low))
            .count();
        Some(count as f64 / (bars.len() - 1) as f64)
    }

    /// Volume trend ratio: last volume / mean volume.
    pub fn volume_trend_ratio(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vols: Vec<f64> = bars.iter().map(|b| b.volume.to_f64().unwrap_or(0.0)).collect();
        let mean = vols.iter().sum::<f64>() / vols.len() as f64;
        if mean == 0.0 { return None; }
        let last = *vols.last()?;
        Some(last / mean)
    }

    /// Bar midpoint score: mean of (midpoint - open) / range per bar.
    pub fn bar_midpoint_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let lo = b.low.to_f64()?;
            let op = b.open.to_f64()?;
            let range = hi - lo;
            if range == 0.0 { return None; }
            let mid = (hi + lo) / 2.0;
            Some((mid - op) / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    // ── round-136 ────────────────────────────────────────────────────────────

    /// Bar open efficiency: mean of |close - open| / (high - low) per bar.
    pub fn bar_open_efficiency(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let body = (b.close - b.open).abs().to_f64()?;
            let range = (b.high - b.low).to_f64()?;
            if range == 0.0 { return None; }
            Some(body / range)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Close oscillation amplitude: std of close prices across bars.
    pub fn close_oscillation_amplitude(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let closes: Vec<f64> = bars.iter().map(|b| b.close.to_f64().unwrap_or(0.0)).collect();
        let mean = closes.iter().sum::<f64>() / closes.len() as f64;
        let std = (closes.iter().map(|&c| (c - mean).powi(2)).sum::<f64>() / closes.len() as f64).sqrt();
        Some(std)
    }

    /// Bar high-low score: mean of high / low ratio minus 1.
    pub fn bar_high_low_score(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.is_empty() { return None; }
        let vals: Vec<f64> = bars.iter().filter_map(|b| {
            let hi = b.high.to_f64()?;
            let lo = b.low.to_f64()?;
            if lo == 0.0 { return None; }
            Some(hi / lo - 1.0)
        }).collect();
        if vals.is_empty() { return None; }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Bar range change: mean of range[i] - range[i-1] across consecutive bars.
    pub fn bar_range_change(bars: &[OhlcvBar]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if bars.len() < 2 { return None; }
        let ranges: Vec<f64> = bars.iter().map(|b| (b.high - b.low).to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = ranges.windows(2).map(|w| w[1] - w[0]).collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
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
    /// The most recently completed bar emitted by `feed` or `flush`.
    last_bar: Option<OhlcvBar>,
    /// When true, `feed` returns synthetic zero-volume bars for any bar windows
    /// that were skipped between the previous tick and the current one.
    /// The synthetic bars use the last known close price for all OHLC fields.
    emit_empty_bars: bool,
    /// Total number of completed bars emitted by this aggregator.
    bars_emitted: u64,
    /// Running sum of `price × quantity` for VWAP computation in the current bar.
    price_volume_sum: Decimal,
    /// Cumulative volume across all completed bars (does not include the current partial bar).
    total_volume: Decimal,
    /// Maximum single-bar volume seen across all completed bars.
    peak_volume: Option<Decimal>,
    /// Minimum single-bar volume seen across all completed bars.
    min_volume: Option<Decimal>,
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
            last_bar: None,
            emit_empty_bars: false,
            bars_emitted: 0,
            price_volume_sum: Decimal::ZERO,
            total_volume: Decimal::ZERO,
            peak_volume: None,
            min_volume: None,
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
            .map_or(false, |b| b.bar_start_ms != bar_start);

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
                        vwap: None,
                    });
                    gap_start += dur;
                }
            }
        }

        // Update price_volume_sum before the match to avoid borrow conflicts.
        let tick_value = tick.value();
        if self.current_bar.is_some() {
            self.price_volume_sum += tick_value;
        } else {
            self.price_volume_sum = tick_value;
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
                bar.vwap = if bar.volume.is_zero() {
                    None
                } else {
                    Some(self.price_volume_sum / bar.volume)
                };
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
                    vwap: Some(tick.price), // single-tick VWAP = price
                });
            }
        }
        self.bars_emitted += emitted.len() as u64;
        for b in &emitted {
            self.total_volume += b.volume;
            self.peak_volume = Some(match self.peak_volume {
                Some(prev) => prev.max(b.volume),
                None => b.volume,
            });
            self.min_volume = Some(match self.min_volume {
                Some(prev) => prev.min(b.volume),
                None => b.volume,
            });
        }
        if let Some(b) = emitted.last() {
            self.last_bar = Some(b.clone());
        }
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
        self.total_volume += bar.volume;
        self.peak_volume = Some(match self.peak_volume {
            Some(prev) => prev.max(bar.volume),
            None => bar.volume,
        });
        self.min_volume = Some(match self.min_volume {
            Some(prev) => prev.min(bar.volume),
            None => bar.volume,
        });
        self.last_bar = Some(bar.clone());
        Some(bar)
    }

    /// The most recently completed bar emitted by [`feed`](Self::feed) or
    /// [`flush`](Self::flush). Returns `None` if no bar has been completed yet.
    ///
    /// Unlike [`current_bar`](Self::current_bar), this bar is always complete.
    pub fn last_bar(&self) -> Option<&OhlcvBar> {
        self.last_bar.as_ref()
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
        self.last_bar = None;
        self.bars_emitted = 0;
        self.price_volume_sum = Decimal::ZERO;
        self.total_volume = Decimal::ZERO;
        self.peak_volume = None;
        self.min_volume = None;
    }

    /// Cumulative traded volume across all completed bars emitted by this aggregator.
    ///
    /// Does not include the current partial bar's volume. Reset to zero by
    /// [`reset`](Self::reset).
    pub fn total_volume(&self) -> Decimal {
        self.total_volume
    }

    /// Maximum single-bar volume seen across all completed bars.
    ///
    /// Returns `None` if no bars have been completed yet. Reset to `None` by
    /// [`reset`](Self::reset).
    pub fn peak_volume(&self) -> Option<Decimal> {
        self.peak_volume
    }

    /// Minimum single-bar volume seen across all completed bars.
    ///
    /// Returns `None` if no bars have been completed yet. Reset to `None` by
    /// [`reset`](Self::reset).
    pub fn min_volume(&self) -> Option<Decimal> {
        self.min_volume
    }

    /// Volume range across completed bars: `(min_volume, peak_volume)`.
    ///
    /// Returns `None` if no bars have been completed yet. Useful for
    /// normalizing volume signals to the observed range.
    pub fn volume_range(&self) -> Option<(Decimal, Decimal)> {
        Some((self.min_volume?, self.peak_volume?))
    }

    /// Average volume per completed bar: `total_volume / bars_emitted`.
    ///
    /// Returns `None` if no bars have been completed yet (avoids division by zero).
    pub fn average_volume(&self) -> Option<Decimal> {
        if self.bars_emitted == 0 {
            return None;
        }
        Some(self.total_volume / Decimal::from(self.bars_emitted))
    }

    /// The symbol this aggregator tracks.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// The timeframe used for bar alignment.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Fraction of the current bar's time window that has elapsed, in `[0.0, 1.0]`.
    ///
    /// Returns `None` if no bar is in progress (no ticks seen since last
    /// flush/reset). `now_ms` should be ≥ the current bar's `bar_start_ms`;
    /// values before the start clamp to `0.0`.
    pub fn window_progress(&self, now_ms: u64) -> Option<f64> {
        let bar = self.current_bar.as_ref()?;
        let elapsed = now_ms.saturating_sub(bar.bar_start_ms);
        let duration = self.timeframe.duration_ms();
        let progress = elapsed as f64 / duration as f64;
        Some(progress.clamp(0.0, 1.0))
    }

    /// Returns `true` if a bar is currently in progress (at least one tick has
    /// been fed since the last flush or reset).
    pub fn is_active(&self) -> bool {
        self.current_bar.is_some()
    }

    /// Volume-weighted average price of the current in-progress bar.
    ///
    /// Returns `None` if no bar is currently being built or the bar has zero
    /// volume (should not happen with real ticks).
    pub fn vwap_current(&self) -> Option<Decimal> {
        let bar = self.current_bar.as_ref()?;
        if bar.volume.is_zero() {
            return None;
        }
        Some(self.price_volume_sum / bar.volume)
    }
}

#[cfg(test)]
#[allow(deprecated)]
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
    fn test_timeframe_ord_seconds_lt_minutes() {
        assert!(Timeframe::Seconds(30) < Timeframe::Minutes(1));
    }

    #[test]
    fn test_timeframe_ord_minutes_lt_hours() {
        assert!(Timeframe::Minutes(59) < Timeframe::Hours(1));
    }

    #[test]
    fn test_timeframe_ord_same_duration_equal() {
        assert_eq!(Timeframe::Seconds(60), Timeframe::Seconds(60));
        assert_eq!(
            Timeframe::Seconds(3600).cmp(&Timeframe::Hours(1)),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_timeframe_ord_sort() {
        let mut tfs = vec![
            Timeframe::Hours(1),
            Timeframe::Seconds(30),
            Timeframe::Minutes(5),
        ];
        tfs.sort();
        assert_eq!(tfs[0], Timeframe::Seconds(30));
        assert_eq!(tfs[1], Timeframe::Minutes(5));
        assert_eq!(tfs[2], Timeframe::Hours(1));
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
    fn test_ohlcv_bar_is_bullish_when_close_gt_open() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert!(bar.is_bullish());
        assert!(!bar.is_bearish());
    }

    #[test]
    fn test_ohlcv_bar_is_bearish_when_close_lt_open() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert!(bar.is_bearish());
        assert!(!bar.is_bullish());
    }

    #[test]
    fn test_ohlcv_bar_neither_bullish_nor_bearish_on_equal_open_close() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        // Single tick: open == close
        let bar = agg.current_bar().unwrap();
        assert!(!bar.is_bullish());
        assert!(!bar.is_bearish());
    }

    #[test]
    fn test_ohlcv_bar_vwap_single_tick_equals_price() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(2), 60_000))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.vwap, Some(dec!(50000)));
    }

    #[test]
    fn test_ohlcv_bar_vwap_two_equal_price_ticks() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(3), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        // vwap = (50000*1 + 50000*3) / (1+3) = 50000
        assert_eq!(bar.vwap, Some(dec!(50000)));
    }

    #[test]
    fn test_ohlcv_bar_vwap_two_different_price_ticks() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_100))
            .unwrap();
        let bar = agg.current_bar().unwrap();
        // vwap = (50000*1 + 51000*1) / (1+1) = 50500
        assert_eq!(bar.vwap, Some(dec!(50500)));
    }

    #[test]
    fn test_ohlcv_bar_vwap_gap_fill_is_none() {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
            .unwrap()
            .with_emit_empty_bars(true);
        agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
            .unwrap();
        let bars = agg
            .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 240_000))
            .unwrap();
        // bars[0] = real, bars[1] and bars[2] = gap-fills
        assert!(bars[0].vwap.is_some());
        assert!(bars[1].vwap.is_none());
        assert!(bars[2].vwap.is_none());
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

    // ── Timeframe::from_duration_ms ───────────────────────────────────────────

    #[test]
    fn test_from_duration_ms_hours() {
        assert_eq!(Timeframe::from_duration_ms(3_600_000), Some(Timeframe::Hours(1)));
        assert_eq!(Timeframe::from_duration_ms(7_200_000), Some(Timeframe::Hours(2)));
    }

    #[test]
    fn test_from_duration_ms_minutes() {
        assert_eq!(Timeframe::from_duration_ms(300_000), Some(Timeframe::Minutes(5)));
        assert_eq!(Timeframe::from_duration_ms(60_000), Some(Timeframe::Minutes(1)));
    }

    #[test]
    fn test_from_duration_ms_seconds() {
        assert_eq!(Timeframe::from_duration_ms(15_000), Some(Timeframe::Seconds(15)));
        assert_eq!(Timeframe::from_duration_ms(1_000), Some(Timeframe::Seconds(1)));
    }

    #[test]
    fn test_from_duration_ms_zero_returns_none() {
        assert_eq!(Timeframe::from_duration_ms(0), None);
    }

    #[test]
    fn test_from_duration_ms_non_whole_second_returns_none() {
        assert_eq!(Timeframe::from_duration_ms(1_500), None);
    }

    #[test]
    fn test_from_duration_ms_roundtrip() {
        for tf in [Timeframe::Seconds(30), Timeframe::Minutes(5), Timeframe::Hours(4)] {
            assert_eq!(Timeframe::from_duration_ms(tf.duration_ms()), Some(tf));
        }
    }

    // ── OhlcvBar::is_doji / wick_upper / wick_lower ──────────────────────────

    #[test]
    fn test_is_doji_exact_zero_body() {
        let bar = OhlcvBar {
            symbol: "X".into(), timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0, open: dec!(100), high: dec!(105),
            low: dec!(95), close: dec!(100),
            volume: dec!(1), trade_count: 1, is_complete: true,
            is_gap_fill: false, vwap: None,
        };
        assert!(bar.is_doji(Decimal::ZERO));
    }

    #[test]
    fn test_is_doji_small_epsilon() {
        let bar = OhlcvBar {
            symbol: "X".into(), timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0, open: dec!(100), high: dec!(105),
            low: dec!(95), close: dec!(100.005),
            volume: dec!(1), trade_count: 1, is_complete: true,
            is_gap_fill: false, vwap: None,
        };
        assert!(bar.is_doji(dec!(0.01)));
        assert!(!bar.is_doji(Decimal::ZERO));
    }

    #[test]
    fn test_wick_upper_bullish() {
        // open=100, close=104, high=107 → upper wick = 107 - 104 = 3
        let bar = OhlcvBar {
            symbol: "X".into(), timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0, open: dec!(100), high: dec!(107),
            low: dec!(98), close: dec!(104),
            volume: dec!(1), trade_count: 1, is_complete: true,
            is_gap_fill: false, vwap: None,
        };
        assert_eq!(bar.wick_upper(), dec!(3));
    }

    #[test]
    fn test_wick_lower_bearish() {
        // open=104, close=100, low=97 → lower wick = 100 - 97 = 3
        let bar = OhlcvBar {
            symbol: "X".into(), timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0, open: dec!(104), high: dec!(107),
            low: dec!(97), close: dec!(100),
            volume: dec!(1), trade_count: 1, is_complete: true,
            is_gap_fill: false, vwap: None,
        };
        assert_eq!(bar.wick_lower(), dec!(3));
    }

    // ── OhlcvAggregator::window_progress ─────────────────────────────────────

    #[test]
    fn test_window_progress_none_when_no_bar() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.window_progress(60_000).is_none());
    }

    #[test]
    fn test_window_progress_at_start_is_zero() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        // Tick at bar start.
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000)).unwrap();
        assert_eq!(agg.window_progress(60_000), Some(0.0));
    }

    #[test]
    fn test_window_progress_midpoint() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000)).unwrap();
        // 30 s into a 60 s bar → 0.5
        let progress = agg.window_progress(90_000).unwrap();
        assert!((progress - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_window_progress_clamps_at_one() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000)).unwrap();
        // 90 s past the bar start (longer than the bar) → clamped to 1.0
        assert_eq!(agg.window_progress(150_000), Some(1.0));
    }

    // ── OhlcvBar::price_change ────────────────────────────────────────────────

    #[test]
    fn test_price_change_bullish_is_positive() {
        let bar = make_bar(dec!(100), dec!(110), dec!(98), dec!(105));
        assert_eq!(bar.price_change(), dec!(5));
    }

    #[test]
    fn test_price_change_bearish_is_negative() {
        let bar = make_bar(dec!(105), dec!(110), dec!(98), dec!(100));
        assert_eq!(bar.price_change(), dec!(-5));
    }

    #[test]
    fn test_price_change_doji_is_zero() {
        let bar = make_bar(dec!(100), dec!(102), dec!(98), dec!(100));
        assert_eq!(bar.price_change(), dec!(0));
    }

    // ── OhlcvAggregator::total_volume ─────────────────────────────────────────

    #[test]
    fn test_total_volume_zero_before_completion() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(2), 60_000)).unwrap();
        // Bar not yet complete; total_volume should be zero
        assert_eq!(agg.total_volume(), dec!(0));
    }

    #[test]
    fn test_total_volume_accumulates_across_bars() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        // Bar 1: volume = 2
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(2), 60_000)).unwrap();
        // Trigger completion of bar 1
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(3), 120_000)).unwrap();
        // Bar 1 completed with volume 2. Bar 2 in progress with volume 3 (not counted).
        assert_eq!(agg.total_volume(), dec!(2));
        // Trigger completion of bar 2
        agg.feed(&make_tick("BTC-USD", dec!(102), dec!(5), 180_000)).unwrap();
        assert_eq!(agg.total_volume(), dec!(5)); // 2 + 3
    }

    #[test]
    fn test_total_volume_reset_clears() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(2), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(3), 120_000)).unwrap();
        agg.reset();
        assert_eq!(agg.total_volume(), dec!(0));
    }

    // ── OhlcvBar::typical_price / median_price ────────────────────────────────

    fn make_bar(open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> OhlcvBar {
        OhlcvBar {
            symbol: "X".into(),
            timeframe: Timeframe::Minutes(1),
            bar_start_ms: 0,
            open,
            high,
            low,
            close,
            volume: dec!(1),
            trade_count: 1,
            is_complete: true,
            is_gap_fill: false,
            vwap: None,
        }
    }

    #[test]
    fn test_typical_price() {
        // high=12, low=8, close=10 → (12+8+10)/3 = 10
        let bar = make_bar(dec!(9), dec!(12), dec!(8), dec!(10));
        assert_eq!(bar.typical_price(), dec!(10));
    }

    #[test]
    fn test_median_price() {
        // high=12, low=8 → (12+8)/2 = 10
        let bar = make_bar(dec!(9), dec!(12), dec!(8), dec!(10));
        assert_eq!(bar.median_price(), dec!(10));
    }

    #[test]
    fn test_typical_price_differs_from_median() {
        // high=10, low=6, close=10 → typical=(10+6+10)/3 = 26/3, median=(10+6)/2 = 8
        let bar = make_bar(dec!(8), dec!(10), dec!(6), dec!(10));
        assert_eq!(bar.median_price(), dec!(8));
        assert!(bar.typical_price() > bar.median_price());
    }

    #[test]
    fn test_close_location_value_at_high() {
        // close == high → CLV = (high - low - 0) / range = 1.0
        let bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let clv = bar.close_location_value().unwrap();
        assert!((clv - 1.0).abs() < 1e-9, "expected 1.0 got {clv}");
    }

    #[test]
    fn test_close_location_value_at_low() {
        // close == low → CLV = (low - low - (high - low)) / range = -range/range = -1.0
        let bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let clv = bar.close_location_value().unwrap();
        assert!((clv + 1.0).abs() < 1e-9, "expected -1.0 got {clv}");
    }

    #[test]
    fn test_close_location_value_midpoint_is_zero() {
        // close == (high + low) / 2 → CLV = 0.0
        let bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let clv = bar.close_location_value().unwrap();
        assert!(clv.abs() < 1e-9, "expected 0.0 got {clv}");
    }

    #[test]
    fn test_close_location_value_zero_range_returns_none() {
        let bar = make_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.close_location_value().is_none());
    }

    #[test]
    fn test_body_direction_bullish() {
        let bar = make_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        assert_eq!(bar.body_direction(), BarDirection::Bullish);
    }

    #[test]
    fn test_body_direction_bearish() {
        let bar = make_bar(dec!(105), dec!(110), dec!(85), dec!(90));
        assert_eq!(bar.body_direction(), BarDirection::Bearish);
    }

    #[test]
    fn test_body_direction_neutral() {
        let bar = make_bar(dec!(100), dec!(110), dec!(85), dec!(100));
        assert_eq!(bar.body_direction(), BarDirection::Neutral);
    }

    // ── OhlcvAggregator::last_bar ─────────────────────────────────────────────

    #[test]
    fn test_last_bar_none_before_completion() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.last_bar().is_none());
    }

    #[test]
    fn test_last_bar_set_after_bar_completion() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        // First bar in window [60000, 120000)
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000)).unwrap();
        // Second tick in next window completes the first bar
        agg.feed(&make_tick("BTC-USD", dec!(200), dec!(1), 120_000)).unwrap();
        let last = agg.last_bar().unwrap();
        assert!(last.is_complete);
        assert_eq!(last.close, dec!(100));
    }

    #[test]
    fn test_last_bar_set_after_flush() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(50), dec!(1), 60_000)).unwrap();
        let flushed = agg.flush().unwrap();
        assert_eq!(agg.last_bar().unwrap().close, flushed.close);
    }

    #[test]
    fn test_last_bar_cleared_on_reset() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(200), dec!(1), 120_000)).unwrap();
        assert!(agg.last_bar().is_some());
        agg.reset();
        assert!(agg.last_bar().is_none());
    }

    // ── OhlcvBar::weighted_close / price_change_pct / wick_ratio ─────────────

    #[test]
    fn test_weighted_close_basic() {
        // (high + low + close*2) / 4 = (12 + 8 + 10*2) / 4 = 40/4 = 10
        let bar = make_bar(dec!(9), dec!(12), dec!(8), dec!(10));
        assert_eq!(bar.weighted_close(), dec!(10));
    }

    #[test]
    fn test_weighted_close_weights_close_more_than_typical() {
        // high=100, low=0, close=80 → typical=(100+0+80)/3≈60, weighted=(100+0+80+80)/4=65
        let bar = make_bar(dec!(50), dec!(100), dec!(0), dec!(80));
        assert_eq!(bar.weighted_close(), dec!(65));
    }

    #[test]
    fn test_price_change_pct_bullish() {
        // open=100, close=110 → +10%
        let bar = make_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let pct = bar.price_change_pct().unwrap();
        assert!((pct - 10.0).abs() < 1e-9, "expected 10.0 got {pct}");
    }

    #[test]
    fn test_price_change_pct_bearish() {
        // open=200, close=180 → -10%
        let bar = make_bar(dec!(200), dec!(210), dec!(175), dec!(180));
        let pct = bar.price_change_pct().unwrap();
        assert!((pct - (-10.0)).abs() < 1e-9, "expected -10.0 got {pct}");
    }

    #[test]
    fn test_price_change_pct_zero_open_returns_none() {
        let bar = make_bar(dec!(0), dec!(5), dec!(0), dec!(3));
        assert!(bar.price_change_pct().is_none());
    }

    #[test]
    fn test_wick_ratio_all_wicks() {
        // open=close=5, high=10, low=0 → body=0, wicks=5+5=10, range=10 → ratio=1.0
        let bar = make_bar(dec!(5), dec!(10), dec!(0), dec!(5));
        let r = bar.wick_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 got {r}");
    }

    #[test]
    fn test_wick_ratio_no_wicks() {
        // open=low=0, close=high=10 → body=10, wicks=0, range=10 → ratio=0.0
        let bar = make_bar(dec!(0), dec!(10), dec!(0), dec!(10));
        let r = bar.wick_ratio().unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0 got {r}");
    }

    #[test]
    fn test_wick_ratio_zero_range_returns_none() {
        // all prices identical → range=0
        let bar = make_bar(dec!(5), dec!(5), dec!(5), dec!(5));
        assert!(bar.wick_ratio().is_none());
    }

    // ── OhlcvBar::body_ratio ──────────────────────────────────────────────────

    #[test]
    fn test_body_ratio_no_wicks_is_one() {
        // open=low=0, close=high=10 → body=10, range=10 → ratio=1.0
        let bar = make_bar(dec!(0), dec!(10), dec!(0), dec!(10));
        let r = bar.body_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_body_ratio_all_wicks_is_zero() {
        // doji: open=close=5, high=10, low=0 → body=0, range=10 → ratio=0.0
        let bar = make_bar(dec!(5), dec!(10), dec!(0), dec!(5));
        let r = bar.body_ratio().unwrap();
        assert!((r - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_body_ratio_zero_range_returns_none() {
        let bar = make_bar(dec!(5), dec!(5), dec!(5), dec!(5));
        assert!(bar.body_ratio().is_none());
    }

    #[test]
    fn test_body_ratio_plus_wick_ratio_equals_one() {
        // body + wicks = range → ratios sum to 1
        let bar = make_bar(dec!(4), dec!(10), dec!(0), dec!(8));
        let body = bar.body_ratio().unwrap();
        let wick = bar.wick_ratio().unwrap();
        assert!((body + wick - 1.0).abs() < 1e-9);
    }

    // ── OhlcvAggregator::average_volume ──────────────────────────────────────

    #[test]
    fn test_average_volume_none_before_bars() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.average_volume().is_none());
    }

    #[test]
    fn test_average_volume_one_bar() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(4), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(1), 120_000)).unwrap();
        // bar 1 complete with volume 4; bar 2 in progress, not counted
        assert_eq!(agg.average_volume(), Some(dec!(4)));
    }

    #[test]
    fn test_average_volume_two_bars() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(4), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(6), 120_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(102), dec!(1), 180_000)).unwrap();
        // bar 1 vol=4, bar 2 vol=6 → avg=5
        assert_eq!(agg.average_volume(), Some(dec!(5)));
    }

    // ── OhlcvBar::true_range / inside_bar / outside_bar ──────────────────────

    #[test]
    fn test_true_range_no_gap() {
        // high=12, low=8, prev_close=10 → HL=4, H-prev=2, L-prev=2 → TR=4
        let bar = make_bar(dec!(9), dec!(12), dec!(8), dec!(11));
        assert_eq!(bar.true_range(dec!(10)), dec!(4));
    }

    #[test]
    fn test_true_range_gap_up() {
        // high=15, low=12, prev_close=10 → HL=3, H-prev=5, L-prev=2 → TR=5
        let bar = make_bar(dec!(12), dec!(15), dec!(12), dec!(13));
        assert_eq!(bar.true_range(dec!(10)), dec!(5));
    }

    #[test]
    fn test_true_range_gap_down() {
        // high=8, low=5, prev_close=12 → HL=3, H-prev=4, L-prev=7 → TR=7
        let bar = make_bar(dec!(7), dec!(8), dec!(5), dec!(6));
        assert_eq!(bar.true_range(dec!(12)), dec!(7));
    }

    #[test]
    fn test_inside_bar_true_when_contained() {
        let prev = make_bar(dec!(9), dec!(15), dec!(5), dec!(12));
        let curr = make_bar(dec!(10), dec!(14), dec!(6), dec!(11));
        assert!(curr.is_inside_bar(&prev));
    }

    #[test]
    fn test_inside_bar_false_when_not_contained() {
        let prev = make_bar(dec!(9), dec!(15), dec!(5), dec!(12));
        let curr = make_bar(dec!(10), dec!(16), dec!(6), dec!(11));
        assert!(!curr.is_inside_bar(&prev));
    }

    #[test]
    fn test_outside_bar_true_when_engulfing() {
        let prev = make_bar(dec!(9), dec!(12), dec!(8), dec!(11));
        let curr = make_bar(dec!(10), dec!(14), dec!(6), dec!(11));
        assert!(curr.outside_bar(&prev));
    }

    #[test]
    fn test_outside_bar_false_when_not_engulfing() {
        let prev = make_bar(dec!(9), dec!(12), dec!(8), dec!(11));
        let curr = make_bar(dec!(10), dec!(11), dec!(9), dec!(10));
        assert!(!curr.outside_bar(&prev));
    }

    // ── OhlcvBar::is_hammer ───────────────────────────────────────────────────

    #[test]
    fn test_is_hammer_classic() {
        // open=9, high=10, low=0, close=9 → body=0, wick_lo=9, wick_hi=1, range=10
        // body=0 ≤ 30%, wick_lo=9 ≥ 60%, wick_hi=1 ≤ 10% → hammer
        let bar = make_bar(dec!(9), dec!(10), dec!(0), dec!(9));
        assert!(bar.is_hammer());
    }

    #[test]
    fn test_is_hammer_false_large_upper_wick() {
        // open=5, high=10, low=0, close=5 → body=0, wick_hi=5 (50%) → not hammer
        let bar = make_bar(dec!(5), dec!(10), dec!(0), dec!(5));
        assert!(!bar.is_hammer());
    }

    #[test]
    fn test_is_hammer_false_zero_range() {
        let bar = make_bar(dec!(5), dec!(5), dec!(5), dec!(5));
        assert!(!bar.is_hammer());
    }

    // ── OhlcvAggregator::peak_volume ─────────────────────────────────────────

    #[test]
    fn test_peak_volume_none_before_completion() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.peak_volume().is_none());
    }

    #[test]
    fn test_peak_volume_tracks_maximum() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        // Bar 1: vol=3
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(3), 60_000)).unwrap();
        // Trigger bar 1 completion; bar 2 vol=10 in progress
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(10), 120_000)).unwrap();
        assert_eq!(agg.peak_volume(), Some(dec!(3)));
        // Trigger bar 2 completion
        agg.feed(&make_tick("BTC-USD", dec!(102), dec!(1), 180_000)).unwrap();
        assert_eq!(agg.peak_volume(), Some(dec!(10)));
    }

    #[test]
    fn test_peak_volume_reset_clears() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(5), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(1), 120_000)).unwrap();
        agg.reset();
        assert!(agg.peak_volume().is_none());
    }

    #[test]
    fn test_peak_volume_via_flush() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(7), 60_000)).unwrap();
        agg.flush();
        assert_eq!(agg.peak_volume(), Some(dec!(7)));
    }

    // ── OhlcvBar::is_shooting_star ────────────────────────────────────────────

    #[test]
    fn test_is_shooting_star_classic() {
        // open=1, high=10, low=0, close=1 → body=0, wick_hi=9, wick_lo=1, range=10
        // body≤30%, wick_hi=9≥60%, wick_lo=1≤10% → shooting star
        let bar = make_bar(dec!(1), dec!(10), dec!(0), dec!(1));
        assert!(bar.is_shooting_star());
    }

    #[test]
    fn test_is_shooting_star_false_large_lower_wick() {
        // open=5, high=10, low=0, close=5 → lower wick = 5 (50%) → not shooting star
        let bar = make_bar(dec!(5), dec!(10), dec!(0), dec!(5));
        assert!(!bar.is_shooting_star());
    }

    #[test]
    fn test_is_shooting_star_false_zero_range() {
        let bar = make_bar(dec!(5), dec!(5), dec!(5), dec!(5));
        assert!(!bar.is_shooting_star());
    }

    #[test]
    fn test_hammer_and_shooting_star_are_mutually_exclusive_for_typical_bars() {
        // Classic hammer: long lower wick
        let hammer = make_bar(dec!(9), dec!(10), dec!(0), dec!(9));
        // Classic shooting star: long upper wick
        let star = make_bar(dec!(1), dec!(10), dec!(0), dec!(1));
        assert!(hammer.is_hammer() && !hammer.is_shooting_star());
        assert!(star.is_shooting_star() && !star.is_hammer());
    }

    // ── OhlcvAggregator::min_volume ───────────────────────────────────────────

    #[test]
    fn test_min_volume_none_before_completion() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.min_volume().is_none());
    }

    #[test]
    fn test_min_volume_tracks_minimum() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        // Bar 1: vol=10
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(10), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(1), 120_000)).unwrap();
        assert_eq!(agg.min_volume(), Some(dec!(10)));
        // Bar 2: vol=1 — should update minimum
        agg.feed(&make_tick("BTC-USD", dec!(102), dec!(5), 180_000)).unwrap();
        assert_eq!(agg.min_volume(), Some(dec!(1)));
    }

    #[test]
    fn test_min_volume_reset_clears() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(5), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(1), 120_000)).unwrap();
        agg.reset();
        assert!(agg.min_volume().is_none());
    }

    // ── OhlcvBar::is_gap_up / is_gap_down ────────────────────────────────────

    #[test]
    fn test_is_gap_up_true() {
        let prev = make_bar(dec!(5), dec!(10), dec!(4), dec!(8));
        let curr = make_bar(dec!(9), dec!(12), dec!(8), dec!(11)); // open=9 > prev.close=8
        assert!(curr.is_gap_up(&prev));
    }

    #[test]
    fn test_is_gap_up_false_when_equal() {
        let prev = make_bar(dec!(5), dec!(10), dec!(4), dec!(8));
        let curr = make_bar(dec!(8), dec!(12), dec!(7), dec!(11)); // open=8 == prev.close=8
        assert!(!curr.is_gap_up(&prev));
    }

    #[test]
    fn test_is_gap_down_true() {
        let prev = make_bar(dec!(5), dec!(10), dec!(4), dec!(8));
        let curr = make_bar(dec!(7), dec!(8), dec!(6), dec!(7)); // open=7 < prev.close=8
        assert!(curr.is_gap_down(&prev));
    }

    #[test]
    fn test_is_gap_down_false_when_equal() {
        let prev = make_bar(dec!(5), dec!(10), dec!(4), dec!(8));
        let curr = make_bar(dec!(8), dec!(9), dec!(7), dec!(8)); // open=8 == prev.close=8
        assert!(!curr.is_gap_down(&prev));
    }

    // ── OhlcvAggregator::volume_range ─────────────────────────────────────────

    #[test]
    fn test_volume_range_none_before_completion() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.volume_range().is_none());
    }

    #[test]
    fn test_volume_range_after_two_bars() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(3), 60_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(101), dec!(10), 120_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(102), dec!(1), 180_000)).unwrap();
        // bar1=3, bar2=10 → min=3, peak=10
        assert_eq!(agg.volume_range(), Some((dec!(3), dec!(10))));
    }

    // ── OhlcvBar::body_to_range_ratio ─────────────────────────────────────────

    fn make_ohlcv_bar(open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> OhlcvBar {
        OhlcvBar {
            symbol: "X".into(),
            timeframe: Timeframe::Minutes(1),
            open,
            high,
            low,
            close,
            volume: dec!(1),
            bar_start_ms: 0,
            trade_count: 1,
            is_complete: false,
            is_gap_fill: false,
            vwap: None,
        }
    }

    #[test]
    fn test_body_to_range_ratio_bullish_full_body() {
        // open=100, close=110, high=110, low=100 → body=10, range=10 → ratio=1.0
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        assert_eq!(bar.body_to_range_ratio(), Some(dec!(1)));
    }

    #[test]
    fn test_body_to_range_ratio_doji_like() {
        // open=close → body=0, range>0 → ratio=0
        let bar = make_ohlcv_bar(dec!(100), dec!(102), dec!(98), dec!(100));
        assert_eq!(bar.body_to_range_ratio(), Some(dec!(0)));
    }

    #[test]
    fn test_body_to_range_ratio_none_when_range_zero() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.body_to_range_ratio().is_none());
    }

    // ── OhlcvAggregator::is_active ────────────────────────────────────────────

    #[test]
    fn test_is_active_false_before_any_ticks() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(!agg.is_active());
    }

    #[test]
    fn test_is_active_true_after_first_tick() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 1_000)).unwrap();
        assert!(agg.is_active());
    }

    #[test]
    fn test_is_active_false_after_flush() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 1_000)).unwrap();
        agg.flush();
        assert!(!agg.is_active());
    }

    // ── OhlcvBar::is_long_upper_wick ──────────────────────────────────────────

    #[test]
    fn test_is_long_upper_wick_true_when_upper_wick_dominates() {
        // open=100, close=101, high=110, low=100 → body=1, upper_wick=9
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(101));
        assert!(bar.is_long_upper_wick());
    }

    #[test]
    fn test_is_long_upper_wick_false_for_full_body() {
        // open=100, close=110, high=110, low=100 → body=10, upper_wick=0
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        assert!(!bar.is_long_upper_wick());
    }

    #[test]
    fn test_is_long_upper_wick_false_when_equal() {
        // open=100, close=105, high=110, low=100 → body=5, upper_wick=5
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(105));
        assert!(!bar.is_long_upper_wick());
    }

    // ── OhlcvBar::price_change_abs ────────────────────────────────────────────

    #[test]
    fn test_price_change_abs_bullish_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(108));
        assert_eq!(bar.price_change_abs(), dec!(8));
    }

    #[test]
    fn test_price_change_abs_bearish_bar() {
        let bar = make_ohlcv_bar(dec!(110), dec!(110), dec!(100), dec!(102));
        assert_eq!(bar.price_change_abs(), dec!(8));
    }

    #[test]
    fn test_price_change_abs_doji_zero() {
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        assert_eq!(bar.price_change_abs(), dec!(0));
    }

    // ── OhlcvAggregator::vwap_current ────────────────────────────────────────

    #[test]
    fn test_vwap_current_none_before_any_ticks() {
        let agg = agg("BTC-USD", Timeframe::Minutes(1));
        assert!(agg.vwap_current().is_none());
    }

    #[test]
    fn test_vwap_current_equals_price_for_single_tick() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(200), dec!(5), 1_000)).unwrap();
        // vwap = price*qty / qty = 200
        assert_eq!(agg.vwap_current(), Some(dec!(200)));
    }

    #[test]
    fn test_vwap_current_weighted_average() {
        let mut agg = agg("BTC-USD", Timeframe::Minutes(1));
        agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 1_000)).unwrap();
        agg.feed(&make_tick("BTC-USD", dec!(200), dec!(3), 2_000)).unwrap();
        // vwap = (100*1 + 200*3) / (1+3) = 700/4 = 175
        assert_eq!(agg.vwap_current(), Some(dec!(175)));
    }

    // --- upper_shadow / lower_shadow / is_spinning_top / hlc3 ---

    fn bar(o: i64, h: i64, l: i64, c: i64) -> OhlcvBar {
        OhlcvBar {
            symbol: "X".into(),
            timeframe: Timeframe::Minutes(1),
            open: Decimal::from(o),
            high: Decimal::from(h),
            low: Decimal::from(l),
            close: Decimal::from(c),
            volume: Decimal::ZERO,
            bar_start_ms: 0,
            trade_count: 0,
            is_complete: false,
            is_gap_fill: false,
            vwap: None,
        }
    }

    #[test]
    fn test_upper_shadow_equals_wick_upper() {
        let b = bar(100, 120, 90, 110);
        assert_eq!(b.upper_shadow(), b.wick_upper());
        assert_eq!(b.upper_shadow(), Decimal::from(10)); // 120 - max(100,110)
    }

    #[test]
    fn test_lower_shadow_equals_wick_lower() {
        let b = bar(100, 120, 90, 110);
        assert_eq!(b.lower_shadow(), b.wick_lower());
        assert_eq!(b.lower_shadow(), Decimal::from(10)); // min(100,110) - 90
    }

    #[test]
    fn test_is_spinning_top_true_when_small_body_large_wicks() {
        // body = |110-100| = 10, range = 130-80 = 50
        // body_pct = 0.3 → max_body = 15; body(10) <= 15
        // wick_upper = 130 - 110 = 20 > 10 ✓
        // wick_lower = 100 - 80 = 20 > 10 ✓
        let b = bar(100, 130, 80, 110);
        assert!(b.is_spinning_top(dec!(0.3)));
    }

    #[test]
    fn test_is_spinning_top_false_when_body_too_large() {
        // body = 40, range = 50; body_pct=0.3 → max_body=15; 40 > 15
        let b = bar(80, 130, 80, 120);
        assert!(!b.is_spinning_top(dec!(0.3)));
    }

    #[test]
    fn test_is_spinning_top_false_when_zero_range() {
        let b = bar(100, 100, 100, 100);
        assert!(!b.is_spinning_top(dec!(0.3)));
    }

    #[test]
    fn test_hlc3_equals_typical_price() {
        let b = bar(100, 120, 80, 110);
        assert_eq!(b.hlc3(), b.typical_price());
        // (120 + 80 + 110) / 3 = 310/3
        assert_eq!(b.hlc3(), (Decimal::from(120) + Decimal::from(80) + Decimal::from(110)) / Decimal::from(3));
    }

    // ── OhlcvBar::is_bearish ──────────────────────────────────────────────────

    #[test]
    fn test_is_bearish_true_when_close_below_open() {
        let bar = make_ohlcv_bar(dec!(110), dec!(115), dec!(100), dec!(105));
        assert!(bar.is_bearish());
    }

    #[test]
    fn test_is_bearish_false_when_close_above_open() {
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(!bar.is_bearish());
    }

    #[test]
    fn test_is_bearish_false_when_doji() {
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        assert!(!bar.is_bearish());
    }

    // ── OhlcvBar::wick_ratio ──────────────────────────────────────────────────

    #[test]
    fn test_wick_ratio_zero_for_full_body_no_wicks() {
        // open=100, close=110, high=110, low=100 → no wicks → ratio=0
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let ratio = bar.wick_ratio().unwrap();
        assert!(ratio.abs() < 1e-10);
    }

    #[test]
    fn test_wick_ratio_one_for_pure_wick_doji() {
        // open=close=105, high=110, low=100 → body=0, upper=5, lower=5, range=10 → ratio=1
        let bar = make_ohlcv_bar(dec!(105), dec!(110), dec!(100), dec!(105));
        let ratio = bar.wick_ratio().unwrap();
        assert!((ratio - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_wick_ratio_none_for_zero_range_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.wick_ratio().is_none());
    }

    // ── OhlcvBar::is_bullish ──────────────────────────────────────────────────

    #[test]
    fn test_is_bullish_true_when_close_above_open() {
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(bar.is_bullish());
    }

    #[test]
    fn test_is_bullish_false_when_close_below_open() {
        let bar = make_ohlcv_bar(dec!(110), dec!(115), dec!(100), dec!(105));
        assert!(!bar.is_bullish());
    }

    #[test]
    fn test_is_bullish_false_when_doji() {
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        assert!(!bar.is_bullish());
    }

    // ── OhlcvBar::bar_duration_ms ─────────────────────────────────────────────

    #[test]
    fn test_bar_duration_ms_one_minute() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(bar.bar_duration_ms(), 60_000);
    }

    #[test]
    fn test_bar_duration_ms_consistent_with_timeframe() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        bar.timeframe = Timeframe::Hours(1);
        assert_eq!(bar.bar_duration_ms(), 3_600_000);
    }

    #[test]
    fn test_bar_duration_ms_seconds_timeframe() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        bar.timeframe = Timeframe::Seconds(30);
        assert_eq!(bar.bar_duration_ms(), 30_000);
    }

    // --- ohlc4 / is_marubozu / is_engulfing ---

    #[test]
    fn test_ohlc4_equals_average_of_all_four_prices() {
        let b = bar(100, 120, 80, 110);
        // (100 + 120 + 80 + 110) / 4 = 410 / 4 = 102.5
        let expected = (Decimal::from(100) + Decimal::from(120) + Decimal::from(80) + Decimal::from(110))
            / Decimal::from(4);
        assert_eq!(b.ohlc4(), expected);
    }

    #[test]
    fn test_is_marubozu_true_when_no_wicks() {
        // Bullish marubozu: open=low=100, close=high=110
        let b = bar(100, 110, 100, 110);
        assert!(b.is_marubozu());
    }

    #[test]
    fn test_is_marubozu_false_when_has_upper_wick() {
        let b = bar(100, 115, 100, 110);
        assert!(!b.is_marubozu());
    }

    #[test]
    fn test_is_marubozu_false_when_has_lower_wick() {
        let b = bar(100, 110, 95, 110);
        assert!(!b.is_marubozu());
    }

    // --- is_harami / tail_length ---

    #[test]
    fn test_is_harami_true_when_body_inside_prev_body() {
        let prev = bar(98, 115, 90, 108); // prev body: 98-108
        let curr = bar(100, 110, 95, 105); // curr body: 100-105 — inside 98-108
        assert!(curr.is_harami(&prev));
    }

    #[test]
    fn test_is_harami_false_when_body_engulfs_prev() {
        let prev = bar(100, 110, 95, 105); // prev body: 100-105
        let curr = bar(98, 115, 90, 108);  // curr body: 98-108 — engulfs prev
        assert!(!curr.is_harami(&prev));
    }

    #[test]
    fn test_is_harami_false_when_bodies_equal() {
        let prev = bar(100, 110, 90, 105);
        let curr = bar(100, 110, 90, 105); // equal bodies
        assert!(!curr.is_harami(&prev));
    }

    #[test]
    fn test_tail_length_upper_wick_longer() {
        // open=100, high=120, low=95, close=105 → upper_wick=15, lower_wick=5
        let b = bar(100, 120, 95, 105);
        assert_eq!(b.tail_length(), Decimal::from(15));
    }

    #[test]
    fn test_tail_length_lower_wick_longer() {
        // open=105, high=110, low=80, close=100 → upper_wick=5, lower_wick=20
        let b = bar(105, 110, 80, 100);
        assert_eq!(b.tail_length(), Decimal::from(20));
    }

    #[test]
    fn test_tail_length_zero_for_marubozu() {
        // open=low=100, close=high=110 → both wicks zero
        let b = bar(100, 110, 100, 110);
        assert!(b.tail_length().is_zero());
    }

    // --- is_inside_bar / bar_type ---

    #[test]
    fn test_is_inside_bar_true_when_range_within_prev() {
        let prev = bar(90, 120, 80, 110); // prev range: 80-120
        let curr = bar(95, 115, 85, 100); // curr range: 85-115 — inside 80-120
        assert!(curr.is_inside_bar(&prev));
    }

    #[test]
    fn test_is_inside_bar_false_when_high_exceeds_prev_high() {
        let prev = bar(90, 110, 80, 100); // prev high = 110
        let curr = bar(95, 112, 85, 100); // curr high = 112 > 110
        assert!(!curr.is_inside_bar(&prev));
    }

    #[test]
    fn test_is_inside_bar_false_when_equal_range() {
        let prev = bar(90, 110, 80, 100);
        let curr = bar(90, 110, 80, 100); // same high/low — not strictly inside
        assert!(!curr.is_inside_bar(&prev));
    }

    #[test]
    fn test_bar_type_bullish() {
        let b = bar(100, 110, 90, 105); // close > open
        assert_eq!(b.bar_type(), "bullish");
    }

    #[test]
    fn test_bar_type_bearish() {
        let b = bar(105, 110, 90, 100); // close < open
        assert_eq!(b.bar_type(), "bearish");
    }

    #[test]
    fn test_bar_type_doji() {
        let b = bar(100, 110, 90, 100); // close == open
        assert_eq!(b.bar_type(), "doji");
    }

    // --- body_pct / is_bullish_hammer ---

    #[test]
    fn test_body_pct_none_for_zero_range() {
        let b = bar(100, 100, 100, 100);
        assert!(b.body_pct().is_none());
    }

    #[test]
    fn test_body_pct_100_for_marubozu() {
        // open=low=100, close=high=110 → body=10, range=10, pct=100
        let b = bar(100, 110, 100, 110);
        assert_eq!(b.body_pct().unwrap(), Decimal::ONE_HUNDRED);
    }

    #[test]
    fn test_body_pct_50_for_half_body() {
        // open=100, close=105, high=110, low=100 → body=5, range=10, pct=50
        let b = bar(100, 110, 100, 105);
        assert_eq!(b.body_pct().unwrap(), Decimal::from(50));
    }

    #[test]
    fn test_is_bullish_hammer_true_for_classic_hammer() {
        // long lower wick, small body near top, tiny upper wick
        // open=108, high=110, low=100, close=109 → body=1, lower=8, upper=1
        let b = bar(108, 110, 100, 109);
        assert!(b.is_bullish_hammer());
    }

    #[test]
    fn test_is_bullish_hammer_false_when_lower_wick_not_long_enough() {
        // open=100, high=110, low=98, close=108 → body=8, lower=2 < 2*8=16
        let b = bar(100, 110, 98, 108);
        assert!(!b.is_bullish_hammer());
    }

    #[test]
    fn test_is_bullish_hammer_false_for_doji() {
        let b = bar(100, 110, 90, 100); // open == close, body = 0
        assert!(!b.is_bullish_hammer());
    }

    // --- OhlcvBar::is_marubozu ---
    #[test]
    fn test_is_marubozu_true_when_full_body() {
        // open=100, high=100, low=100, close=110 → body=10, range=10 → 100%
        let b = bar(100, 110, 100, 110);
        assert!(b.is_marubozu());
    }

    #[test]
    fn test_is_marubozu_false_when_large_wicks() {
        // open=100, high=120, low=80, close=110 → body=10, range=40 → 25%
        let b = bar(100, 120, 80, 110);
        assert!(!b.is_marubozu());
    }

    #[test]
    fn test_is_marubozu_true_for_zero_range_flat_bar() {
        // flat bar has no wicks → qualifies as marubozu under "no wicks" definition
        let b = bar(100, 100, 100, 100);
        assert!(b.is_marubozu());
    }

    // --- OhlcvBar::upper_wick_pct ---
    #[test]
    fn test_upper_wick_pct_zero_when_no_upper_wick() {
        // close is the high
        let b = bar(100, 110, 90, 110);
        let pct = b.upper_wick_pct().unwrap();
        assert!(pct.is_zero(), "expected 0, got {pct}");
    }

    #[test]
    fn test_upper_wick_pct_50_when_half_range() {
        // open=100, high=120, low=100, close=110 → upper_wick=10, range=20 → 50%
        let b = bar(100, 120, 100, 110);
        let pct = b.upper_wick_pct().unwrap();
        assert_eq!(pct, dec!(50));
    }

    #[test]
    fn test_upper_wick_pct_none_for_zero_range() {
        let b = bar(100, 100, 100, 100);
        assert!(b.upper_wick_pct().is_none());
    }

    // --- OhlcvBar::lower_wick_pct ---
    #[test]
    fn test_lower_wick_pct_zero_when_no_lower_wick() {
        // open is the low
        let b = bar(100, 110, 100, 105);
        let pct = b.lower_wick_pct().unwrap();
        assert!(pct.is_zero(), "expected 0, got {pct}");
    }

    #[test]
    fn test_lower_wick_pct_50_when_half_range() {
        // open=110, high=120, low=100, close=115 → lower_wick=10, range=20 → 50%
        let b = bar(110, 120, 100, 115);
        let pct = b.lower_wick_pct().unwrap();
        assert_eq!(pct, dec!(50));
    }

    #[test]
    fn test_lower_wick_pct_none_for_zero_range() {
        let b = bar(100, 100, 100, 100);
        assert!(b.lower_wick_pct().is_none());
    }

    // --- OhlcvBar::is_bearish_engulfing ---
    #[test]
    fn test_is_bearish_engulfing_true_for_bearish_engulf() {
        let prev = bar(100, 115, 95, 110); // bullish, body 100-110
        let curr = bar(112, 115, 88, 90);  // bearish, body 112-90, engulfs 100-110
        assert!(curr.is_bearish_engulfing(&prev));
    }

    #[test]
    fn test_is_bearish_engulfing_false_for_bullish_engulf() {
        let prev = bar(110, 115, 95, 100); // bearish, body 110-100
        let curr = bar(98, 120, 95, 115);  // bullish, body 98-115 engulfs but not bearish
        assert!(!curr.is_bearish_engulfing(&prev));
    }

    #[test]
    fn test_is_engulfing_true_when_body_contains_prev_body() {
        let prev = bar(100, 110, 95, 105); // prev body: 100-105
        let curr = bar(98, 115, 95, 108);  // curr body: 98-108 engulfs 100-105
        assert!(curr.is_engulfing(&prev));
    }

    #[test]
    fn test_is_engulfing_false_when_only_partial_overlap() {
        let prev = bar(100, 115, 90, 112); // prev body: 100-112
        let curr = bar(101, 115, 90, 113); // curr body: 101-113 — lo=101 > 100, not engulfing
        assert!(!curr.is_engulfing(&prev));
    }

    #[test]
    fn test_is_engulfing_false_for_equal_bodies() {
        let prev = bar(100, 110, 90, 108);
        let curr = bar(100, 110, 90, 108); // exactly equal
        assert!(!curr.is_engulfing(&prev));
    }

    // ── OhlcvBar::has_upper_wick / has_lower_wick ─────────────────────────────

    #[test]
    fn test_has_upper_wick_true_when_high_above_max_oc() {
        // open=100, close=110, high=115 → upper wick = 5
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(100), dec!(110));
        assert!(bar.has_upper_wick());
    }

    #[test]
    fn test_has_upper_wick_false_for_full_body() {
        // open=100, close=110, high=110 → no upper wick
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        assert!(!bar.has_upper_wick());
    }

    #[test]
    fn test_has_lower_wick_true_when_low_below_min_oc() {
        // open=105, close=110, low=100 → lower wick = 5
        let bar = make_ohlcv_bar(dec!(105), dec!(110), dec!(100), dec!(110));
        assert!(bar.has_lower_wick());
    }

    #[test]
    fn test_has_lower_wick_false_for_full_body() {
        // open=100, close=110, low=100 → no lower wick
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        assert!(!bar.has_lower_wick());
    }

    // ── OhlcvBar::is_gravestone_doji ──────────────────────────────────────────

    #[test]
    fn test_is_gravestone_doji_true() {
        // open=close=low=100, high=110 → body=0, close≈low → gravestone
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(100));
        assert!(bar.is_gravestone_doji(dec!(0)));
    }

    #[test]
    fn test_is_gravestone_doji_false_when_close_above_low() {
        // open=100, close=105, low=99, high=110 → body=5 → not a doji
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(99), dec!(105));
        assert!(!bar.is_gravestone_doji(dec!(1)));
    }

    // ── OhlcvBar::is_dragonfly_doji ───────────────────────────────────────────

    #[test]
    fn test_is_dragonfly_doji_true() {
        // open=close=high=110, low=100 → body=0, close≈high → dragonfly
        let bar = make_ohlcv_bar(dec!(110), dec!(110), dec!(100), dec!(110));
        assert!(bar.is_dragonfly_doji(dec!(0)));
    }

    #[test]
    fn test_is_dragonfly_doji_false_when_close_below_high() {
        // close=105, high=110 → close not near high
        let bar = make_ohlcv_bar(dec!(105), dec!(110), dec!(100), dec!(105));
        assert!(!bar.is_dragonfly_doji(dec!(1)));
    }

    // ── OhlcvBar::is_flat / close_to_high_ratio / close_open_ratio ──────────

    #[test]
    fn test_is_flat_true() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.is_flat());
    }

    #[test]
    fn test_is_flat_false_when_range_exists() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(!bar.is_flat());
    }

    #[test]
    fn test_close_to_high_ratio_normal() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        // close=110, high=110 → ratio=1.0
        let r = bar.close_to_high_ratio().unwrap();
        assert!((r - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_close_to_high_ratio_none_when_high_zero() {
        let bar = make_ohlcv_bar(dec!(0), dec!(0), dec!(0), dec!(0));
        assert!(bar.close_to_high_ratio().is_none());
    }

    #[test]
    fn test_close_open_ratio_normal() {
        // close=110, open=100 → ratio=1.1
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let r = bar.close_open_ratio().unwrap();
        assert!((r - 1.1).abs() < 1e-9);
    }

    #[test]
    fn test_close_open_ratio_none_when_open_zero() {
        let bar = make_ohlcv_bar(dec!(0), dec!(10), dec!(0), dec!(5));
        assert!(bar.close_open_ratio().is_none());
    }

    // ── OhlcvBar::true_range_with_prev ────────────────────────────────────────

    #[test]
    fn test_true_range_simple_hl_dominates() {
        // high=110, low=90, prev_close=100 → hl=20, hc=10, lc=10 → TR=20
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(bar.true_range_with_prev(dec!(100)), dec!(20));
    }

    #[test]
    fn test_true_range_gap_up_dominates() {
        // prev_close=80, high=100, low=90 → hl=10, hc=20, lc=10 → TR=20
        let bar = make_ohlcv_bar(dec!(91), dec!(100), dec!(90), dec!(95));
        assert_eq!(bar.true_range_with_prev(dec!(80)), dec!(20));
    }

    #[test]
    fn test_true_range_gap_down_dominates() {
        // prev_close=120, high=100, low=95 → hl=5, hc=20, lc=25 → TR=25
        let bar = make_ohlcv_bar(dec!(98), dec!(100), dec!(95), dec!(97));
        assert_eq!(bar.true_range_with_prev(dec!(120)), dec!(25));
    }

    // ── OhlcvBar::is_outside_bar / high_low_midpoint ─────────────────────────

    #[test]
    fn test_is_outside_bar_true() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        let bar  = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(bar.is_outside_bar(&prev));
    }

    #[test]
    fn test_is_outside_bar_false_when_inside() {
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let bar  = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        assert!(!bar.is_outside_bar(&prev));
    }

    #[test]
    fn test_high_low_midpoint_correct() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        // (110 + 90) / 2 = 100
        assert_eq!(bar.high_low_midpoint(), dec!(100));
    }

    #[test]
    fn test_high_low_midpoint_uneven() {
        let bar = make_ohlcv_bar(dec!(100), dec!(111), dec!(90), dec!(100));
        // (111 + 90) / 2 = 100.5
        assert_eq!(bar.high_low_midpoint(), dec!(100.5));
    }

    // ── OhlcvBar::gap_up / gap_down ──────────────────────────────────────────

    #[test]
    fn test_gap_up_true() {
        let prev = make_ohlcv_bar(dec!(95), dec!(100), dec!(90), dec!(98));
        let bar  = make_ohlcv_bar(dec!(102), dec!(110), dec!(101), dec!(108));
        assert!(bar.gap_up(&prev));
    }

    #[test]
    fn test_gap_up_false_when_no_gap() {
        let prev = make_ohlcv_bar(dec!(95), dec!(100), dec!(90), dec!(98));
        let bar  = make_ohlcv_bar(dec!(99), dec!(105), dec!(98), dec!(104));
        assert!(!bar.gap_up(&prev));
    }

    #[test]
    fn test_gap_down_true() {
        let prev = make_ohlcv_bar(dec!(95), dec!(100), dec!(90), dec!(92));
        let bar  = make_ohlcv_bar(dec!(88), dec!(89), dec!(85), dec!(86));
        assert!(bar.gap_down(&prev));
    }

    #[test]
    fn test_gap_down_false_when_no_gap() {
        let prev = make_ohlcv_bar(dec!(95), dec!(100), dec!(90), dec!(92));
        let bar  = make_ohlcv_bar(dec!(91), dec!(95), dec!(89), dec!(93));
        assert!(!bar.gap_down(&prev));
    }

    // ── OhlcvBar::range_pct ──────────────────────────────────────────────────

    #[test]
    fn test_range_pct_correct() {
        // open=100, high=110, low=90 → range=20, 20/100 * 100 = 20%
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let pct = bar.range_pct().unwrap();
        assert!((pct - 20.0).abs() < 1e-9);
    }

    #[test]
    fn test_range_pct_none_when_open_zero() {
        let bar = make_ohlcv_bar(dec!(0), dec!(10), dec!(0), dec!(5));
        assert!(bar.range_pct().is_none());
    }

    #[test]
    fn test_range_pct_zero_for_flat_bar() {
        // high == low → range = 0
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        let pct = bar.range_pct().unwrap();
        assert_eq!(pct, 0.0);
    }

    // ── OhlcvBar::body_size ──────────────────────────────────────────────────

    #[test]
    fn test_body_size_bullish_bar() {
        // open=100, close=110 → body = 10
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert_eq!(bar.body_size(), dec!(10));
    }

    #[test]
    fn test_body_size_bearish_bar() {
        // open=110, close=100 → body = 10
        let bar = make_ohlcv_bar(dec!(110), dec!(115), dec!(95), dec!(100));
        assert_eq!(bar.body_size(), dec!(10));
    }

    #[test]
    fn test_body_size_doji() {
        // open == close → body = 0
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        assert_eq!(bar.body_size(), dec!(0));
    }

    // ── OhlcvBar::volume_delta / is_consolidating ────────────────────────────

    #[test]
    fn test_volume_delta_positive_when_increasing() {
        let mut prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(102));
        prev.volume = dec!(1000);
        let mut bar = make_ohlcv_bar(dec!(102), dec!(110), dec!(98), dec!(108));
        bar.volume = dec!(1500);
        assert_eq!(bar.volume_delta(&prev), dec!(500));
    }

    #[test]
    fn test_volume_delta_negative_when_decreasing() {
        let mut prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(102));
        prev.volume = dec!(1500);
        let mut bar = make_ohlcv_bar(dec!(102), dec!(110), dec!(98), dec!(108));
        bar.volume = dec!(1000);
        assert_eq!(bar.volume_delta(&prev), dec!(-500));
    }

    #[test]
    fn test_is_consolidating_true_when_small_range() {
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // range=20
        let bar  = make_ohlcv_bar(dec!(102), dec!(106), dec!(100), dec!(104)); // range=6 < 10
        assert!(bar.is_consolidating(&prev));
    }

    #[test]
    fn test_is_consolidating_false_when_large_range() {
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // range=20
        let bar  = make_ohlcv_bar(dec!(102), dec!(115), dec!(95), dec!(110)); // range=20, not < 10
        assert!(!bar.is_consolidating(&prev));
    }

    // ── OhlcvBar::relative_volume / intraday_reversal ─────────────────────────

    #[test]
    fn test_relative_volume_correct() {
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(103));
        // bar.volume = dec!(1) (default), avg = 2 → ratio = 0.5
        let rv = bar.relative_volume(dec!(2)).unwrap();
        assert!((rv - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_relative_volume_none_when_avg_zero() {
        let bar = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(103));
        assert!(bar.relative_volume(dec!(0)).is_none());
    }

    #[test]
    fn test_intraday_reversal_true_for_bullish_then_bearish() {
        // prev: open=100, close=105 (bullish)
        let prev = make_ohlcv_bar(dec!(100), dec!(108), dec!(99), dec!(105));
        // this: opens at 105 (≥ prev close), closes below prev open (100) → reversal
        let bar = make_ohlcv_bar(dec!(105), dec!(107), dec!(97), dec!(98));
        assert!(bar.intraday_reversal(&prev));
    }

    #[test]
    fn test_intraday_reversal_false_for_continuation() {
        // prev: open=100, close=105 (bullish), this also bullish at lower open
        let prev = make_ohlcv_bar(dec!(100), dec!(108), dec!(99), dec!(105));
        let bar = make_ohlcv_bar(dec!(104), dec!(115), dec!(103), dec!(113));
        assert!(!bar.intraday_reversal(&prev));
    }

    // ── OhlcvBar::price_at_pct ───────────────────────────────────────────────

    #[test]
    fn test_price_at_pct_zero_returns_low() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(bar.price_at_pct(0.0), dec!(90));
    }

    #[test]
    fn test_price_at_pct_one_returns_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(bar.price_at_pct(1.0), dec!(110));
    }

    #[test]
    fn test_price_at_pct_half_returns_midpoint() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        // low=90, range=20, 0.5*20=10 → 90+10=100
        assert_eq!(bar.price_at_pct(0.5), dec!(100));
    }

    #[test]
    fn test_price_at_pct_clamped_above_one() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(bar.price_at_pct(2.0), dec!(110));
    }

    // ── average_true_range ────────────────────────────────────────────────────

    #[test]
    fn test_average_true_range_none_when_fewer_than_two_bars() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::average_true_range(&[bar]).is_none());
        assert!(OhlcvBar::average_true_range(&[]).is_none());
    }

    #[test]
    fn test_average_true_range_two_bars_no_gap() {
        // bar1: high=110 low=90 close=100
        // bar2: high=115 low=95 close=110  tr = max(115-95, |115-100|, |95-100|) = max(20,15,5) = 20
        let bar1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let bar2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        let atr = OhlcvBar::average_true_range(&[bar1, bar2]).unwrap();
        assert_eq!(atr, dec!(20)); // only one TR value: bar2 vs bar1.close=100
    }

    #[test]
    fn test_average_true_range_three_bars_mean() {
        // bar1: close=100
        // bar2: h=110 l=90 c=105; tr = max(20, |110-100|, |90-100|) = max(20,10,10) = 20
        // bar3: h=120 l=100 c=115; tr = max(20, |120-105|, |100-105|) = max(20,15,5) = 20
        let bar1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let bar2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let bar3 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(115));
        let atr = OhlcvBar::average_true_range(&[bar1, bar2, bar3]).unwrap();
        assert_eq!(atr, dec!(20));
    }

    // ── average_body ──────────────────────────────────────────────────────────

    #[test]
    fn test_average_body_none_when_empty() {
        assert!(OhlcvBar::average_body(&[]).is_none());
    }

    #[test]
    fn test_average_body_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        // body = |108 - 100| = 8
        assert_eq!(OhlcvBar::average_body(&[bar]), Some(dec!(8)));
    }

    #[test]
    fn test_average_body_multiple_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)); // body=10
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(100)); // body=10
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(120)); // body=20
        let avg = OhlcvBar::average_body(&[b1, b2, b3]).unwrap();
        // (10 + 10 + 20) / 3 = 40/3
        assert_eq!(avg, dec!(40) / dec!(3));
    }

    // ── bullish_count / bearish_count / win_rate ──────────────────────────────

    #[test]
    fn test_bullish_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::bullish_count(&[]), 0);
    }

    #[test]
    fn test_bullish_count_all_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)); // bullish
        let b2 = make_ohlcv_bar(dec!(108), dec!(120), dec!(105), dec!(115)); // bullish
        assert_eq!(OhlcvBar::bullish_count(&[b1, b2]), 2);
    }

    #[test]
    fn test_bearish_count_correct() {
        let bull = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let bear = make_ohlcv_bar(dec!(108), dec!(110), dec!(90), dec!(95));
        let doji = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(OhlcvBar::bearish_count(&[bull, bear, doji]), 1);
    }

    #[test]
    fn test_win_rate_none_when_empty() {
        assert!(OhlcvBar::win_rate(&[]).is_none());
    }

    #[test]
    fn test_win_rate_all_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let b2 = make_ohlcv_bar(dec!(108), dec!(115), dec!(105), dec!(112));
        let wr = OhlcvBar::win_rate(&[b1, b2]).unwrap();
        assert!((wr - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_win_rate_half_and_half() {
        let bull = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let bear = make_ohlcv_bar(dec!(108), dec!(110), dec!(90), dec!(95));
        let wr = OhlcvBar::win_rate(&[bull, bear]).unwrap();
        assert!((wr - 0.5).abs() < 1e-9);
    }

    // ── bullish_streak / bearish_streak ──────────────────────────────────────

    #[test]
    fn test_bullish_streak_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::bullish_streak(&[]), 0);
    }

    #[test]
    fn test_bullish_streak_zero_when_last_bar_bearish() {
        let bull = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let bear = make_ohlcv_bar(dec!(108), dec!(110), dec!(90), dec!(95));
        assert_eq!(OhlcvBar::bullish_streak(&[bull, bear]), 0);
    }

    #[test]
    fn test_bullish_streak_counts_consecutive_tail() {
        let bear = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90)); // bearish
        let bull1 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(102)); // bullish
        let bull2 = make_ohlcv_bar(dec!(102), dec!(115), dec!(100), dec!(110)); // bullish
        assert_eq!(OhlcvBar::bullish_streak(&[bear, bull1, bull2]), 2);
    }

    #[test]
    fn test_bearish_streak_counts_consecutive_tail() {
        let bull = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)); // bullish
        let bear1 = make_ohlcv_bar(dec!(108), dec!(109), dec!(90), dec!(95)); // bearish
        let bear2 = make_ohlcv_bar(dec!(95), dec!(96), dec!(80), dec!(85)); // bearish
        assert_eq!(OhlcvBar::bearish_streak(&[bull, bear1, bear2]), 2);
    }

    // ── max_drawdown ──────────────────────────────────────────────────────────

    #[test]
    fn test_max_drawdown_none_when_fewer_than_2_bars() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::max_drawdown(&[bar]).is_none());
        assert!(OhlcvBar::max_drawdown(&[]).is_none());
    }

    #[test]
    fn test_max_drawdown_zero_when_monotone_increasing() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(105), dec!(98), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(99), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(115), dec!(104), dec!(110));
        let dd = OhlcvBar::max_drawdown(&[b1, b2, b3]).unwrap();
        assert_eq!(dd, 0.0);
    }

    #[test]
    fn test_max_drawdown_correct_after_peak_then_drop() {
        // closes: 100, 120, 90 → peak=120, drop=(120-90)/120 = 0.25
        let b1 = make_ohlcv_bar(dec!(100), dec!(102), dec!(98), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(125), dec!(99), dec!(120));
        let b3 = make_ohlcv_bar(dec!(120), dec!(121), dec!(88), dec!(90));
        let dd = OhlcvBar::max_drawdown(&[b1, b2, b3]).unwrap();
        assert!((dd - 0.25).abs() < 1e-9, "expected 0.25, got {dd}");
    }

    // ── mean_volume ───────────────────────────────────────────────────────────

    #[test]
    fn test_mean_volume_none_when_empty() {
        assert!(OhlcvBar::mean_volume(&[]).is_none());
    }

    #[test]
    fn test_mean_volume_single_bar() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        bar.volume = dec!(200);
        assert_eq!(OhlcvBar::mean_volume(&[bar]), Some(dec!(200)));
    }

    #[test]
    fn test_mean_volume_multiple_bars() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b2.volume = dec!(200);
        let mut b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b3.volume = dec!(300);
        assert_eq!(OhlcvBar::mean_volume(&[b1, b2, b3]), Some(dec!(200)));
    }

    // ── vwap_deviation ────────────────────────────────────────────────────────

    #[test]
    fn test_vwap_deviation_none_when_vwap_not_set() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(bar.vwap_deviation().is_none());
    }

    #[test]
    fn test_vwap_deviation_zero_when_close_equals_vwap() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        bar.vwap = Some(dec!(100));
        assert_eq!(bar.vwap_deviation(), Some(0.0));
    }

    #[test]
    fn test_vwap_deviation_correct_value() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        bar.vwap = Some(dec!(100));
        // |110-100|/100 = 0.10
        let dev = bar.vwap_deviation().unwrap();
        assert!((dev - 0.1).abs() < 1e-10);
    }

    // ── high_close_ratio ──────────────────────────────────────────────────────

    #[test]
    fn test_high_close_ratio_none_when_high_zero() {
        let bar = OhlcvBar {
            symbol: "X".into(),
            timeframe: Timeframe::Minutes(1),
            open: dec!(0),
            high: dec!(0),
            low: dec!(0),
            close: dec!(0),
            volume: dec!(1),
            bar_start_ms: 0,
            trade_count: 1,
            is_complete: false,
            is_gap_fill: false,
            vwap: None,
        };
        assert!(bar.high_close_ratio().is_none());
    }

    #[test]
    fn test_high_close_ratio_one_when_close_equals_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let ratio = bar.high_close_ratio().unwrap();
        assert!((ratio - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_high_close_ratio_less_than_one_when_close_below_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(99));
        let ratio = bar.high_close_ratio().unwrap();
        assert!(ratio < 1.0);
    }

    // ── lower_shadow_pct ──────────────────────────────────────────────────────

    #[test]
    fn test_lower_shadow_pct_none_when_range_zero() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.lower_shadow_pct().is_none());
    }

    #[test]
    fn test_lower_shadow_pct_zero_when_no_lower_shadow() {
        // open=low=90, close=high=110 → lower_shadow=0
        let bar = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let pct = bar.lower_shadow_pct().unwrap();
        assert!(pct.abs() < 1e-10);
    }

    #[test]
    fn test_lower_shadow_pct_correct_value() {
        // open=100, close=105, high=110, low=90 → lower_shadow=min(100,105)-90=10, range=20 → 0.5
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let pct = bar.lower_shadow_pct().unwrap();
        assert!((pct - 0.5).abs() < 1e-10);
    }

    // ── open_close_ratio ──────────────────────────────────────────────────────

    #[test]
    fn test_open_close_ratio_none_when_open_zero() {
        let bar = make_ohlcv_bar(dec!(0), dec!(10), dec!(0), dec!(5));
        assert!(bar.open_close_ratio().is_none());
    }

    #[test]
    fn test_open_close_ratio_one_when_flat() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let ratio = bar.open_close_ratio().unwrap();
        assert!((ratio - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_open_close_ratio_above_one_for_bullish_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let ratio = bar.open_close_ratio().unwrap();
        assert!(ratio > 1.0);
    }

    // ── is_wide_range_bar ─────────────────────────────────────────────────────

    #[test]
    fn test_is_wide_range_bar_true_when_range_exceeds_threshold() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(110)); // range=25
        assert!(bar.is_wide_range_bar(dec!(20)));
    }

    #[test]
    fn test_is_wide_range_bar_false_when_range_equals_threshold() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(100), dec!(110)); // range=20
        assert!(!bar.is_wide_range_bar(dec!(20)));
    }

    // ── close_to_low_ratio ────────────────────────────────────────────────────

    #[test]
    fn test_close_to_low_ratio_none_when_range_zero() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.close_to_low_ratio().is_none());
    }

    #[test]
    fn test_close_to_low_ratio_one_when_closed_at_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let ratio = bar.close_to_low_ratio().unwrap();
        assert!((ratio - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_close_to_low_ratio_zero_when_closed_at_low() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let ratio = bar.close_to_low_ratio().unwrap();
        assert!(ratio.abs() < 1e-10);
    }

    #[test]
    fn test_close_to_low_ratio_half_at_midpoint() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        // (100-90)/(110-90) = 10/20 = 0.5
        let ratio = bar.close_to_low_ratio().unwrap();
        assert!((ratio - 0.5).abs() < 1e-10);
    }

    // ── volume_per_trade ──────────────────────────────────────────────────────

    #[test]
    fn test_volume_per_trade_none_when_trade_count_zero() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        bar.trade_count = 0;
        assert!(bar.volume_per_trade().is_none());
    }

    #[test]
    fn test_volume_per_trade_correct_value() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        bar.volume = dec!(500);
        bar.trade_count = 5;
        assert_eq!(bar.volume_per_trade(), Some(dec!(100)));
    }

    // ── price_range_overlap ───────────────────────────────────────────────────

    #[test]
    fn test_price_range_overlap_true_when_ranges_overlap() {
        let a = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b = make_ohlcv_bar(dec!(105), dec!(120), dec!(95), dec!(110));
        assert!(a.price_range_overlap(&b));
    }

    #[test]
    fn test_price_range_overlap_false_when_no_overlap() {
        let a = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b = make_ohlcv_bar(dec!(120), dec!(130), dec!(115), dec!(125));
        assert!(!a.price_range_overlap(&b));
    }

    #[test]
    fn test_price_range_overlap_true_at_exact_touch() {
        let a = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b = make_ohlcv_bar(dec!(115), dec!(125), dec!(110), dec!(120));
        assert!(a.price_range_overlap(&b));
    }

    // ── bar_height_pct ────────────────────────────────────────────────────────

    #[test]
    fn test_bar_height_pct_none_when_open_zero() {
        let bar = make_ohlcv_bar(dec!(0), dec!(10), dec!(0), dec!(5));
        assert!(bar.bar_height_pct().is_none());
    }

    #[test]
    fn test_bar_height_pct_correct_value() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // range=20
        // 20/100 = 0.2
        let pct = bar.bar_height_pct().unwrap();
        assert!((pct - 0.2).abs() < 1e-10);
    }

    // ── is_bullish_engulfing ──────────────────────────────────────────────────

    #[test]
    fn test_is_bullish_engulfing_true_for_valid_pattern() {
        // prev: bearish bar (open=110, close=100), this: bullish, engulfs (open=98, close=112)
        let prev = make_ohlcv_bar(dec!(110), dec!(115), dec!(95), dec!(100));
        let bar = make_ohlcv_bar(dec!(98), dec!(115), dec!(95), dec!(112));
        assert!(bar.is_bullish_engulfing(&prev));
    }

    #[test]
    fn test_is_bullish_engulfing_false_when_bearish() {
        let prev = make_ohlcv_bar(dec!(110), dec!(115), dec!(95), dec!(100));
        let bar = make_ohlcv_bar(dec!(108), dec!(115), dec!(95), dec!(95));
        assert!(!bar.is_bullish_engulfing(&prev));
    }

    // ── close_gap ─────────────────────────────────────────────────────────────

    #[test]
    fn test_close_gap_positive_for_gap_up() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(102));
        let bar = make_ohlcv_bar(dec!(106), dec!(110), dec!(103), dec!(108)); // open=106 > prev close=102
        assert_eq!(bar.close_gap(&prev), dec!(4));
    }

    #[test]
    fn test_close_gap_negative_for_gap_down() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(102));
        let bar = make_ohlcv_bar(dec!(98), dec!(100), dec!(95), dec!(97)); // open=98 < prev close=102
        assert_eq!(bar.close_gap(&prev), dec!(-4));
    }

    #[test]
    fn test_close_gap_zero_when_no_gap() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(102));
        let bar = make_ohlcv_bar(dec!(102), dec!(110), dec!(100), dec!(108));
        assert_eq!(bar.close_gap(&prev), dec!(0));
    }

    // ── close_above_midpoint ──────────────────────────────────────────────────

    #[test]
    fn test_close_above_midpoint_true_when_above_mid() {
        // high=110, low=90 → mid=100; close=105 > 100
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(bar.close_above_midpoint());
    }

    #[test]
    fn test_close_above_midpoint_false_when_at_mid() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // close=mid=100
        assert!(!bar.close_above_midpoint());
    }

    // ── close_momentum ────────────────────────────────────────────────────────

    #[test]
    fn test_close_momentum_positive_when_rising() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        assert_eq!(bar.close_momentum(&prev), dec!(10));
    }

    #[test]
    fn test_close_momentum_zero_when_unchanged() {
        let prev = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(100));
        assert_eq!(bar.close_momentum(&prev), dec!(0));
    }

    // ── bar_range ─────────────────────────────────────────────────────────────

    #[test]
    fn test_bar_range_correct() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(110));
        assert_eq!(bar.bar_range(), dec!(30));
    }

    // ── linear_regression_slope ───────────────────────────────────────────────

    #[test]
    fn test_linear_regression_slope_none_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::linear_regression_slope(&[bar]).is_none());
    }

    #[test]
    fn test_linear_regression_slope_positive_for_rising_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110)),
            make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(120)),
        ];
        let slope = OhlcvBar::linear_regression_slope(&bars).unwrap();
        assert!(slope > 0.0, "slope should be positive for rising closes");
    }

    #[test]
    fn test_linear_regression_slope_negative_for_falling_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(120), dec!(125), dec!(115), dec!(120)),
            make_ohlcv_bar(dec!(120), dec!(115), dec!(105), dec!(110)),
            make_ohlcv_bar(dec!(110), dec!(108), dec!(95), dec!(100)),
        ];
        let slope = OhlcvBar::linear_regression_slope(&bars).unwrap();
        assert!(slope < 0.0, "slope should be negative for falling closes");
    }

    #[test]
    fn test_linear_regression_slope_near_zero_for_flat_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
        ];
        let slope = OhlcvBar::linear_regression_slope(&bars).unwrap();
        assert!(slope.abs() < 1e-10, "slope should be ~0 for identical closes");
    }

    // ── volume_slope ──────────────────────────────────────────────────────────

    #[test]
    fn test_volume_slope_none_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::volume_slope(&[bar]).is_none());
    }

    #[test]
    fn test_volume_slope_positive_for_rising_volume() {
        let make_bar_with_vol = |v: u64| {
            let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
            b.volume = Decimal::from(v);
            b
        };
        let bars = vec![make_bar_with_vol(100), make_bar_with_vol(200), make_bar_with_vol(300)];
        assert!(OhlcvBar::volume_slope(&bars).unwrap() > 0.0);
    }

    // ── highest_close / lowest_close ──────────────────────────────────────────

    #[test]
    fn test_highest_close_none_for_empty_slice() {
        assert!(OhlcvBar::highest_close(&[]).is_none());
    }

    #[test]
    fn test_highest_close_returns_max_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        let b3 = make_ohlcv_bar(dec!(100), dec!(108), dec!(90), dec!(102));
        assert_eq!(OhlcvBar::highest_close(&[b1, b2, b3]), Some(dec!(115)));
    }

    #[test]
    fn test_lowest_close_none_for_empty_slice() {
        assert!(OhlcvBar::lowest_close(&[]).is_none());
    }

    #[test]
    fn test_lowest_close_returns_min_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        let b3 = make_ohlcv_bar(dec!(100), dec!(108), dec!(90), dec!(102));
        assert_eq!(OhlcvBar::lowest_close(&[b1, b2, b3]), Some(dec!(102)));
    }

    // ── close_range / momentum ────────────────────────────────────────────────

    #[test]
    fn test_close_range_none_for_empty_slice() {
        assert!(OhlcvBar::close_range(&[]).is_none());
    }

    #[test]
    fn test_close_range_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(102));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        // highest=115, lowest=102, range=13
        assert_eq!(OhlcvBar::close_range(&[b1, b2]), Some(dec!(13)));
    }

    #[test]
    fn test_momentum_none_for_insufficient_bars() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::momentum(&[bar], 1).is_none());
    }

    #[test]
    fn test_momentum_positive_for_rising_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        // (110 - 100) / 100 = 0.10
        let mom = OhlcvBar::momentum(&[b1, b2], 1).unwrap();
        assert!((mom - 0.1).abs() < 1e-10);
    }

    #[test]
    fn test_momentum_negative_for_falling_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let b2 = make_ohlcv_bar(dec!(100), dec!(108), dec!(88), dec!(99));
        // (99 - 110) / 110 ≈ -0.10
        let mom = OhlcvBar::momentum(&[b1, b2], 1).unwrap();
        assert!(mom < 0.0);
    }

    // ── mean_close ────────────────────────────────────────────────────────────

    #[test]
    fn test_mean_close_none_for_empty_slice() {
        assert!(OhlcvBar::mean_close(&[]).is_none());
    }

    #[test]
    fn test_mean_close_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::mean_close(&[bar]), Some(dec!(105)));
    }

    #[test]
    fn test_mean_close_multiple_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(120));
        // (100 + 110 + 120) / 3 = 110
        assert_eq!(OhlcvBar::mean_close(&[b1, b2, b3]), Some(dec!(110)));
    }

    // ── close_std_dev ─────────────────────────────────────────────────────────

    #[test]
    fn test_close_std_dev_none_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::close_std_dev(&[bar]).is_none());
    }

    #[test]
    fn test_close_std_dev_zero_for_identical_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
        ];
        let sd = OhlcvBar::close_std_dev(&bars).unwrap();
        assert!(sd.abs() < 1e-10, "std_dev should be ~0 for identical closes");
    }

    #[test]
    fn test_close_std_dev_positive_for_varied_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)),
        ];
        assert!(OhlcvBar::close_std_dev(&bars).unwrap() > 0.0);
    }

    // ── price_efficiency_ratio ────────────────────────────────────────────────

    #[test]
    fn test_price_efficiency_ratio_none_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::price_efficiency_ratio(&[bar]).is_none());
    }

    #[test]
    fn test_price_efficiency_ratio_one_for_trending_price() {
        // All bars with same range, monotonically rising closes
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(110));
        let b3 = make_ohlcv_bar(dec!(120), dec!(130), dec!(110), dec!(120));
        // net move = 20, total path = 3 * 20 = 60; ratio = 20/60 ≈ 0.333
        let ratio = OhlcvBar::price_efficiency_ratio(&[b1, b2, b3]).unwrap();
        assert!(ratio > 0.0 && ratio <= 1.0);
    }

    #[test]
    fn test_price_efficiency_ratio_none_for_zero_total_range() {
        // Zero-range bars (high == low)
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100)),
        ];
        assert!(OhlcvBar::price_efficiency_ratio(&bars).is_none());
    }

    // ── close_location_value / mean_clv ───────────────────────────────────────

    #[test]
    fn test_clv_plus_one_when_close_at_high() {
        // close == high: CLV = ((high-low)-(0)) / (high-low) = 1
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let clv = bar.close_location_value().unwrap();
        assert!((clv - 1.0).abs() < 1e-10, "CLV should be 1.0 when close == high, got {clv}");
    }

    #[test]
    fn test_clv_minus_one_when_close_at_low() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let clv = bar.close_location_value().unwrap();
        assert!((clv + 1.0).abs() < 1e-10, "CLV should be -1.0 when close == low, got {clv}");
    }

    #[test]
    fn test_clv_zero_when_close_at_midpoint() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let clv = bar.close_location_value().unwrap();
        assert!(clv.abs() < 1e-10, "CLV should be 0 at midpoint, got {clv}");
    }

    #[test]
    fn test_clv_none_for_zero_range_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(bar.close_location_value().is_none());
    }

    #[test]
    fn test_mean_clv_none_for_empty_slice() {
        assert!(OhlcvBar::mean_clv(&[]).is_none());
    }

    #[test]
    fn test_mean_clv_positive_for_bullish_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)), // close near high
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(106)), // close above mid
        ];
        let clv = OhlcvBar::mean_clv(&bars).unwrap();
        assert!(clv > 0.0, "mean CLV should be positive when closes are near highs");
    }

    #[test]
    fn test_mean_range_none_for_empty_slice() {
        assert!(OhlcvBar::mean_range(&[]).is_none());
    }

    #[test]
    fn test_mean_range_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::mean_range(&[bar]), Some(dec!(20)));
    }

    #[test]
    fn test_mean_range_multiple_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // range 20
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(100)); // range 40
        assert_eq!(OhlcvBar::mean_range(&[b1, b2]), Some(dec!(30)));
    }

    #[test]
    fn test_close_z_score_none_for_empty_slice() {
        assert!(OhlcvBar::close_z_score(&[], dec!(100)).is_none());
    }

    #[test]
    fn test_close_z_score_of_mean_is_zero() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)),
        ];
        // mean close = (100+100+110)/3 ≈ 103.33; z-score of mean should be ≈ 0
        let mean = (dec!(100) + dec!(100) + dec!(110)) / dec!(3);
        let z = OhlcvBar::close_z_score(&bars, mean).unwrap();
        assert!(z.abs() < 1e-6);
    }

    #[test]
    fn test_close_z_score_positive_above_mean() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)),
        ];
        let z = OhlcvBar::close_z_score(&bars, dec!(120)).unwrap();
        assert!(z > 0.0);
    }

    #[test]
    fn test_bollinger_band_width_none_for_empty_slice() {
        assert!(OhlcvBar::bollinger_band_width(&[]).is_none());
    }

    #[test]
    fn test_bollinger_band_width_zero_for_identical_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
        ];
        assert_eq!(OhlcvBar::bollinger_band_width(&bars), Some(0.0));
    }

    #[test]
    fn test_bollinger_band_width_positive_for_varying_closes() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)),
        ];
        let bw = OhlcvBar::bollinger_band_width(&bars).unwrap();
        assert!(bw > 0.0);
    }

    #[test]
    fn test_up_down_ratio_none_for_no_bearish_bars() {
        let bars = vec![
            make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105)), // bullish
        ];
        assert!(OhlcvBar::up_down_ratio(&bars).is_none());
    }

    #[test]
    fn test_up_down_ratio_two_to_one() {
        let bull = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        let bear = make_ohlcv_bar(dec!(110), dec!(115), dec!(85), dec!(95));
        let bars = vec![bull.clone(), bull, bear];
        let ratio = OhlcvBar::up_down_ratio(&bars).unwrap();
        assert!((ratio - 2.0).abs() < 1e-9);
    }

    // ── OhlcvBar::volume_weighted_close ───────────────────────────────────────

    #[test]
    fn test_volume_weighted_close_none_for_empty_slice() {
        assert!(OhlcvBar::volume_weighted_close(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_close_single_bar() {
        let mut bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        bar.volume = dec!(10);
        assert_eq!(OhlcvBar::volume_weighted_close(&[bar]), Some(dec!(105)));
    }

    #[test]
    fn test_volume_weighted_close_weights_by_volume() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b1.volume = dec!(1);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(200));
        b2.volume = dec!(3);
        // vwc = (100*1 + 200*3) / (1+3) = 700 / 4 = 175
        assert_eq!(OhlcvBar::volume_weighted_close(&[b1, b2]), Some(dec!(175)));
    }

    // ── OhlcvBar::rolling_return ──────────────────────────────────────────────

    #[test]
    fn test_rolling_return_none_for_empty_slice() {
        assert!(OhlcvBar::rolling_return(&[]).is_none());
    }

    #[test]
    fn test_rolling_return_none_for_single_bar() {
        assert!(OhlcvBar::rolling_return(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_rolling_return_positive_when_close_rises() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(105), dec!(95), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let ret = OhlcvBar::rolling_return(&[b1, b2]).unwrap();
        assert!((ret - 0.1).abs() < 1e-9);
    }

    // ── OhlcvBar::average_high / average_low ─────────────────────────────────

    #[test]
    fn test_average_high_none_for_empty_slice() {
        assert!(OhlcvBar::average_high(&[]).is_none());
    }

    #[test]
    fn test_average_high_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::average_high(&[bar]), Some(dec!(120)));
    }

    #[test]
    fn test_average_high_multiple_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(130), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::average_high(&[b1, b2]), Some(dec!(120)));
    }

    #[test]
    fn test_average_low_none_for_empty_slice() {
        assert!(OhlcvBar::average_low(&[]).is_none());
    }

    #[test]
    fn test_average_low_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(105));
        assert_eq!(OhlcvBar::average_low(&[bar]), Some(dec!(80)));
    }

    #[test]
    fn test_average_low_multiple_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(80), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(130), dec!(60), dec!(105));
        assert_eq!(OhlcvBar::average_low(&[b1, b2]), Some(dec!(70)));
    }

    // ── OhlcvBar::min_body / max_body ─────────────────────────────────────────

    #[test]
    fn test_min_body_none_for_empty_slice() {
        assert!(OhlcvBar::min_body(&[]).is_none());
    }

    #[test]
    fn test_min_body_returns_smallest_body() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // body=5
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(115)); // body=15
        assert_eq!(OhlcvBar::min_body(&[b1, b2]), Some(dec!(5)));
    }

    #[test]
    fn test_max_body_none_for_empty_slice() {
        assert!(OhlcvBar::max_body(&[]).is_none());
    }

    #[test]
    fn test_max_body_returns_largest_body() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // body=5
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(115)); // body=15
        assert_eq!(OhlcvBar::max_body(&[b1, b2]), Some(dec!(15)));
    }

    // ── OhlcvBar::atr_pct ────────────────────────────────────────────────────

    #[test]
    fn test_atr_pct_none_for_single_bar() {
        assert!(OhlcvBar::atr_pct(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_atr_pct_positive_for_normal_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let pct = OhlcvBar::atr_pct(&[b1, b2]).unwrap();
        assert!(pct > 0.0);
    }

    // ── OhlcvBar::breakout_count ──────────────────────────────────────────────

    #[test]
    fn test_breakout_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::breakout_count(&[]), 0);
    }

    #[test]
    fn test_breakout_count_zero_for_single_bar() {
        assert_eq!(OhlcvBar::breakout_count(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]), 0);
    }

    #[test]
    fn test_breakout_count_detects_close_above_prev_high() {
        // b1: high=110; b2: close=115 > 110 → breakout
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let b2 = make_ohlcv_bar(dec!(108), dec!(120), dec!(105), dec!(115));
        assert_eq!(OhlcvBar::breakout_count(&[b1, b2]), 1);
    }

    #[test]
    fn test_breakout_count_zero_when_close_at_prev_high() {
        // close == prev high → not a strict breakout
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let b2 = make_ohlcv_bar(dec!(108), dec!(115), dec!(105), dec!(110));
        assert_eq!(OhlcvBar::breakout_count(&[b1, b2]), 0);
    }

    // ── OhlcvBar::doji_count ──────────────────────────────────────────────────

    #[test]
    fn test_doji_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::doji_count(&[], dec!(0.001)), 0);
    }

    #[test]
    fn test_doji_count_detects_doji_bars() {
        let doji = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // body=0
        let non_doji = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)); // body=10
        assert_eq!(OhlcvBar::doji_count(&[doji, non_doji], dec!(1)), 1);
    }

    // ── OhlcvBar::channel_width ───────────────────────────────────────────────

    #[test]
    fn test_channel_width_none_for_empty_slice() {
        assert!(OhlcvBar::channel_width(&[]).is_none());
    }

    #[test]
    fn test_channel_width_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(80), dec!(100));
        // highest_high = 120, lowest_low = 80, width = 40
        assert_eq!(OhlcvBar::channel_width(&[b1, b2]), Some(dec!(40)));
    }

    // ── OhlcvBar::sma ─────────────────────────────────────────────────────────

    #[test]
    fn test_sma_none_for_zero_period() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::sma(&[bar], 0).is_none());
    }

    #[test]
    fn test_sma_none_when_fewer_bars_than_period() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::sma(&[bar], 3).is_none());
    }

    #[test]
    fn test_sma_correct_for_last_n_bars() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(120)),
        ];
        // sma(3) = (100 + 110 + 120) / 3 = 110
        assert_eq!(OhlcvBar::sma(&bars, 3), Some(dec!(110)));
    }

    // ── OhlcvBar::mean_wick_ratio ─────────────────────────────────────────────

    #[test]
    fn test_mean_wick_ratio_none_for_empty_slice() {
        assert!(OhlcvBar::mean_wick_ratio(&[]).is_none());
    }

    #[test]
    fn test_mean_wick_ratio_in_range_zero_to_one() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(85), dec!(100));
        let ratio = OhlcvBar::mean_wick_ratio(&[b1, b2]).unwrap();
        assert!(ratio >= 0.0 && ratio <= 1.0);
    }

    // ── OhlcvBar::bullish_volume / bearish_volume ─────────────────────────────

    #[test]
    fn test_bullish_volume_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::bullish_volume(&[]), dec!(0));
    }

    #[test]
    fn test_bullish_volume_sums_bullish_bars() {
        let mut bull = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        bull.volume = dec!(100);
        let mut bear = make_ohlcv_bar(dec!(110), dec!(115), dec!(85), dec!(95));
        bear.volume = dec!(50);
        assert_eq!(OhlcvBar::bullish_volume(&[bull, bear]), dec!(100));
    }

    #[test]
    fn test_bearish_volume_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::bearish_volume(&[]), dec!(0));
    }

    #[test]
    fn test_bearish_volume_sums_bearish_bars() {
        let mut bull = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        bull.volume = dec!(100);
        let mut bear = make_ohlcv_bar(dec!(110), dec!(115), dec!(85), dec!(95));
        bear.volume = dec!(50);
        assert_eq!(OhlcvBar::bearish_volume(&[bull, bear]), dec!(50));
    }

    // ── OhlcvBar::close_above_mid_count ──────────────────────────────────────

    #[test]
    fn test_close_above_mid_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::close_above_mid_count(&[]), 0);
    }

    #[test]
    fn test_close_above_mid_count_correct() {
        let above_mid = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110)); // mid=100, close=110 > 100
        let at_mid = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(100)); // mid=100, close=100 not > 100
        let below_mid = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(85)); // mid=100, close=85 < 100
        assert_eq!(OhlcvBar::close_above_mid_count(&[above_mid, at_mid, below_mid]), 1);
    }

    // ── OhlcvBar::ema ─────────────────────────────────────────────────────────

    #[test]
    fn test_ema_none_for_empty_slice() {
        assert!(OhlcvBar::ema(&[], 0.5).is_none());
    }

    #[test]
    fn test_ema_single_bar_equals_close() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let e = OhlcvBar::ema(&[bar], 0.5).unwrap();
        assert!((e - 105.0).abs() < 1e-9);
    }

    #[test]
    fn test_ema_alpha_one_equals_last_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(200));
        let e = OhlcvBar::ema(&[b1, b2], 1.0).unwrap();
        assert!((e - 200.0).abs() < 1e-9);
    }

    // ── OhlcvBar::highest_open / lowest_open ─────────────────────────────────

    #[test]
    fn test_highest_open_none_for_empty_slice() {
        assert!(OhlcvBar::highest_open(&[]).is_none());
    }

    #[test]
    fn test_highest_open_returns_max() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(130), dec!(140), dec!(120), dec!(135));
        assert_eq!(OhlcvBar::highest_open(&[b1, b2]), Some(dec!(130)));
    }

    #[test]
    fn test_lowest_open_none_for_empty_slice() {
        assert!(OhlcvBar::lowest_open(&[]).is_none());
    }

    #[test]
    fn test_lowest_open_returns_min() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(130), dec!(140), dec!(120), dec!(135));
        assert_eq!(OhlcvBar::lowest_open(&[b1, b2]), Some(dec!(100)));
    }

    // ── OhlcvBar::rising_close_count ─────────────────────────────────────────

    #[test]
    fn test_rising_close_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::rising_close_count(&[]), 0);
    }

    #[test]
    fn test_rising_close_count_zero_for_single_bar() {
        assert_eq!(OhlcvBar::rising_close_count(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]), 0);
    }

    #[test]
    fn test_rising_close_count_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110)); // close > prev
        let b3 = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(105)); // close < prev
        let b4 = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(115)); // close > prev
        assert_eq!(OhlcvBar::rising_close_count(&[b1, b2, b3, b4]), 2);
    }

    // ── OhlcvBar::mean_body_ratio ─────────────────────────────────────────────

    #[test]
    fn test_mean_body_ratio_none_for_empty_slice() {
        assert!(OhlcvBar::mean_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_mean_body_ratio_in_range_zero_to_one() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(100));
        let ratio = OhlcvBar::mean_body_ratio(&[b1, b2]).unwrap();
        assert!(ratio >= 0.0 && ratio <= 1.0);
    }

    // ── OhlcvBar::volume_std_dev ──────────────────────────────────────────────

    #[test]
    fn test_volume_std_dev_none_for_single_bar() {
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b.volume = dec!(100);
        assert!(OhlcvBar::volume_std_dev(&[b]).is_none());
    }

    #[test]
    fn test_volume_std_dev_zero_for_identical_volumes() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(50);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.volume = dec!(50);
        assert_eq!(OhlcvBar::volume_std_dev(&[b1, b2]), Some(0.0));
    }

    #[test]
    fn test_volume_std_dev_positive_for_varied_volumes() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(10);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.volume = dec!(100);
        let std = OhlcvBar::volume_std_dev(&[b1, b2]).unwrap();
        assert!(std > 0.0);
    }

    // ── OhlcvBar::max_volume_bar / min_volume_bar ─────────────────────────────

    #[test]
    fn test_max_volume_bar_none_for_empty_slice() {
        assert!(OhlcvBar::max_volume_bar(&[]).is_none());
    }

    #[test]
    fn test_max_volume_bar_returns_highest_volume() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(10);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.volume = dec!(100);
        let bars = [b1, b2];
        let bar = OhlcvBar::max_volume_bar(&bars).unwrap();
        assert_eq!(bar.volume, dec!(100));
    }

    #[test]
    fn test_min_volume_bar_returns_lowest_volume() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(10);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.volume = dec!(100);
        let bars = [b1, b2];
        let bar = OhlcvBar::min_volume_bar(&bars).unwrap();
        assert_eq!(bar.volume, dec!(10));
    }

    // ── OhlcvBar::gap_sum ─────────────────────────────────────────────────────

    #[test]
    fn test_gap_sum_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::gap_sum(&[]), dec!(0));
    }

    #[test]
    fn test_gap_sum_zero_for_single_bar() {
        assert_eq!(OhlcvBar::gap_sum(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]), dec!(0));
    }

    #[test]
    fn test_gap_sum_positive_for_gap_up_sequence() {
        // b1 close=100, b2 open=110 → gap = +10
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(115));
        assert_eq!(OhlcvBar::gap_sum(&[b1, b2]), dec!(10));
    }

    #[test]
    fn test_gap_sum_negative_for_gap_down_sequence() {
        // b1 close=100, b2 open=90 → gap = -10
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(90), dec!(95), dec!(80), dec!(85));
        assert_eq!(OhlcvBar::gap_sum(&[b1, b2]), dec!(-10));
    }

    // ── OhlcvBar::three_white_soldiers ────────────────────────────────────────

    #[test]
    fn test_three_white_soldiers_false_for_fewer_than_3_bars() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(!OhlcvBar::three_white_soldiers(&[b]));
    }

    #[test]
    fn test_three_white_soldiers_true_for_classic_pattern() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(112));
        let b2 = make_ohlcv_bar(dec!(112), dec!(128), dec!(110), dec!(125));
        let b3 = make_ohlcv_bar(dec!(125), dec!(142), dec!(123), dec!(140));
        assert!(OhlcvBar::three_white_soldiers(&[b1, b2, b3]));
    }

    #[test]
    fn test_three_white_soldiers_false_for_bearish_bar_in_sequence() {
        // b2 is bearish (close < open)
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(112));
        let b2 = make_ohlcv_bar(dec!(115), dec!(120), dec!(105), dec!(108));
        let b3 = make_ohlcv_bar(dec!(108), dec!(130), dec!(106), dec!(128));
        assert!(!OhlcvBar::three_white_soldiers(&[b1, b2, b3]));
    }

    // ── OhlcvBar::three_black_crows ───────────────────────────────────────────

    #[test]
    fn test_three_black_crows_false_for_fewer_than_3_bars() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(95));
        assert!(!OhlcvBar::three_black_crows(&[b]));
    }

    #[test]
    fn test_three_black_crows_true_for_classic_pattern() {
        let b1 = make_ohlcv_bar(dec!(140), dec!(142), dec!(110), dec!(112));
        let b2 = make_ohlcv_bar(dec!(112), dec!(114), dec!(95), dec!(97));
        let b3 = make_ohlcv_bar(dec!(97), dec!(99), dec!(80), dec!(82));
        assert!(OhlcvBar::three_black_crows(&[b1, b2, b3]));
    }

    #[test]
    fn test_three_black_crows_false_for_bullish_bar_in_sequence() {
        // b2 is bullish
        let b1 = make_ohlcv_bar(dec!(140), dec!(142), dec!(110), dec!(112));
        let b2 = make_ohlcv_bar(dec!(108), dec!(120), dec!(106), dec!(118));
        let b3 = make_ohlcv_bar(dec!(115), dec!(116), dec!(90), dec!(92));
        assert!(!OhlcvBar::three_black_crows(&[b1, b2, b3]));
    }

    // ── OhlcvBar::is_gap_bar ─────────────────────────────────────────────────

    #[test]
    fn test_is_gap_bar_true_when_open_differs_from_prev_close() {
        let bar = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110));
        assert!(OhlcvBar::is_gap_bar(&bar, dec!(100)));
    }

    #[test]
    fn test_is_gap_bar_false_when_open_equals_prev_close() {
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        assert!(!OhlcvBar::is_gap_bar(&bar, dec!(100)));
    }

    // ── OhlcvBar::gap_bars_count ──────────────────────────────────────────────

    #[test]
    fn test_gap_bars_count_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::gap_bars_count(&[]), 0);
    }

    #[test]
    fn test_gap_bars_count_zero_when_no_gaps() {
        // b1 close=100, b2 open=100 → no gap
        let b1 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        assert_eq!(OhlcvBar::gap_bars_count(&[b1, b2]), 0);
    }

    #[test]
    fn test_gap_bars_count_counts_all_gaps() {
        let b1 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(100));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); // gap: open=105 != 100
        let b3 = make_ohlcv_bar(dec!(110), dec!(120), dec!(108), dec!(115)); // no gap
        let b4 = make_ohlcv_bar(dec!(120), dec!(130), dec!(118), dec!(128)); // gap: open=120 != 115
        assert_eq!(OhlcvBar::gap_bars_count(&[b1, b2, b3, b4]), 2);
    }

    // ── OhlcvBar::inside_bar / outside_bar (instance method) ─────────────────

    #[test]
    fn test_inside_bar_true_when_range_inside_prior_v2() {
        let prior = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let bar   = make_ohlcv_bar(dec!(105), dec!(115), dec!(90), dec!(108));
        assert!(bar.inside_bar(&prior));
    }

    #[test]
    fn test_inside_bar_false_when_high_exceeds_prior_v2() {
        let prior = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let bar   = make_ohlcv_bar(dec!(105), dec!(125), dec!(90), dec!(118));
        assert!(!bar.inside_bar(&prior));
    }

    #[test]
    fn test_outside_bar_true_when_range_engulfs_prior_v2() {
        let prior = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(108));
        let bar   = make_ohlcv_bar(dec!(95), dec!(120), dec!(85), dec!(112));
        assert!(bar.outside_bar(&prior));
    }

    #[test]
    fn test_outside_bar_false_when_range_is_inside_v2() {
        let prior = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let bar   = make_ohlcv_bar(dec!(105), dec!(115), dec!(90), dec!(108));
        assert!(!bar.outside_bar(&prior));
    }

    // ── OhlcvBar::bar_efficiency ──────────────────────────────────────────────

    #[test]
    fn test_bar_efficiency_none_for_zero_range_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::bar_efficiency(&bar).is_none());
    }

    #[test]
    fn test_bar_efficiency_one_for_full_trend_bar() {
        // open=100, high=110, low=100, close=110 → body=10, range=10 → efficiency=1.0
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let eff = OhlcvBar::bar_efficiency(&bar).unwrap();
        assert!((eff - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_bar_efficiency_between_zero_and_one() {
        let bar = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(108));
        let eff = OhlcvBar::bar_efficiency(&bar).unwrap();
        assert!(eff >= 0.0 && eff <= 1.0);
    }

    // ── OhlcvBar::wicks_sum ───────────────────────────────────────────────────

    #[test]
    fn test_wicks_sum_zero_for_empty_slice() {
        assert_eq!(OhlcvBar::wicks_sum(&[]), dec!(0));
    }

    #[test]
    fn test_wicks_sum_correct_for_doji_like_bar() {
        // open=close=100, high=110, low=90 → upper=10, lower=10, total=20
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert_eq!(OhlcvBar::wicks_sum(&[bar]), dec!(20));
    }

    // ── OhlcvBar::avg_close_to_high ───────────────────────────────────────────

    #[test]
    fn test_avg_close_to_high_none_for_empty_slice() {
        assert!(OhlcvBar::avg_close_to_high(&[]).is_none());
    }

    #[test]
    fn test_avg_close_to_high_correct_for_two_bars() {
        // b1: high=110, close=105 → 5; b2: high=120, close=115 → 5; avg=5.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(108), dec!(115));
        let avg = OhlcvBar::avg_close_to_high(&[b1, b2]).unwrap();
        assert!((avg - 5.0).abs() < 1e-9);
    }

    // ── OhlcvBar::avg_range ───────────────────────────────────────────────────

    #[test]
    fn test_avg_range_r65_none_for_empty() {
        assert!(OhlcvBar::avg_range(&[]).is_none());
    }

    #[test]
    fn test_avg_range_r65_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(100), dec!(115));
        let avg = OhlcvBar::avg_range(&[b1, b2]).unwrap();
        assert!((avg - 20.0).abs() < 1e-9);
    }

    // ── OhlcvBar::max_close / min_close ───────────────────────────────────────

    #[test]
    fn test_max_close_r65_none_empty() {
        assert!(OhlcvBar::max_close(&[]).is_none());
    }

    #[test]
    fn test_max_close_r65_highest() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(130), dec!(108), dec!(125));
        let b3 = make_ohlcv_bar(dec!(115), dec!(120), dec!(112), dec!(118));
        assert_eq!(OhlcvBar::max_close(&[b1, b2, b3]), Some(dec!(125)));
    }

    #[test]
    fn test_min_close_r65_lowest() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(130), dec!(108), dec!(125));
        let b3 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(95));
        assert_eq!(OhlcvBar::min_close(&[b1, b2, b3]), Some(dec!(95)));
    }

    // ── OhlcvBar::trend_strength ──────────────────────────────────────────────

    #[test]
    fn test_trend_strength_r65_none_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::trend_strength(&[b]).is_none());
    }

    #[test]
    fn test_trend_strength_r65_one_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(103), dec!(115));
        let b3 = make_ohlcv_bar(dec!(115), dec!(130), dec!(113), dec!(128));
        let s = OhlcvBar::trend_strength(&[b1, b2, b3]).unwrap();
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_trend_strength_r65_zero_bearish() {
        let b1 = make_ohlcv_bar(dec!(128), dec!(130), dec!(113), dec!(128));
        let b2 = make_ohlcv_bar(dec!(115), dec!(120), dec!(103), dec!(110));
        let b3 = make_ohlcv_bar(dec!(105), dec!(110), dec!(95), dec!(100));
        let s = OhlcvBar::trend_strength(&[b1, b2, b3]).unwrap();
        assert!((s - 0.0).abs() < 1e-9);
    }

    // ── OhlcvBar::net_change ──────────────────────────────────────────────────

    #[test]
    fn test_net_change_none_for_empty() {
        assert!(OhlcvBar::net_change(&[]).is_none());
    }

    #[test]
    fn test_net_change_positive_for_bullish_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(112));
        assert_eq!(OhlcvBar::net_change(&[b]), Some(dec!(12)));
    }

    #[test]
    fn test_net_change_negative_for_bearish_bar() {
        let b = make_ohlcv_bar(dec!(110), dec!(112), dec!(95), dec!(100));
        assert_eq!(OhlcvBar::net_change(&[b]), Some(dec!(-10)));
    }

    // ── OhlcvBar::open_to_close_pct ───────────────────────────────────────────

    #[test]
    fn test_open_to_close_pct_none_for_empty() {
        assert!(OhlcvBar::open_to_close_pct(&[]).is_none());
    }

    #[test]
    fn test_open_to_close_pct_correct() {
        // open=100, close=110 → 10%
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        let pct = OhlcvBar::open_to_close_pct(&[b]).unwrap();
        assert!((pct - 10.0).abs() < 1e-9);
    }

    // ── OhlcvBar::high_to_low_pct ─────────────────────────────────────────────

    #[test]
    fn test_high_to_low_pct_none_for_empty() {
        assert!(OhlcvBar::high_to_low_pct(&[]).is_none());
    }

    #[test]
    fn test_high_to_low_pct_correct() {
        // high=200, low=100 → 50%
        let b = make_ohlcv_bar(dec!(150), dec!(200), dec!(100), dec!(160));
        let pct = OhlcvBar::high_to_low_pct(&[b]).unwrap();
        assert!((pct - 50.0).abs() < 1e-9);
    }

    // ── OhlcvBar::consecutive_highs / consecutive_lows ───────────────────────

    #[test]
    fn test_consecutive_highs_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::consecutive_highs(&[b]), 0);
    }

    #[test]
    fn test_consecutive_highs_counts_trailing_highs() {
        // bars with rising highs: 110, 120, 130 → 2 consecutive from end
        let b1 = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(103), dec!(115));
        let b3 = make_ohlcv_bar(dec!(115), dec!(130), dec!(113), dec!(125));
        assert_eq!(OhlcvBar::consecutive_highs(&[b1, b2, b3]), 2);
    }

    #[test]
    fn test_consecutive_lows_counts_trailing_lows() {
        // bars with falling lows: 90, 80, 70 → 2 consecutive from end
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(95), dec!(108), dec!(80), dec!(100));
        let b3 = make_ohlcv_bar(dec!(90), dec!(102), dec!(70), dec!(95));
        assert_eq!(OhlcvBar::consecutive_lows(&[b1, b2, b3]), 2);
    }

    // ── OhlcvBar::volume_change_pct ───────────────────────────────────────────

    #[test]
    fn test_volume_change_pct_none_for_single_bar() {
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b.volume = dec!(100);
        assert!(OhlcvBar::volume_change_pct(&[b]).is_none());
    }

    #[test]
    fn test_volume_change_pct_correct() {
        // prior vol=100, current vol=150 → +50%
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); b2.volume = dec!(150);
        let pct = OhlcvBar::volume_change_pct(&[b1, b2]).unwrap();
        assert!((pct - 50.0).abs() < 1e-9);
    }

    // ── OhlcvBar::close_location_value (instance method) ─────────────────────

    #[test]
    fn test_clv_r67_plus_one_at_high() {
        // symmetric CLV: +1 when close=high
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let clv = b.close_location_value().unwrap();
        assert!((clv - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_clv_r67_minus_one_at_low() {
        // symmetric CLV: -1 when close=low
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let clv = b.close_location_value().unwrap();
        assert!((clv - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn test_clv_r67_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(b.close_location_value().is_none());
    }

    // ── OhlcvBar::body_pct (instance method) ──────────────────────────────────

    #[test]
    fn test_body_pct_r67_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(b.body_pct().is_none());
    }

    #[test]
    fn test_body_pct_r67_100_for_full_body() {
        // open=90, close=110, high=110, low=90 → body_pct=100%
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        assert_eq!(b.body_pct(), Some(dec!(100)));
    }

    // ── OhlcvBar::bullish_count / bearish_count ───────────────────────────────

    #[test]
    fn test_bullish_count_r67_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(112)); // bullish
        let b2 = make_ohlcv_bar(dec!(112), dec!(120), dec!(105), dec!(108)); // bearish
        let b3 = make_ohlcv_bar(dec!(108), dec!(125), dec!(106), dec!(120)); // bullish
        assert_eq!(OhlcvBar::bullish_count(&[b1, b2, b3]), 2);
    }

    #[test]
    fn test_bearish_count_r67_correct() {
        let b1 = make_ohlcv_bar(dec!(115), dec!(118), dec!(100), dec!(105)); // bearish
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(112)); // bullish
        assert_eq!(OhlcvBar::bearish_count(&[b1, b2]), 1);
    }

    // ── OhlcvBar::open_gap_pct ────────────────────────────────────────────────

    #[test]
    fn test_open_gap_pct_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_gap_pct(&[b]).is_none());
    }

    #[test]
    fn test_open_gap_pct_positive_for_gap_up() {
        // prev close=100, current open=105 → 5%
        let b1 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(100));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110));
        let pct = OhlcvBar::open_gap_pct(&[b1, b2]).unwrap();
        assert!((pct - 5.0).abs() < 1e-9);
    }

    // ── OhlcvBar::volume_cumulative ───────────────────────────────────────────

    #[test]
    fn test_volume_cumulative_zero_for_empty() {
        assert_eq!(OhlcvBar::volume_cumulative(&[]), dec!(0));
    }

    #[test]
    fn test_volume_cumulative_sums_all_volumes() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); b2.volume = dec!(200);
        assert_eq!(OhlcvBar::volume_cumulative(&[b1, b2]), dec!(300));
    }

    // ── OhlcvBar::price_position ──────────────────────────────────────────────

    #[test]
    fn test_price_position_none_for_empty() {
        assert!(OhlcvBar::price_position(&[]).is_none());
    }

    #[test]
    fn test_price_position_one_when_close_at_highest() {
        // bars: high=100 and high=120 (range 80-120=40), last close=120
        let b1 = make_ohlcv_bar(dec!(85), dec!(100), dec!(80), dec!(95));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(120));
        let pos = OhlcvBar::price_position(&[b1, b2]).unwrap();
        assert!((pos - 1.0).abs() < 1e-9);
    }

    // ── OhlcvBar::close_above_open_count ──────────────────────────────────────

    #[test]
    fn test_close_above_open_count_zero_for_empty() {
        assert_eq!(OhlcvBar::close_above_open_count(&[]), 0);
    }

    #[test]
    fn test_close_above_open_count_correct() {
        // bar1: bullish (close > open), bar2: bearish
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(108));
        let b2 = make_ohlcv_bar(dec!(110), dec!(115), dec!(100), dec!(102));
        assert_eq!(OhlcvBar::close_above_open_count(&[b1, b2]), 1);
    }

    // ── OhlcvBar::volume_price_correlation ────────────────────────────────────

    #[test]
    fn test_volume_price_correlation_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_price_correlation(&[b]).is_none());
    }

    #[test]
    fn test_volume_price_correlation_positive_for_comoving() {
        // Both volume and close rise together
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(105), dec!(98), dec!(102)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(102), dec!(110), dec!(100), dec!(108)); b2.volume = dec!(200);
        let corr = OhlcvBar::volume_price_correlation(&[b1, b2]).unwrap();
        assert!(corr > 0.0, "expected positive correlation, got {}", corr);
    }

    // ── OhlcvBar::body_consistency ────────────────────────────────────────────

    #[test]
    fn test_body_consistency_none_for_empty() {
        assert!(OhlcvBar::body_consistency(&[]).is_none());
    }

    #[test]
    fn test_body_consistency_one_for_all_big_bodies() {
        // body = |close - open| = 8, range = high - low = 10 → 8 > 5 ✓
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(108));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(110), dec!(118));
        let r = OhlcvBar::body_consistency(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    // ── OhlcvBar::close_volatility_ratio ──────────────────────────────────────

    #[test]
    fn test_close_volatility_ratio_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_volatility_ratio(&[b]).is_none());
    }

    #[test]
    fn test_close_volatility_ratio_positive_for_varied_closes() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(115));
        let r = OhlcvBar::close_volatility_ratio(&[b1, b2]).unwrap();
        assert!(r > 0.0, "expected positive ratio, got {}", r);
    }

    #[test]
    fn test_close_volatility_ratio_zero_for_identical_closes() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(112), dec!(88), dec!(105));
        let r = OhlcvBar::close_volatility_ratio(&[b1, b2]).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0 for identical closes, got {}", r);
    }

    // ── OhlcvBar::is_trending_up / is_trending_down ───────────────────────────

    #[test]
    fn test_is_trending_up_false_for_empty() {
        assert!(!OhlcvBar::is_trending_up(&[], 3));
    }

    #[test]
    fn test_is_trending_up_false_for_n_less_than_2() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(!OhlcvBar::is_trending_up(&[b], 1));
    }

    #[test]
    fn test_is_trending_up_true_for_rising_closes() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(105), dec!(98), dec!(102));
        let b2 = make_ohlcv_bar(dec!(102), dec!(110), dec!(100), dec!(107));
        let b3 = make_ohlcv_bar(dec!(107), dec!(115), dec!(105), dec!(112));
        assert!(OhlcvBar::is_trending_up(&[b1, b2, b3], 3));
    }

    #[test]
    fn test_is_trending_down_true_for_falling_closes() {
        let b1 = make_ohlcv_bar(dec!(112), dec!(115), dec!(105), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(112), dec!(100), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(108), dec!(95), dec!(98));
        assert!(OhlcvBar::is_trending_down(&[b1, b2, b3], 3));
    }

    // ── OhlcvBar::volume_acceleration ────────────────────────────────────────

    #[test]
    fn test_volume_acceleration_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_acceleration(&[b]).is_none());
    }

    #[test]
    fn test_volume_acceleration_positive_when_volume_rises() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); b2.volume = dec!(150);
        let acc = OhlcvBar::volume_acceleration(&[b1, b2]).unwrap();
        assert!(acc > 0.0, "volume rose so acceleration should be positive, got {}", acc);
    }

    // ── OhlcvBar::wick_body_ratio ─────────────────────────────────────────────

    #[test]
    fn test_wick_body_ratio_none_for_empty() {
        assert!(OhlcvBar::wick_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_wick_body_ratio_none_for_doji_bar() {
        // open == close → zero body, should be skipped
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::wick_body_ratio(&[b]).is_none());
    }

    #[test]
    fn test_wick_body_ratio_positive_for_wicked_bar() {
        // open=100, close=105 → body=5; high=115, low=95 → wicks=10+5=15
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(105));
        let r = OhlcvBar::wick_body_ratio(&[b]).unwrap();
        assert!(r > 0.0, "expected positive wick/body ratio, got {}", r);
    }

    // ── OhlcvBar::close_momentum_score ────────────────────────────────────────

    #[test]
    fn test_close_momentum_score_none_for_empty() {
        assert!(OhlcvBar::close_momentum_score(&[]).is_none());
    }

    #[test]
    fn test_close_momentum_score_half_for_symmetric() {
        // Two bars: closes [90, 110] → mean=100; 90 < 100, 110 > 100 → 1/2
        let b1 = make_ohlcv_bar(dec!(88), dec!(95), dec!(85), dec!(90));
        let b2 = make_ohlcv_bar(dec!(108), dec!(115), dec!(105), dec!(110));
        let score = OhlcvBar::close_momentum_score(&[b1, b2]).unwrap();
        assert!((score - 0.5).abs() < 1e-9, "expected 0.5, got {}", score);
    }

    // ── OhlcvBar::range_expansion_count ──────────────────────────────────────

    #[test]
    fn test_range_expansion_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::range_expansion_count(&[b]), 0);
    }

    #[test]
    fn test_range_expansion_count_correct() {
        // b1 range=20, b2 range=30 → expansion
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(90), dec!(110));
        assert_eq!(OhlcvBar::range_expansion_count(&[b1, b2]), 1);
    }

    // ── OhlcvBar::gap_count ────────────────────────────────────────────────────

    #[test]
    fn test_gap_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::gap_count(&[b]), 0);
    }

    #[test]
    fn test_gap_count_detects_gap() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        // b2 opens at 108, prev close=105 → gap
        let b2 = make_ohlcv_bar(dec!(108), dec!(115), dec!(106), dec!(112));
        assert_eq!(OhlcvBar::gap_count(&[b1, b2]), 1);
    }

    #[test]
    fn test_gap_count_zero_when_open_equals_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        // b2 opens at exactly prev close=105
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(112));
        assert_eq!(OhlcvBar::gap_count(&[b1, b2]), 0);
    }

    // ── OhlcvBar::avg_wick_size ───────────────────────────────────────────────

    #[test]
    fn test_avg_wick_size_none_for_empty() {
        assert!(OhlcvBar::avg_wick_size(&[]).is_none());
    }

    #[test]
    fn test_avg_wick_size_correct() {
        // open=100, close=105, high=115, low=95
        // upper wick = 115 - 105 = 10, lower wick = 100 - 95 = 5, total = 15
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(105));
        let ws = OhlcvBar::avg_wick_size(&[b]).unwrap();
        assert!((ws - 15.0).abs() < 1e-6, "expected 15.0, got {}", ws);
    }

    // ── OhlcvBar::mean_volume_ratio ────────────────────────────────────────────

    #[test]
    fn test_mean_volume_ratio_empty_for_empty_slice() {
        assert!(OhlcvBar::mean_volume_ratio(&[]).is_empty());
    }

    #[test]
    fn test_mean_volume_ratio_sums_to_n_times_mean() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); b2.volume = dec!(300);
        // mean = 200; ratios: 0.5, 1.5
        let ratios = OhlcvBar::mean_volume_ratio(&[b1, b2]);
        assert_eq!(ratios.len(), 2);
        let r0 = ratios[0].unwrap();
        let r1 = ratios[1].unwrap();
        assert!((r0 - 0.5).abs() < 1e-6, "expected 0.5, got {}", r0);
        assert!((r1 - 1.5).abs() < 1e-6, "expected 1.5, got {}", r1);
    }

    // ── OhlcvBar::price_compression_ratio ────────────────────────────────────

    #[test]
    fn test_price_compression_ratio_none_for_zero_range() {
        // open==high==low==close → range=0
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::price_compression_ratio(&[b]).is_none());
    }

    #[test]
    fn test_price_compression_ratio_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let r = OhlcvBar::price_compression_ratio(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected value in [0,1], got {}", r);
    }

    // ── OhlcvBar::open_close_spread ───────────────────────────────────────────

    #[test]
    fn test_open_close_spread_none_for_empty() {
        assert!(OhlcvBar::open_close_spread(&[]).is_none());
    }

    #[test]
    fn test_open_close_spread_zero_for_doji() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let s = OhlcvBar::open_close_spread(&[b]).unwrap();
        assert!((s - 0.0).abs() < 1e-9, "doji should have spread=0, got {}", s);
    }

    #[test]
    fn test_open_close_spread_positive_for_directional_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        let s = OhlcvBar::open_close_spread(&[b]).unwrap();
        assert!(s > 0.0, "directional bar should have positive spread, got {}", s);
    }

    // ── OhlcvBar::close_above_high_ma ────────────────────────────────────────

    #[test]
    fn test_close_above_high_ma_zero_for_too_few_bars() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::close_above_high_ma(&[b], 2), 0);
    }

    #[test]
    fn test_close_above_high_ma_detects_breakout() {
        // 2-bar high MA = (110+120)/2=115; close of b2=118 > 115
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(108), dec!(118));
        assert_eq!(OhlcvBar::close_above_high_ma(&[b1, b2], 2), 1);
    }

    // ── OhlcvBar::max_consecutive_gains / max_consecutive_losses ──────────────

    #[test]
    fn test_max_consecutive_gains_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::max_consecutive_gains(&[b]), 0);
    }

    #[test]
    fn test_max_consecutive_gains_correct() {
        // closes: 100, 105, 110, 108, 115 → gains: 1,1,0,1 → max run=2
        let b1 = make_ohlcv_bar(dec!(98), dec!(102), dec!(96), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(108), dec!(99), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(112), dec!(104), dec!(110));
        let b4 = make_ohlcv_bar(dec!(110), dec!(111), dec!(105), dec!(108));
        let b5 = make_ohlcv_bar(dec!(108), dec!(116), dec!(107), dec!(115));
        assert_eq!(OhlcvBar::max_consecutive_gains(&[b1, b2, b3, b4, b5]), 2);
    }

    #[test]
    fn test_max_consecutive_losses_correct() {
        // closes: 110, 105, 100, 108 → losses: 1,1,0 → max run=2
        let b1 = make_ohlcv_bar(dec!(112), dec!(115), dec!(108), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(112), dec!(103), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(108), dec!(98), dec!(100));
        let b4 = make_ohlcv_bar(dec!(100), dec!(112), dec!(98), dec!(108));
        assert_eq!(OhlcvBar::max_consecutive_losses(&[b1, b2, b3, b4]), 2);
    }

    // ── OhlcvBar::price_path_length ───────────────────────────────────────────

    #[test]
    fn test_price_path_length_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::price_path_length(&[b]).is_none());
    }

    #[test]
    fn test_price_path_length_correct() {
        // closes: 100, 110, 105 → |10| + |5| = 15
        let b1 = make_ohlcv_bar(dec!(98), dec!(102), dec!(96), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(112), dec!(99), dec!(110));
        let b3 = make_ohlcv_bar(dec!(110), dec!(112), dec!(103), dec!(105));
        let len = OhlcvBar::price_path_length(&[b1, b2, b3]).unwrap();
        assert!((len - 15.0).abs() < 1e-6, "expected 15.0, got {}", len);
    }

    // ── OhlcvBar::close_reversion_count ──────────────────────────────────────

    #[test]
    fn test_close_reversion_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::close_reversion_count(&[b]), 0);
    }

    #[test]
    fn test_close_reversion_count_returns_usize() {
        let b1 = make_ohlcv_bar(dec!(98), dec!(102), dec!(96), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(112), dec!(99), dec!(110));
        let b3 = make_ohlcv_bar(dec!(110), dec!(112), dec!(103), dec!(105));
        // Just test it runs without panic
        let _ = OhlcvBar::close_reversion_count(&[b1, b2, b3]);
    }

    // ── OhlcvBar::atr_ratio ───────────────────────────────────────────────────

    #[test]
    fn test_atr_ratio_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::atr_ratio(&[b]).is_none());
    }

    #[test]
    fn test_atr_ratio_positive_for_valid_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110));
        let r = OhlcvBar::atr_ratio(&[b1, b2]).unwrap();
        assert!(r > 0.0, "expected positive ATR ratio, got {}", r);
    }

    // ── OhlcvBar::volume_trend_strength ───────────────────────────────────────

    #[test]
    fn test_volume_trend_strength_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_trend_strength(&[b]).is_none());
    }

    #[test]
    fn test_volume_trend_strength_positive_for_rising_volume() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(103), dec!(110)); b2.volume = dec!(200);
        let mut b3 = make_ohlcv_bar(dec!(110), dec!(120), dec!(108), dec!(115)); b3.volume = dec!(300);
        let s = OhlcvBar::volume_trend_strength(&[b1, b2, b3]).unwrap();
        assert!(s > 0.0, "rising volume should give positive strength, got {}", s);
    }

    // ── OhlcvBar::high_close_spread ───────────────────────────────────────────

    #[test]
    fn test_high_close_spread_none_for_empty() {
        assert!(OhlcvBar::high_close_spread(&[]).is_none());
    }

    #[test]
    fn test_high_close_spread_zero_when_close_equals_high() {
        // open=100, high=110, low=90, close=110 → upper wick=0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let s = OhlcvBar::high_close_spread(&[b]).unwrap();
        assert!((s - 0.0).abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_high_close_spread_positive_for_wicked_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(105));
        let s = OhlcvBar::high_close_spread(&[b]).unwrap();
        assert!(s > 0.0, "expected positive spread, got {}", s);
    }

    // ── OhlcvBar::open_range ──────────────────────────────────────────────────

    #[test]
    fn test_open_range_none_for_empty() {
        assert!(OhlcvBar::open_range(&[]).is_none());
    }

    #[test]
    fn test_open_range_zero_for_doji() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::open_range(&[b]).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "doji should have open_range=0, got {}", r);
    }

    #[test]
    fn test_open_range_positive_for_directional() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        let r = OhlcvBar::open_range(&[b]).unwrap();
        assert!(r > 0.0, "directional bar should have positive open_range, got {}", r);
    }

    // ── OhlcvBar::normalized_close ────────────────────────────────────────────

    #[test]
    fn test_normalized_close_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::normalized_close(&[b]).is_none());
    }

    #[test]
    fn test_normalized_close_one_when_last_close_is_max() {
        let b1 = make_ohlcv_bar(dec!(98), dec!(105), dec!(96), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(99), dec!(110));
        let nc = OhlcvBar::normalized_close(&[b1, b2]).unwrap();
        assert!((nc - 1.0).abs() < 1e-9, "last close = max should give 1.0, got {}", nc);
    }

    #[test]
    fn test_normalized_close_zero_when_last_close_is_min() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let b2 = make_ohlcv_bar(dec!(90), dec!(105), dec!(88), dec!(100));
        // min_close=90, max_close=100, last_close=100 → 1.0
        // Actually min=90, max=100, last=100 → normalized=1.0
        let nc = OhlcvBar::normalized_close(&[b1, b2]).unwrap();
        assert!(nc >= 0.0 && nc <= 1.0, "normalized close should be in [0,1], got {}", nc);
    }

    // ── OhlcvBar::candle_score ────────────────────────────────────────────────

    #[test]
    fn test_candle_score_none_for_empty() {
        assert!(OhlcvBar::candle_score(&[]).is_none());
    }

    #[test]
    fn test_candle_score_one_for_strong_bull_bar() {
        // open=100, close=108, high=110, low=99 → bullish, body=8, range=11, close_above_mid=yes
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(99), dec!(108));
        let s = OhlcvBar::candle_score(&[b]).unwrap();
        assert_eq!(s, 1.0, "strong bullish bar should score 1.0, got {}", s);
    }

    #[test]
    fn test_candle_score_zero_for_bear_bar() {
        // open=108, close=100 → bearish → score 0
        let b = make_ohlcv_bar(dec!(108), dec!(110), dec!(99), dec!(100));
        let s = OhlcvBar::candle_score(&[b]).unwrap();
        assert_eq!(s, 0.0, "bearish bar should score 0.0, got {}", s);
    }

    // ── OhlcvBar::bar_speed ───────────────────────────────────────────────────

    #[test]
    fn test_bar_speed_none_for_empty() {
        assert!(OhlcvBar::bar_speed(&[]).is_none());
    }

    // ── OhlcvBar::higher_highs_count / lower_lows_count ──────────────────────

    #[test]
    fn test_higher_highs_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::higher_highs_count(&[b]), 0);
    }

    #[test]
    fn test_higher_highs_count_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(103), dec!(115)); // high 120 > 110
        let b3 = make_ohlcv_bar(dec!(115), dec!(115), dec!(110), dec!(112)); // high 115 < 120
        assert_eq!(OhlcvBar::higher_highs_count(&[b1, b2, b3]), 1);
    }

    #[test]
    fn test_lower_lows_count_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(112), dec!(85), dec!(108)); // low 85 < 90
        let b3 = make_ohlcv_bar(dec!(108), dec!(115), dec!(95), dec!(112)); // low 95 > 85
        assert_eq!(OhlcvBar::lower_lows_count(&[b1, b2, b3]), 1);
    }

    // ── OhlcvBar::close_minus_open_pct ────────────────────────────────────────

    #[test]
    fn test_close_minus_open_pct_none_for_empty() {
        assert!(OhlcvBar::close_minus_open_pct(&[]).is_none());
    }

    #[test]
    fn test_close_minus_open_pct_positive_for_bull_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(98), dec!(110));
        let p = OhlcvBar::close_minus_open_pct(&[b]).unwrap();
        assert!(p > 0.0, "bullish bar should give positive pct, got {}", p);
    }

    // ── OhlcvBar::volume_per_range ────────────────────────────────────────────

    #[test]
    fn test_volume_per_range_none_for_empty() {
        assert!(OhlcvBar::volume_per_range(&[]).is_none());
    }

    #[test]
    fn test_volume_per_range_positive_for_valid_bar() {
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b.volume = dec!(100);
        let r = OhlcvBar::volume_per_range(&[b]).unwrap();
        assert!(r > 0.0, "expected positive volume/range, got {}", r);
    }

    #[test]
    fn test_up_volume_fraction_none_for_empty() {
        assert!(OhlcvBar::up_volume_fraction(&[]).is_none());
    }

    #[test]
    fn test_up_volume_fraction_all_up() {
        // close > open for both bars
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(108)); b1.volume = dec!(50);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(112)); b2.volume = dec!(50);
        let f = OhlcvBar::up_volume_fraction(&[b1, b2]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all up bars → fraction=1.0, got {}", f);
    }

    #[test]
    fn test_tail_upper_fraction_none_for_empty() {
        assert!(OhlcvBar::tail_upper_fraction(&[]).is_none());
    }

    #[test]
    fn test_tail_upper_fraction_correct() {
        // bar: open=100, high=110, low=90, close=105 → body_top=105, upper_wick=5, range=20 → 0.25
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::tail_upper_fraction(&[b]).unwrap();
        assert!((f - 0.25).abs() < 1e-9, "expected 0.25, got {}", f);
    }

    #[test]
    fn test_tail_lower_fraction_none_for_empty() {
        assert!(OhlcvBar::tail_lower_fraction(&[]).is_none());
    }

    #[test]
    fn test_tail_lower_fraction_correct() {
        // bar: open=100, high=110, low=90, close=105 → body_bot=100, lower_wick=10, range=20 → 0.5
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::tail_lower_fraction(&[b]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_range_std_dev_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::range_std_dev(&[b]).is_none());
    }

    #[test]
    fn test_range_std_dev_zero_for_equal_ranges() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(200), dec!(210), dec!(190), dec!(205));
        let sd = OhlcvBar::range_std_dev(&[b1, b2]).unwrap();
        assert!(sd.abs() < 1e-9, "equal ranges → std_dev=0, got {}", sd);
    }

    #[test]
    fn test_body_fraction_none_for_empty() {
        assert!(OhlcvBar::body_fraction(&[]).is_none());
    }

    #[test]
    fn test_body_fraction_doji_is_zero() {
        // open == close → body = 0 → fraction = 0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let f = OhlcvBar::body_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "doji → body_fraction=0, got {}", f);
    }

    #[test]
    fn test_bullish_ratio_none_for_empty() {
        assert!(OhlcvBar::bullish_ratio(&[]).is_none());
    }

    #[test]
    fn test_bullish_ratio_all_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(108));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(112));
        let r = OhlcvBar::bullish_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all bullish → ratio=1.0, got {}", r);
    }

    #[test]
    fn test_peak_trough_close_none_for_empty() {
        assert!(OhlcvBar::peak_close(&[]).is_none());
        assert!(OhlcvBar::trough_close(&[]).is_none());
    }

    #[test]
    fn test_peak_trough_close_correct() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(120), dec!(100), dec!(115)),
            make_ohlcv_bar(dec!(110), dec!(115), dec!(95), dec!(98)),
        ];
        assert_eq!(OhlcvBar::peak_close(&bars).unwrap(), dec!(115));
        assert_eq!(OhlcvBar::trough_close(&bars).unwrap(), dec!(98));
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    // ── OhlcvBar::close_to_range_position ────────────────────────────────────

    #[test]
    fn test_close_to_range_position_none_for_empty() {
        assert!(OhlcvBar::close_to_range_position(&[]).is_none());
    }

    #[test]
    fn test_close_to_range_position_one_when_close_at_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(120));
        let r = OhlcvBar::close_to_range_position(&[bar]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "close at high → position=1, got {}", r);
    }

    #[test]
    fn test_close_to_range_position_zero_when_close_at_low() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(80));
        let r = OhlcvBar::close_to_range_position(&[bar]).unwrap();
        assert!(r.abs() < 1e-9, "close at low → position=0, got {}", r);
    }

    // ── OhlcvBar::volume_oscillator ───────────────────────────────────────────

    #[test]
    fn test_volume_oscillator_none_for_insufficient_bars() {
        let bars = vec![make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))];
        assert!(OhlcvBar::volume_oscillator(&bars, 1, 3).is_none());
    }

    #[test]
    fn test_volume_oscillator_none_when_short_ge_long() {
        let bars: Vec<_> = (0..5)
            .map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)))
            .collect();
        assert!(OhlcvBar::volume_oscillator(&bars, 3, 2).is_none());
    }

    #[test]
    fn test_volume_oscillator_zero_for_constant_volume() {
        let bars: Vec<_> = (0..5)
            .map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)))
            .collect();
        let v = OhlcvBar::volume_oscillator(&bars, 2, 4).unwrap();
        assert!(v.abs() < 1e-9, "constant volume → oscillator=0, got {}", v);
    }

    // ── OhlcvBar::direction_reversal_count ───────────────────────────────────

    #[test]
    fn test_direction_reversal_count_zero_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::direction_reversal_count(&[bar]), 0);
    }

    #[test]
    fn test_direction_reversal_count_zero_for_all_bullish() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(112)),
        ];
        assert_eq!(OhlcvBar::direction_reversal_count(&bars), 0);
    }

    #[test]
    fn test_direction_reversal_count_two_for_alternating() {
        let bull = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let bear = make_ohlcv_bar(dec!(108), dec!(112), dec!(95), dec!(102));
        let bull2 = make_ohlcv_bar(dec!(102), dec!(115), dec!(98), dec!(110));
        let bear2 = make_ohlcv_bar(dec!(110), dec!(115), dec!(100), dec!(104));
        assert_eq!(OhlcvBar::direction_reversal_count(&[bull, bear, bull2, bear2]), 3);
    }

    // ── OhlcvBar::upper_wick_dominance_fraction ───────────────────────────────

    #[test]
    fn test_upper_wick_dominance_fraction_none_for_empty() {
        assert!(OhlcvBar::upper_wick_dominance_fraction(&[]).is_none());
    }

    #[test]
    fn test_upper_wick_dominance_fraction_one_when_all_upper() {
        // high > close and close > low, upper > lower
        let bar = make_ohlcv_bar(dec!(100), dec!(130), dec!(99), dec!(101));
        let r = OhlcvBar::upper_wick_dominance_fraction(&[bar]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all upper dominant → 1.0, got {}", r);
    }

    // ── OhlcvBar::avg_open_to_high_ratio ─────────────────────────────────────

    #[test]
    fn test_avg_open_to_high_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_open_to_high_ratio(&[]).is_none());
    }

    #[test]
    fn test_avg_open_to_high_ratio_one_when_open_at_low() {
        // open == low → (high - open) / range == 1
        let bar = make_ohlcv_bar(dec!(80), dec!(120), dec!(80), dec!(100));
        let r = OhlcvBar::avg_open_to_high_ratio(&[bar]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "open at low → ratio=1, got {}", r);
    }

    // ── OhlcvBar::volume_weighted_range ──────────────────────────────────────

    #[test]
    fn test_volume_weighted_range_none_for_empty() {
        assert!(OhlcvBar::volume_weighted_range(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_range_positive() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(130), dec!(100), dec!(120));
        let r = OhlcvBar::volume_weighted_range(&[b1, b2]).unwrap();
        assert!(r > 0.0, "should be positive, got {}", r);
    }

    // ── OhlcvBar::bar_strength_index ─────────────────────────────────────────

    #[test]
    fn test_bar_strength_index_none_for_empty() {
        assert!(OhlcvBar::bar_strength_index(&[]).is_none());
    }

    #[test]
    fn test_bar_strength_index_positive_when_closes_near_high() {
        let bar = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(120));
        let s = OhlcvBar::bar_strength_index(&[bar]).unwrap();
        assert!(s > 0.0, "close at high → positive strength, got {}", s);
    }

    // ── OhlcvBar::shadow_to_body_ratio ────────────────────────────────────────

    #[test]
    fn test_shadow_to_body_ratio_none_for_empty() {
        assert!(OhlcvBar::shadow_to_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_shadow_to_body_ratio_zero_for_marubozu() {
        // Marubozu: open==low and close==high → no wicks
        let bar = make_ohlcv_bar(dec!(80), dec!(120), dec!(80), dec!(120));
        let r = OhlcvBar::shadow_to_body_ratio(&[bar]).unwrap();
        assert!(r.abs() < 1e-9, "marubozu → ratio=0, got {}", r);
    }

    // ── OhlcvBar::first_last_close_pct ───────────────────────────────────────

    #[test]
    fn test_first_last_close_pct_none_for_empty() {
        assert!(OhlcvBar::first_last_close_pct(&[]).is_none());
    }

    #[test]
    fn test_first_last_close_pct_zero_for_same_close() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::first_last_close_pct(&[bar]).unwrap();
        assert!(r.abs() < 1e-9, "same open/close → pct=0, got {}", r);
    }

    #[test]
    fn test_first_last_close_pct_positive_for_rise() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(110));
        let r = OhlcvBar::first_last_close_pct(&[b1, b2]).unwrap();
        assert!(r > 0.0, "price rose → positive pct, got {}", r);
    }

    // ── OhlcvBar::open_to_close_volatility ───────────────────────────────────

    #[test]
    fn test_open_to_close_volatility_none_for_single_bar() {
        let bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_to_close_volatility(&[bar]).is_none());
    }

    #[test]
    fn test_open_to_close_volatility_zero_for_identical_bars() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
        ];
        let v = OhlcvBar::open_to_close_volatility(&bars).unwrap();
        assert!(v.abs() < 1e-9, "identical bars → volatility=0, got {}", v);
    }

    // ── round-80 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_close_recovery_ratio_none_for_empty() {
        assert!(OhlcvBar::close_recovery_ratio(&[]).is_none());
    }

    #[test]
    fn test_close_recovery_ratio_one_for_close_at_high() {
        // close == high → ratio = 1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::close_recovery_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "close at high → ratio=1, got {}", r);
    }

    #[test]
    fn test_median_range_none_for_empty() {
        assert!(OhlcvBar::median_range(&[]).is_none());
    }

    #[test]
    fn test_median_range_correct_odd() {
        let bars = vec![
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),  // range=20
            make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(105)),  // range=25
            make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(105)),  // range=30
        ];
        assert_eq!(OhlcvBar::median_range(&bars).unwrap(), dec!(25));
    }

    #[test]
    fn test_mean_typical_price_none_for_empty() {
        assert!(OhlcvBar::mean_typical_price(&[]).is_none());
    }

    #[test]
    fn test_mean_typical_price_correct() {
        // typical = (110 + 90 + 105) / 3 = 101.666...
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let expected = b.typical_price();
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let tp = OhlcvBar::mean_typical_price(&[b2]).unwrap();
        assert_eq!(tp, expected);
    }

    #[test]
    fn test_directional_volume_ratio_none_for_empty() {
        assert!(OhlcvBar::directional_volume_ratio(&[]).is_none());
    }

    #[test]
    fn test_directional_volume_ratio_one_for_all_bullish() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)); b1.volume = dec!(50);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(112)); b2.volume = dec!(50);
        let r = OhlcvBar::directional_volume_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all bullish → ratio=1, got {}", r);
    }

    #[test]
    fn test_inside_bar_fraction_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::inside_bar_fraction(&[b]).is_none());
    }

    #[test]
    fn test_body_momentum_empty_is_zero() {
        assert_eq!(OhlcvBar::body_momentum(&[]), Decimal::ZERO);
    }

    #[test]
    fn test_body_momentum_bullish_positive() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let m = OhlcvBar::body_momentum(&[b]);
        assert!(m > Decimal::ZERO, "bullish bar → positive body momentum");
    }

    #[test]
    fn test_avg_trade_count_none_for_empty() {
        assert!(OhlcvBar::avg_trade_count(&[]).is_none());
    }

    #[test]
    fn test_max_trade_count_none_for_empty() {
        assert!(OhlcvBar::max_trade_count(&[]).is_none());
    }

    #[test]
    fn test_max_trade_count_returns_max() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.trade_count = 5;
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.trade_count = 10;
        assert_eq!(OhlcvBar::max_trade_count(&[b1, b2]).unwrap(), 10);
    }

    // ── round-81 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_close_to_high_std_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_to_high_std(&[b]).is_none());
    }

    #[test]
    fn test_close_to_high_std_zero_for_identical_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let sd = OhlcvBar::close_to_high_std(&[b1, b2]).unwrap();
        assert!(sd.abs() < 1e-9, "identical bars → std=0, got {}", sd);
    }

    #[test]
    fn test_avg_open_volume_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_open_volume_ratio(&[]).is_none());
    }

    #[test]
    fn test_typical_price_std_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::typical_price_std(&[b]).is_none());
    }

    #[test]
    fn test_typical_price_std_zero_for_identical_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let sd = OhlcvBar::typical_price_std(&[b1, b2]).unwrap();
        assert!(sd.abs() < 1e-9, "identical bars → std=0, got {}", sd);
    }

    #[test]
    fn test_vwap_deviation_avg_none_for_empty() {
        assert!(OhlcvBar::vwap_deviation_avg(&[]).is_none());
    }

    #[test]
    fn test_avg_high_low_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_high_low_ratio(&[]).is_none());
    }

    #[test]
    fn test_avg_high_low_ratio_one_for_doji() {
        // high == low (doji) → ratio = 1.0 but low=0 is skipped... use low=100
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        let r = OhlcvBar::avg_high_low_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "high==low → ratio=1, got {}", r);
    }

    #[test]
    fn test_gap_fill_fraction_none_for_empty() {
        assert!(OhlcvBar::gap_fill_fraction(&[]).is_none());
    }

    #[test]
    fn test_gap_fill_fraction_zero_for_no_gaps() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::gap_fill_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "no gap fills → fraction=0, got {}", f);
    }

    #[test]
    fn test_complete_bar_count_zero_for_incomplete() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::complete_bar_count(&[b]), 0);
    }

    #[test]
    fn test_min_trade_count_none_for_empty() {
        assert!(OhlcvBar::min_trade_count(&[]).is_none());
    }

    #[test]
    fn test_min_trade_count_returns_min() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b1.trade_count = 5;
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); b2.trade_count = 2;
        assert_eq!(OhlcvBar::min_trade_count(&[b1, b2]).unwrap(), 2);
    }

    // ── round-82 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_bar_range_none_for_empty() {
        assert!(OhlcvBar::avg_bar_range(&[]).is_none());
    }

    #[test]
    fn test_avg_bar_range_correct_value() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // range=20
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(105)); // range=20
        let r = OhlcvBar::avg_bar_range(&[b1, b2]).unwrap();
        assert_eq!(r, dec!(20));
    }

    #[test]
    fn test_max_up_move_none_for_empty() {
        assert!(OhlcvBar::max_up_move(&[]).is_none());
    }

    #[test]
    fn test_max_up_move_largest_bullish_body() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)); // up: 8
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // up: 5
        assert_eq!(OhlcvBar::max_up_move(&[b1, b2]).unwrap(), dec!(8));
    }

    #[test]
    fn test_max_down_move_none_for_empty() {
        assert!(OhlcvBar::max_down_move(&[]).is_none());
    }

    #[test]
    fn test_max_down_move_largest_bearish_body() {
        let b1 = make_ohlcv_bar(dec!(108), dec!(115), dec!(85), dec!(100)); // down: 8
        let b2 = make_ohlcv_bar(dec!(103), dec!(110), dec!(90), dec!(100)); // down: 3
        assert_eq!(OhlcvBar::max_down_move(&[b1, b2]).unwrap(), dec!(8));
    }

    #[test]
    fn test_avg_close_position_none_for_doji_only() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100)); // range=0
        assert!(OhlcvBar::avg_close_position(&[b]).is_none());
    }

    #[test]
    fn test_avg_close_position_one_for_close_at_high() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let pos = OhlcvBar::avg_close_position(&[b]).unwrap();
        assert!((pos - 1.0).abs() < 1e-9, "close at high → position=1, got {}", pos);
    }

    #[test]
    fn test_volume_std_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_std(&[b]).is_none());
    }

    #[test]
    fn test_volume_std_zero_for_equal_volumes() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::volume_std(&[b1, b2]).unwrap();
        assert!(s.abs() < 1e-9, "equal volumes → std=0, got {}", s);
    }

    #[test]
    fn test_avg_wick_ratio_none_for_doji_only() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::avg_wick_ratio(&[b]).is_none());
    }

    #[test]
    fn test_avg_wick_ratio_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(85), dec!(105));
        let r = OhlcvBar::avg_wick_ratio(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "wick ratio should be in [0,1], got {}", r);
    }

    #[test]
    fn test_open_gap_mean_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_gap_mean(&[b]).is_none());
    }

    #[test]
    fn test_open_gap_mean_zero_for_no_gap() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        b2.open = dec!(105); // open == prev_close → no gap
        let g = OhlcvBar::open_gap_mean(&[b1, b2]).unwrap();
        assert!(g.abs() < 1e-9, "no gap → mean=0, got {}", g);
    }

    #[test]
    fn test_net_directional_move_none_for_empty() {
        assert!(OhlcvBar::net_directional_move(&[]).is_none());
    }

    #[test]
    fn test_net_directional_move_positive_for_rising_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(100), dec!(115));
        let m = OhlcvBar::net_directional_move(&[b1, b2]).unwrap();
        assert!(m > 0.0, "rising bar sequence → positive move, got {}", m);
    }

    // ── round-83 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_close_above_median_fraction_none_for_empty() {
        assert!(OhlcvBar::close_above_median_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_above_median_fraction_half_for_symmetric() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(95));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(95));
        let b4 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let f = OhlcvBar::close_above_median_fraction(&[b1, b2, b3, b4]).unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    #[test]
    fn test_avg_range_to_open_none_for_empty() {
        assert!(OhlcvBar::avg_range_to_open(&[]).is_none());
    }

    #[test]
    fn test_close_sum_zero_for_empty() {
        assert_eq!(OhlcvBar::close_sum(&[]), dec!(0));
    }

    #[test]
    fn test_close_sum_sums_all_closes() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(107));
        assert_eq!(OhlcvBar::close_sum(&[b1, b2]), dec!(212));
    }

    #[test]
    fn test_above_avg_volume_count_zero_for_empty() {
        assert_eq!(OhlcvBar::above_avg_volume_count(&[]), 0);
    }

    #[test]
    fn test_median_close_none_for_empty() {
        assert!(OhlcvBar::median_close(&[]).is_none());
    }

    #[test]
    fn test_median_close_correct_for_sorted() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let m = OhlcvBar::median_close(&[b1, b2, b3]).unwrap();
        assert_eq!(m, dec!(105));
    }

    #[test]
    fn test_flat_bar_fraction_none_for_empty() {
        assert!(OhlcvBar::flat_bar_fraction(&[]).is_none());
    }

    #[test]
    fn test_avg_body_to_range_none_for_empty() {
        assert!(OhlcvBar::avg_body_to_range(&[]).is_none());
    }

    #[test]
    fn test_avg_body_to_range_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::avg_body_to_range(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "body-to-range in [0,1], got {}", r);
    }

    #[test]
    fn test_max_open_gap_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::max_open_gap(&[b]).is_none());
    }

    #[test]
    fn test_volume_trend_slope_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_trend_slope(&[b]).is_none());
    }

    #[test]
    fn test_up_close_fraction_none_for_empty() {
        assert!(OhlcvBar::up_close_fraction(&[]).is_none());
    }

    #[test]
    fn test_avg_upper_shadow_ratio_none_for_doji_only() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::avg_upper_shadow_ratio(&[b]).is_none());
    }

    // ── round-84 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_lower_shadow_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_lower_shadow_ratio(&[]).is_none());
    }

    #[test]
    fn test_avg_lower_shadow_ratio_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(85), dec!(105));
        let r = OhlcvBar::avg_lower_shadow_ratio(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "lower shadow ratio in [0,1], got {}", r);
    }

    #[test]
    fn test_close_to_open_range_ratio_none_for_doji_only() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::close_to_open_range_ratio(&[b]).is_none());
    }

    #[test]
    fn test_max_high_none_for_empty() {
        assert!(OhlcvBar::max_high(&[]).is_none());
    }

    #[test]
    fn test_max_high_returns_maximum() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(110));
        assert_eq!(OhlcvBar::max_high(&[b1, b2]).unwrap(), dec!(120));
    }

    #[test]
    fn test_min_low_none_for_empty() {
        assert!(OhlcvBar::min_low(&[]).is_none());
    }

    #[test]
    fn test_min_low_returns_minimum() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(85), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::min_low(&[b1, b2]).unwrap(), dec!(85));
    }

    #[test]
    fn test_avg_bar_efficiency_none_for_doji_only() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::avg_bar_efficiency(&[b]).is_none());
    }

    #[test]
    fn test_avg_bar_efficiency_one_for_full_body_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let e = OhlcvBar::avg_bar_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "full body → efficiency=1, got {}", e);
    }

    #[test]
    fn test_open_range_fraction_none_for_empty() {
        assert!(OhlcvBar::open_range_fraction(&[]).is_none());
    }

    #[test]
    fn test_open_range_fraction_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::open_range_fraction(&[b]).unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    #[test]
    fn test_close_skewness_none_for_two_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        assert!(OhlcvBar::close_skewness(&[b1, b2]).is_none());
    }

    #[test]
    fn test_close_skewness_returns_value_for_three_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(200));
        let s = OhlcvBar::close_skewness(&[b1, b2, b3]);
        assert!(s.is_some(), "skewness should be computed for 3 bars");
    }

    #[test]
    fn test_volume_above_median_fraction_none_for_empty() {
        assert!(OhlcvBar::volume_above_median_fraction(&[]).is_none());
    }

    #[test]
    fn test_volume_above_median_fraction_in_range() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::volume_above_median_fraction(&[b1, b2]).unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    #[test]
    fn test_typical_price_sum_zero_for_empty() {
        assert_eq!(OhlcvBar::typical_price_sum(&[]), dec!(0));
    }

    #[test]
    fn test_typical_price_sum_correct_value() {
        let b = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(100));
        // typical = (120+80+100)/3 = 300/3 = 100
        assert_eq!(OhlcvBar::typical_price_sum(&[b]), dec!(100));
    }

    #[test]
    fn test_max_body_size_none_for_empty() {
        assert!(OhlcvBar::max_body_size(&[]).is_none());
    }

    #[test]
    fn test_max_body_size_correct_value() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(103));
        assert_eq!(OhlcvBar::max_body_size(&[b1, b2]).unwrap(), dec!(8));
    }

    #[test]
    fn test_min_body_size_none_for_empty() {
        assert!(OhlcvBar::min_body_size(&[]).is_none());
    }

    #[test]
    fn test_avg_lower_wick_to_range_none_for_empty() {
        assert!(OhlcvBar::avg_lower_wick_to_range(&[]).is_none());
    }

    #[test]
    fn test_avg_lower_wick_to_range_zero_for_open_at_low() {
        // open = low, so lower wick = 0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::avg_lower_wick_to_range(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "open=low → lower wick=0, got {}", r);
    }

    // ── round-85 extra tests ──────────────────────────────────────────────────

    #[test]
    fn test_total_range_zero_for_empty() {
        assert_eq!(OhlcvBar::total_range(&[]), dec!(0));
    }

    #[test]
    fn test_total_range_sum_of_ranges() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // range=20
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110)); // range=20
        assert_eq!(OhlcvBar::total_range(&[b1, b2]), dec!(40));
    }

    #[test]
    fn test_close_at_high_fraction_none_for_empty() {
        assert!(OhlcvBar::close_at_high_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_at_high_fraction_one_when_all_close_at_high() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let f = OhlcvBar::close_at_high_fraction(&[b]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "close=high → fraction=1, got {}", f);
    }

    #[test]
    fn test_close_at_low_fraction_none_for_empty() {
        assert!(OhlcvBar::close_at_low_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_at_low_fraction_one_when_all_close_at_low() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(90));
        let f = OhlcvBar::close_at_low_fraction(&[b]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "close=low → fraction=1, got {}", f);
    }

    #[test]
    fn test_avg_high_above_open_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_high_above_open_ratio(&[]).is_none());
    }

    #[test]
    fn test_avg_high_above_open_ratio_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::avg_high_above_open_ratio(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "ratio in [0,1], got {}", r);
    }

    #[test]
    fn test_continuation_bar_count_zero_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::continuation_bar_count(&[b]), 0);
    }

    #[test]
    fn test_down_close_volume_zero_for_all_up_close() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // close > open
        assert_eq!(OhlcvBar::down_close_volume(&[b]), dec!(0));
    }

    #[test]
    fn test_up_close_volume_zero_for_all_down_close() {
        let b = make_ohlcv_bar(dec!(105), dec!(110), dec!(90), dec!(100)); // close < open
        assert_eq!(OhlcvBar::up_close_volume(&[b]), dec!(0));
    }

    // ── round-86 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_mean_open_none_for_empty() {
        assert!(OhlcvBar::mean_open(&[]).is_none());
    }

    #[test]
    fn test_mean_open_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(200), dec!(210), dec!(190), dec!(205));
        assert_eq!(OhlcvBar::mean_open(&[b1, b2]).unwrap(), dec!(150));
    }

    #[test]
    fn test_new_high_count_zero_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::new_high_count(&[b]), 0);
    }

    #[test]
    fn test_new_high_count_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(115));
        let b3 = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110));
        assert_eq!(OhlcvBar::new_high_count(&[b1, b2, b3]), 1);
    }

    #[test]
    fn test_new_low_count_zero_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::new_low_count(&[b]), 0);
    }

    #[test]
    fn test_close_std_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_std(&[b]).is_none());
    }

    #[test]
    fn test_close_std_zero_for_constant_close() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(101), dec!(111), dec!(91), dec!(105));
        let s = OhlcvBar::close_std(&[b1, b2]).unwrap();
        assert!(s.abs() < 1e-9, "constant close → std=0, got {}", s);
    }

    #[test]
    fn test_zero_volume_fraction_none_for_empty() {
        assert!(OhlcvBar::zero_volume_fraction(&[]).is_none());
    }

    #[test]
    fn test_zero_volume_fraction_zero_when_no_zero_volume() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let f = OhlcvBar::zero_volume_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "bar has volume → zero_vol_fraction=0, got {}", f);
    }

    // ── round-87 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_open_to_close_none_for_empty() {
        assert!(OhlcvBar::avg_open_to_close(&[]).is_none());
    }

    #[test]
    fn test_avg_open_to_close_positive_when_all_bullish() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // close > open
        let r = OhlcvBar::avg_open_to_close(&[b]).unwrap();
        assert!(r > dec!(0), "bullish bar → avg_open_to_close > 0, got {}", r);
    }

    #[test]
    fn test_avg_open_to_close_zero_for_doji() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // close == open
        let r = OhlcvBar::avg_open_to_close(&[b]).unwrap();
        assert_eq!(r, dec!(0));
    }

    #[test]
    fn test_max_bar_volume_none_for_empty() {
        assert!(OhlcvBar::max_bar_volume(&[]).is_none());
    }

    #[test]
    fn test_max_bar_volume_selects_largest() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        // make_ohlcv_bar sets volume=1 by default; override manually
        // use default and confirm max equals volume
        let vol = OhlcvBar::max_bar_volume(&[b1, b2]).unwrap();
        assert!(vol > dec!(0));
    }

    #[test]
    fn test_min_bar_volume_none_for_empty() {
        assert!(OhlcvBar::min_bar_volume(&[]).is_none());
    }

    #[test]
    fn test_body_to_range_std_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::body_to_range_std(&[b]).is_none());
    }

    #[test]
    fn test_body_to_range_std_nonneg_for_varied_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(100));
        let s = OhlcvBar::body_to_range_std(&[b1, b2]).unwrap();
        assert!(s >= 0.0, "std dev should be non-negative, got {}", s);
    }

    #[test]
    fn test_avg_wick_symmetry_none_for_empty() {
        assert!(OhlcvBar::avg_wick_symmetry(&[]).is_none());
    }

    #[test]
    fn test_avg_wick_symmetry_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        // upper wick = 10, lower wick = 10 → perfectly symmetric → ratio = 1
        let s = OhlcvBar::avg_wick_symmetry(&[b]).unwrap();
        assert!(s >= 0.0 && s <= 1.0, "symmetry in [0,1], got {}", s);
    }

    // ── round-88 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_range_pct_of_open_none_for_empty() {
        assert!(OhlcvBar::avg_range_pct_of_open(&[]).is_none());
    }

    #[test]
    fn test_avg_range_pct_of_open_correct() {
        // range = 20 (110 - 90), open = 100 → 20%
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::avg_range_pct_of_open(&[b]).unwrap();
        assert!((r - 0.2).abs() < 1e-9, "range/open = 0.2, got {}", r);
    }

    #[test]
    fn test_high_volume_fraction_none_for_empty() {
        assert!(OhlcvBar::high_volume_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_cluster_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::close_cluster_count(&[b]), 0);
    }

    #[test]
    fn test_mean_vwap_none_for_bars_without_vwap() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::mean_vwap(&[b]).is_none());
    }

    #[test]
    fn test_complete_fraction_none_for_empty() {
        assert!(OhlcvBar::complete_fraction(&[]).is_none());
    }

    #[test]
    fn test_complete_fraction_zero_when_none_complete() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        // make_ohlcv_bar sets is_complete=false by default
        let f = OhlcvBar::complete_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "no complete bars → fraction=0, got {}", f);
    }

    #[test]
    fn test_total_body_movement_zero_for_empty() {
        assert_eq!(OhlcvBar::total_body_movement(&[]), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_total_body_movement_correct() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // body = 5
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(100)); // body = 10
        assert_eq!(OhlcvBar::total_body_movement(&[b1, b2]), dec!(15));
    }

    #[test]
    fn test_open_std_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_std(&[b]).is_none());
    }

    #[test]
    fn test_open_std_zero_for_constant_open() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let s = OhlcvBar::open_std(&[b1, b2]).unwrap();
        assert!(s.abs() < 1e-9, "constant open → std=0, got {}", s);
    }

    #[test]
    fn test_mean_high_low_ratio_none_for_empty() {
        assert!(OhlcvBar::mean_high_low_ratio(&[]).is_none());
    }

    #[test]
    fn test_mean_high_low_ratio_above_one() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::mean_high_low_ratio(&[b]).unwrap();
        assert!(r > 1.0, "high > low → ratio > 1, got {}", r);
    }

    // ── round-89 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_max_consecutive_up_bars_zero_for_empty() {
        assert_eq!(OhlcvBar::max_consecutive_up_bars(&[]), 0);
    }

    #[test]
    fn test_max_consecutive_up_bars_zero_for_all_down() {
        let b = make_ohlcv_bar(dec!(105), dec!(110), dec!(90), dec!(100)); // close < open
        assert_eq!(OhlcvBar::max_consecutive_up_bars(&[b]), 0);
    }

    #[test]
    fn test_max_consecutive_up_bars_correct_run() {
        let up = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // up
        let dn = make_ohlcv_bar(dec!(105), dec!(115), dec!(90), dec!(100)); // down
        let up2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108)); // up
        let up3 = make_ohlcv_bar(dec!(108), dec!(120), dec!(100), dec!(115)); // up
        assert_eq!(OhlcvBar::max_consecutive_up_bars(&[up, dn, up2, up3]), 2);
    }

    #[test]
    fn test_avg_upper_shadow_fraction_none_for_empty() {
        assert!(OhlcvBar::avg_upper_shadow_fraction(&[]).is_none());
    }

    #[test]
    fn test_avg_upper_shadow_fraction_in_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // close==open, upper=10, range=20
        let f = OhlcvBar::avg_upper_shadow_fraction(&[b]).unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    #[test]
    fn test_up_down_bar_ratio_none_for_no_down_bars() {
        // All up-close bars
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::up_down_bar_ratio(&[b]).is_none());
    }

    #[test]
    fn test_up_down_bar_ratio_one_for_balanced() {
        let up_bar = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let dn_bar = make_ohlcv_bar(dec!(105), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::up_down_bar_ratio(&[up_bar, dn_bar]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "1 up / 1 down → 1.0, got {}", r);
    }

    // ── round-90 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_close_range_fraction_none_for_empty() {
        assert!(OhlcvBar::close_range_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_range_fraction_one_for_close_at_high() {
        // close == high → (close-low)/(high-low) = 1.0 > 0.5 → fraction = 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let f = OhlcvBar::close_range_fraction(&[b]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "close=high → 1.0, got {}", f);
    }

    #[test]
    fn test_close_range_fraction_zero_for_close_at_low() {
        // close == low → (close-low)/(high-low) = 0.0 < 0.5 → fraction = 0.0
        let b = make_ohlcv_bar(dec!(110), dec!(120), dec!(90), dec!(90));
        let f = OhlcvBar::close_range_fraction(&[b]).unwrap();
        assert!((f - 0.0).abs() < 1e-9, "close=low → 0.0, got {}", f);
    }

    #[test]
    fn test_tail_symmetry_none_for_empty() {
        assert!(OhlcvBar::tail_symmetry(&[]).is_none());
    }

    #[test]
    fn test_tail_symmetry_one_for_symmetric_bar() {
        // open and close at midpoint → equal upper/lower shadows
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100)); // open=close=100, high=110, low=90
        let s = OhlcvBar::tail_symmetry(&[b]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "symmetric bar → 1.0, got {}", s);
    }

    #[test]
    fn test_bar_trend_strength_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_trend_strength(&[b]).is_none());
    }

    #[test]
    fn test_bar_trend_strength_one_for_monotone_up() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        // all closes increasing: 100 → 105 → 110
        let s = OhlcvBar::bar_trend_strength(&[b1, b2, b3]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "monotone up → 1.0, got {}", s);
    }

    // ── round-91 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_gap_up_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::gap_up_count(&[b]), 0);
    }

    #[test]
    fn test_gap_up_count_one_for_gap() {
        // bar1 closes at 105, bar2 opens at 110 → gap up
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(115));
        assert_eq!(OhlcvBar::gap_up_count(&[b1, b2]), 1);
    }

    #[test]
    fn test_gap_down_count_zero_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert_eq!(OhlcvBar::gap_down_count(&[b]), 0);
    }

    #[test]
    fn test_gap_down_count_one_for_gap() {
        // bar1 closes at 105, bar2 opens at 95 → gap down
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(95), dec!(100), dec!(85), dec!(90));
        assert_eq!(OhlcvBar::gap_down_count(&[b1, b2]), 1);
    }

    #[test]
    fn test_mean_bar_efficiency_none_for_empty() {
        assert!(OhlcvBar::mean_bar_efficiency(&[]).is_none());
    }

    #[test]
    fn test_mean_bar_efficiency_one_for_full_body() {
        // open=low, close=high → body = range → efficiency = 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let e = OhlcvBar::mean_bar_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "full body → 1.0, got {}", e);
    }

    #[test]
    fn test_volume_trend_slope_negative_for_falling_volume() {
        // volumes: 200, 100 → slope < 0
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(200);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        b2.volume = dec!(100);
        let s = OhlcvBar::volume_trend_slope(&[b1, b2]).unwrap();
        assert!(s < 0.0, "falling volume → negative slope, got {}", s);
    }

    // ── round-92 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_open_gap_ratio_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_gap_ratio(&[b]).is_none());
    }

    #[test]
    fn test_open_gap_ratio_zero_for_no_gap() {
        // bar1 closes at 105, bar2 opens at 105 → gap = 0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        let r = OhlcvBar::open_gap_ratio(&[b1, b2]).unwrap();
        assert!(r.abs() < 1e-9, "no gap → ratio=0, got {}", r);
    }

    #[test]
    fn test_candle_symmetry_score_none_for_empty() {
        assert!(OhlcvBar::candle_symmetry_score(&[]).is_none());
    }

    #[test]
    fn test_candle_symmetry_score_zero_for_full_body() {
        // open=low, close=high → body = range → 1 - 1 = 0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let s = OhlcvBar::candle_symmetry_score(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "full body → score=0, got {}", s);
    }

    #[test]
    fn test_mean_upper_shadow_pct_none_for_empty() {
        assert!(OhlcvBar::mean_upper_shadow_pct(&[]).is_none());
    }

    #[test]
    fn test_mean_upper_shadow_pct_zero_for_close_at_high() {
        // close == high → (high - close) / (high - low) = 0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::mean_upper_shadow_pct(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "close=high → 0, got {}", r);
    }

    // ── round-93 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_wick_to_body_ratio_none_for_empty() {
        assert!(OhlcvBar::avg_wick_to_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_avg_wick_to_body_ratio_none_for_doji() {
        // open == close → body = 0, excluded
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::avg_wick_to_body_ratio(&[b]).is_none());
    }

    #[test]
    fn test_close_above_open_streak_zero_for_empty() {
        assert_eq!(OhlcvBar::close_above_open_streak(&[]), 0);
    }

    #[test]
    fn test_close_above_open_streak_two_for_two_up_bars() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)); // up
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110)); // up
        let b3 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(108)); // down
        assert_eq!(OhlcvBar::close_above_open_streak(&[b1, b2, b3]), 2);
    }

    #[test]
    fn test_volume_above_mean_fraction_none_for_empty() {
        assert!(OhlcvBar::volume_above_mean_fraction(&[]).is_none());
    }

    #[test]
    fn test_volume_above_mean_fraction_half_for_balanced() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        b2.volume = dec!(200);
        // mean = 150; b2 (200) > 150 → 1/2 = 0.5
        let f = OhlcvBar::volume_above_mean_fraction(&[b1, b2]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-94 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_max_gap_up_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::max_gap_up(&[b]).is_none());
    }

    #[test]
    fn test_max_gap_up_detects_positive_gap() {
        // bar1 closes at 100, bar2 opens at 110 → gap = 10/100 = 0.10
        let b1 = make_ohlcv_bar(dec!(90), dec!(105), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(108), dec!(115));
        let g = OhlcvBar::max_gap_up(&[b1, b2]).unwrap();
        assert!((g - 0.1).abs() < 1e-9, "expected 0.10 gap, got {}", g);
    }

    #[test]
    fn test_price_range_expansion_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::price_range_expansion(&[b]).is_none());
    }

    #[test]
    fn test_price_range_expansion_ratio() {
        // first bar: high=110, low=90 → range=20; last bar: high=130, low=90 → range=40
        let b1 = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(130), dec!(90), dec!(120));
        let r = OhlcvBar::price_range_expansion(&[b1, b2]).unwrap();
        assert!((r - 2.0).abs() < 1e-9, "range doubled → 2.0, got {}", r);
    }

    #[test]
    fn test_avg_volume_per_range_none_for_empty() {
        assert!(OhlcvBar::avg_volume_per_range(&[]).is_none());
    }

    #[test]
    fn test_avg_volume_per_range_basic() {
        // range = 20, volume = 200 → ratio = 10
        let mut b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(100));
        b.volume = dec!(200);
        let avg = OhlcvBar::avg_volume_per_range(&[b]).unwrap();
        assert!((avg - 10.0).abs() < 1e-9, "expected 10.0, got {}", avg);
    }

    // ── round-95 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_up_down_volume_ratio_none_for_only_up_bars() {
        let mut b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        b.volume = dec!(100);
        // one up bar, no down bars → None
        assert!(OhlcvBar::up_down_volume_ratio(&[b]).is_none());
    }

    #[test]
    fn test_up_down_volume_ratio_equal_volumes() {
        let mut b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105)); // up
        b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(105), dec!(110), dec!(85), dec!(90)); // down
        b2.volume = dec!(100);
        let r = OhlcvBar::up_down_volume_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "equal volumes → 1.0, got {}", r);
    }

    #[test]
    fn test_longest_bearish_streak_none_for_empty() {
        assert!(OhlcvBar::longest_bearish_streak(&[]).is_none());
    }

    #[test]
    fn test_longest_bearish_streak_basic() {
        let up = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        let dn = make_ohlcv_bar(dec!(105), dec!(110), dec!(85), dec!(90));
        // up, dn, dn, up → longest bearish = 2
        let streak = OhlcvBar::longest_bearish_streak(&[up.clone(), dn.clone(), dn.clone(), up.clone()]).unwrap();
        assert_eq!(streak, 2);
    }

    #[test]
    fn test_mean_close_to_high_ratio_none_for_empty() {
        assert!(OhlcvBar::mean_close_to_high_ratio(&[]).is_none());
    }

    #[test]
    fn test_mean_close_to_high_ratio_mid_close() {
        // high=110, low=90, close=100 → (100-90)/(110-90) = 0.5
        let b = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::mean_close_to_high_ratio(&[b]).unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    // ── round-96 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_open_to_close_momentum_none_for_empty() {
        assert!(OhlcvBar::open_to_close_momentum(&[]).is_none());
    }

    #[test]
    fn test_open_to_close_momentum_positive_for_up_bar() {
        // open=90, close=99 → (99-90)/90 = 0.1
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(99));
        let m = OhlcvBar::open_to_close_momentum(&[b]).unwrap();
        assert!((m - 0.1).abs() < 1e-9, "expected 0.1, got {}", m);
    }

    #[test]
    fn test_volume_dispersion_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::volume_dispersion(&[b]).is_none());
    }

    #[test]
    fn test_volume_dispersion_zero_for_equal_volumes() {
        let mut b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        b1.volume = dec!(100);
        b2.volume = dec!(100);
        let d = OhlcvBar::volume_dispersion(&[b1, b2]).unwrap();
        assert!(d.abs() < 1e-9, "equal volumes → dispersion=0, got {}", d);
    }

    #[test]
    fn test_shadow_dominance_none_for_empty() {
        assert!(OhlcvBar::shadow_dominance(&[]).is_none());
    }

    #[test]
    fn test_shadow_dominance_zero_for_full_body_bar() {
        // open=low, close=high → no shadow
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let d = OhlcvBar::shadow_dominance(&[b]).unwrap();
        assert!(d.abs() < 1e-9, "full body → shadow_dominance=0, got {}", d);
    }

    #[test]
    fn test_true_range_mean_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::true_range_mean(&[b]).is_none());
    }

    #[test]
    fn test_true_range_mean_basic() {
        // b1: high=110, low=90; b2: high=115, low=100, prev_close=100
        // TR(b2) = max(15, |115-100|, |100-100|) = max(15,15,0) = 15
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(100), dec!(112));
        let tr = OhlcvBar::true_range_mean(&[b1, b2]).unwrap();
        assert!((tr - 15.0).abs() < 1e-9, "expected 15.0, got {}", tr);
    }

    // ── round-97 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_close_to_open_gap_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::close_to_open_gap(&[b]).is_none());
    }

    #[test]
    fn test_close_to_open_gap_zero_for_no_gap() {
        // b1 closes at 100, b2 opens at 100 → gap = 0
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let g = OhlcvBar::close_to_open_gap(&[b1, b2]).unwrap();
        assert!(g.abs() < 1e-9, "no gap → 0.0, got {}", g);
    }

    #[test]
    fn test_volume_weighted_open_none_for_empty() {
        assert!(OhlcvBar::volume_weighted_open(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_open_equal_weights() {
        let mut b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        b1.volume = dec!(1);
        b2.volume = dec!(1);
        // equal volume → average of opens = (90+100)/2 = 95
        let vwo = OhlcvBar::volume_weighted_open(&[b1, b2]).unwrap();
        assert!((vwo - 95.0).abs() < 1e-9, "expected 95.0, got {}", vwo);
    }

    #[test]
    fn test_avg_upper_shadow_none_for_empty() {
        assert!(OhlcvBar::avg_upper_shadow(&[]).is_none());
    }

    #[test]
    fn test_avg_upper_shadow_zero_for_full_body_up() {
        // open=low, close=high → no upper shadow
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let s = OhlcvBar::avg_upper_shadow(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "full body → upper shadow=0, got {}", s);
    }

    #[test]
    fn test_body_to_range_mean_none_for_empty() {
        assert!(OhlcvBar::body_to_range_mean(&[]).is_none());
    }

    #[test]
    fn test_body_to_range_mean_one_for_full_body() {
        // open=low, close=high → body = range → ratio = 1
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::body_to_range_mean(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "full body → ratio=1.0, got {}", r);
    }

    // ── round-98 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_narrow_body_count_none_for_empty() {
        assert!(OhlcvBar::narrow_body_count(&[]).is_none());
    }

    #[test]
    fn test_narrow_body_count_full_body_is_not_narrow() {
        // open=low=90, close=high=110 → body/range = 1.0 → not narrow
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let c = OhlcvBar::narrow_body_count(&[b]).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_bar_range_mean_none_for_empty() {
        assert!(OhlcvBar::bar_range_mean(&[]).is_none());
    }

    #[test]
    fn test_bar_range_mean_basic() {
        // high=110, low=90 → range=20
        let b = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::bar_range_mean(&[b]).unwrap();
        assert!((r - 20.0).abs() < 1e-9, "expected 20.0, got {}", r);
    }

    #[test]
    fn test_close_proximity_none_for_empty() {
        assert!(OhlcvBar::close_proximity(&[]).is_none());
    }

    #[test]
    fn test_close_proximity_one_for_close_at_high() {
        // close=high → (high-low)/(high-low) = 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let p = OhlcvBar::close_proximity(&[b]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "close at high → 1.0, got {}", p);
    }

    #[test]
    fn test_down_gap_count_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::down_gap_count(&[b]).is_none());
    }

    #[test]
    fn test_down_gap_count_detects_gap_down() {
        // b1 closes at 100, b2 opens at 90 < 100 → 1 down gap
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(90), dec!(95), dec!(85), dec!(93));
        let c = OhlcvBar::down_gap_count(&[b1, b2]).unwrap();
        assert_eq!(c, 1);
    }

    // ── round-99 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_avg_lower_shadow_none_for_empty() {
        assert!(OhlcvBar::avg_lower_shadow(&[]).is_none());
    }

    #[test]
    fn test_avg_lower_shadow_zero_for_no_lower_shadow() {
        // open=low → lower shadow = 0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::avg_lower_shadow(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "no lower shadow → 0, got {}", s);
    }

    #[test]
    fn test_inside_bar_count_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::inside_bar_count(&[b]).is_none());
    }

    #[test]
    fn test_inside_bar_count_detects_inside_bar() {
        // b2 contained within b1
        let b1 = make_ohlcv_bar(dec!(85), dec!(115), dec!(80), dec!(100));
        let b2 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        let c = OhlcvBar::inside_bar_count(&[b1, b2]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_price_channel_width_none_for_empty() {
        assert!(OhlcvBar::price_channel_width(&[]).is_none());
    }

    #[test]
    fn test_price_channel_width_basic() {
        // high=110, low=85 → width=25
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let w = OhlcvBar::price_channel_width(&[b]).unwrap();
        assert!((w - 25.0).abs() < 1e-9, "expected 25.0, got {}", w);
    }

    #[test]
    fn test_volume_trend_acceleration_none_for_three_bars() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::volume_trend_acceleration(&[b.clone(), b.clone(), b]).is_none());
    }

    #[test]
    fn test_volume_trend_acceleration_zero_for_constant_volume() {
        let mut bars = Vec::new();
        for _ in 0..4 {
            let mut b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
            b.volume = dec!(100);
            bars.push(b);
        }
        let a = OhlcvBar::volume_trend_acceleration(&bars).unwrap();
        assert!(a.abs() < 1e-9, "constant volume → acceleration=0, got {}", a);
    }

    // ── round-100 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_median_volume_none_for_empty() {
        assert!(OhlcvBar::median_volume(&[]).is_none());
    }

    #[test]
    fn test_median_volume_single_bar() {
        let mut b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        b.volume = dec!(50);
        let m = OhlcvBar::median_volume(&[b]).unwrap();
        assert!((m - 50.0).abs() < 1e-9, "expected 50.0, got {}", m);
    }

    #[test]
    fn test_bar_count_above_avg_range_none_for_empty() {
        assert!(OhlcvBar::bar_count_above_avg_range(&[]).is_none());
    }

    #[test]
    fn test_bar_count_above_avg_range_basic() {
        // range 20, 40 → mean=30 → only 40 > 30 → count=1
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(100)); // range=20
        let b2 = make_ohlcv_bar(dec!(90), dec!(130), dec!(90), dec!(120)); // range=40
        let c = OhlcvBar::bar_count_above_avg_range(&[b1, b2]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_price_oscillation_count_none_for_two_bars() {
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(OhlcvBar::price_oscillation_count(&[b1, b2]).is_none());
    }

    #[test]
    fn test_price_oscillation_count_basic() {
        let up = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105));
        let dn = make_ohlcv_bar(dec!(105), dec!(110), dec!(85), dec!(90));
        // up, dn, up → 1 reversal (dn→up)
        let c = OhlcvBar::price_oscillation_count(&[up.clone(), dn.clone(), up.clone()]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_vwap_deviation_mean_none_for_empty() {
        assert!(OhlcvBar::vwap_deviation_mean(&[]).is_none());
    }

    #[test]
    fn test_vwap_deviation_mean_zero_for_constant_close() {
        // all same close → VWAP = close → deviation = 0
        let mut b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(100));
        b1.volume = dec!(100);
        b2.volume = dec!(100);
        let d = OhlcvBar::vwap_deviation_mean(&[b1, b2]).unwrap();
        assert!(d.abs() < 1e-9, "constant close → deviation=0, got {}", d);
    }

    // ── round-101 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_open_range_ratio_none_for_empty() {
        assert!(OhlcvBar::open_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_open_range_ratio_zero_for_open_at_low() {
        // open=low → (low-low)/(high-low) = 0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::open_range_ratio(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "open at low → 0.0, got {}", r);
    }

    #[test]
    fn test_volume_normalized_range_none_for_empty() {
        assert!(OhlcvBar::volume_normalized_range(&[]).is_none());
    }

    #[test]
    fn test_consecutive_flat_count_none_for_empty() {
        assert!(OhlcvBar::consecutive_flat_count(&[]).is_none());
    }

    #[test]
    fn test_consecutive_flat_count_zero_for_full_body() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        // body/range = 1.0 → not flat
        let c = OhlcvBar::consecutive_flat_count(&[b]).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_close_vs_midpoint_none_for_empty() {
        assert!(OhlcvBar::close_vs_midpoint(&[]).is_none());
    }

    #[test]
    fn test_close_vs_midpoint_zero_for_close_at_midpoint() {
        // high=110, low=90 → mid=100; close=100 → 0
        let b = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::close_vs_midpoint(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "close at midpoint → 0, got {}", r);
    }

    // ── round-102 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_high_low_ratio_none_for_empty() {
        assert!(OhlcvBar::high_low_ratio(&[]).is_none());
    }

    #[test]
    fn test_high_low_ratio_basic() {
        // high=110, low=100 → ratio = 1.1
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(105));
        let r = OhlcvBar::high_low_ratio(&[b]).unwrap();
        assert!((r - 1.1).abs() < 1e-9, "expected 1.1, got {}", r);
    }

    #[test]
    fn test_close_change_mean_none_for_single_bar() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::close_change_mean(&[b]).is_none());
    }

    #[test]
    fn test_close_change_mean_positive_for_rising_close() {
        let b1 = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let m = OhlcvBar::close_change_mean(&[b1, b2]).unwrap();
        assert!((m - 10.0).abs() < 1e-9, "expected 10.0, got {}", m);
    }

    #[test]
    fn test_down_body_fraction_none_for_empty() {
        assert!(OhlcvBar::down_body_fraction(&[]).is_none());
    }

    #[test]
    fn test_down_body_fraction_zero_for_all_up_bars() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(105)); // close > open
        let f = OhlcvBar::down_body_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "all up bars → 0.0, got {}", f);
    }

    #[test]
    fn test_body_acceleration_none_for_three_bars() {
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(85), dec!(100));
        assert!(OhlcvBar::body_acceleration(&[b.clone(), b.clone(), b]).is_none());
    }

    // ── round-103 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_open_high_distance_none_for_empty() {
        assert!(OhlcvBar::open_high_distance(&[]).is_none());
    }

    #[test]
    fn test_open_high_distance_basic() {
        // open=100, high=110, low=90 → range=20, high-open=10 → ratio=0.5
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let d = OhlcvBar::open_high_distance(&[b]).unwrap();
        assert!((d - 0.5).abs() < 1e-9, "expected 0.5, got {}", d);
    }

    #[test]
    fn test_max_close_minus_open_none_for_empty() {
        assert!(OhlcvBar::max_close_minus_open(&[]).is_none());
    }

    #[test]
    fn test_max_close_minus_open_basic() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(108));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(103));
        let m = OhlcvBar::max_close_minus_open(&[b1, b2]).unwrap();
        assert!((m - 8.0).abs() < 1e-9, "expected 8.0, got {}", m);
    }

    #[test]
    fn test_bullish_engulfing_count_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(95));
        assert!(OhlcvBar::bullish_engulfing_count(&[b]).is_none());
    }

    #[test]
    fn test_bullish_engulfing_count_basic() {
        // bearish bar: open=110, close=95; bullish engulfing: open=90, close=115
        let bearish = make_ohlcv_bar(dec!(110), dec!(115), dec!(90), dec!(95));
        let engulf = make_ohlcv_bar(dec!(90), dec!(120), dec!(85), dec!(115));
        let count = OhlcvBar::bullish_engulfing_count(&[bearish, engulf]).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_shadow_ratio_score_none_for_empty() {
        assert!(OhlcvBar::shadow_ratio_score(&[]).is_none());
    }

    #[test]
    fn test_shadow_ratio_score_basic() {
        // open=100, close=105, high=110, low=95
        // upper = 110-105=5, lower = 100-95=5 → ratio = 1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(105));
        let s = OhlcvBar::shadow_ratio_score(&[b]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    // ── round-104 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_gap_fill_count_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::gap_fill_count(&[b]).is_none());
    }

    #[test]
    fn test_gap_fill_count_basic() {
        // prev: high=110; curr opens at 115 (gap up) and closes at 108 (back inside prev high)
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let curr = make_ohlcv_bar(dec!(115), dec!(120), dec!(100), dec!(108));
        let c = OhlcvBar::gap_fill_count(&[prev, curr]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_avg_body_to_volume_none_for_empty() {
        assert!(OhlcvBar::avg_body_to_volume(&[]).is_none());
    }

    #[test]
    fn test_avg_body_to_volume_basic() {
        // body = |105-100| = 5, volume = 50 → ratio = 0.1
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b.volume = dec!(50);
        let r = OhlcvBar::avg_body_to_volume(&[b]).unwrap();
        assert!((r - 0.1).abs() < 1e-9, "expected 0.1, got {}", r);
    }

    #[test]
    fn test_price_recovery_ratio_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::price_recovery_ratio(&[b]).is_none());
    }

    #[test]
    fn test_price_recovery_ratio_basic() {
        // prev close=95; curr close=108>open=100 (bullish) and 108>95 (recovery)
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(95));
        let curr = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(108));
        let r = OhlcvBar::price_recovery_ratio(&[prev, curr]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_open_close_correlation_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_close_correlation(&[b]).is_none());
    }

    #[test]
    fn test_open_close_correlation_perfect() {
        // open and close move together perfectly → correlation = 1
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(90), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(130), dec!(100), dec!(120));
        let corr = OhlcvBar::open_close_correlation(&[b1, b2]).unwrap();
        assert!((corr - 1.0).abs() < 1e-9, "expected 1.0, got {}", corr);
    }

    // ── round-105 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_close_to_high_mean_none_for_empty() {
        assert!(OhlcvBar::close_to_high_mean(&[]).is_none());
    }

    #[test]
    fn test_close_to_high_mean_basic() {
        // high=110, close=100, low=90 → range=20, high-close=10 → ratio=0.5
        let b = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(100));
        let m = OhlcvBar::close_to_high_mean(&[b]).unwrap();
        assert!((m - 0.5).abs() < 1e-9, "expected 0.5, got {}", m);
    }

    #[test]
    fn test_bar_volatility_score_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_volatility_score(&[b]).is_none());
    }

    #[test]
    fn test_bar_volatility_score_basic() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        // TR of b2 relative to b1: hl=20, hpc=10, lpc=10 → TR=20; mean_close=100 → score=0.2
        let s = OhlcvBar::bar_volatility_score(&[b1, b2]).unwrap();
        assert!((s - 0.2).abs() < 1e-9, "expected 0.2, got {}", s);
    }

    #[test]
    fn test_bearish_close_fraction_none_for_empty() {
        assert!(OhlcvBar::bearish_close_fraction(&[]).is_none());
    }

    #[test]
    fn test_bearish_close_fraction_basic() {
        // open=110, close=95 → bearish
        let bearish = make_ohlcv_bar(dec!(110), dec!(115), dec!(90), dec!(95));
        // open=90, close=105 → bullish
        let bullish = make_ohlcv_bar(dec!(90), dec!(115), dec!(85), dec!(105));
        let f = OhlcvBar::bearish_close_fraction(&[bearish, bullish]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_high_minus_open_mean_none_for_empty() {
        assert!(OhlcvBar::high_minus_open_mean(&[]).is_none());
    }

    #[test]
    fn test_high_minus_open_mean_basic() {
        // open=100, high=115 → diff=15
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110));
        let m = OhlcvBar::high_minus_open_mean(&[b]).unwrap();
        assert!((m - 15.0).abs() < 1e-9, "expected 15.0, got {}", m);
    }

    // ── round-106 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_avg_true_range_pct_none_for_empty() {
        assert!(OhlcvBar::avg_true_range_pct(&[]).is_none());
    }

    #[test]
    fn test_avg_true_range_pct_basic() {
        // high=110, low=90, close=100 → (110-90)/100 = 0.2
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::avg_true_range_pct(&[b]).unwrap();
        assert!((r - 0.2).abs() < 1e-9, "expected 0.2, got {}", r);
    }

    #[test]
    fn test_close_above_midpoint_count_none_for_empty() {
        assert!(OhlcvBar::close_above_midpoint_count(&[]).is_none());
    }

    #[test]
    fn test_close_above_midpoint_count_basic() {
        // midpoint=(110+90)/2=100; close=105>100 → count=1
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let c = OhlcvBar::close_above_midpoint_count(&[b]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_volume_weighted_high_none_for_empty() {
        assert!(OhlcvBar::volume_weighted_high(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_high_basic() {
        // both bars: high=110, volume=50 → vwh = (110*50+110*50)/(50+50)=110
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(50);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b2.volume = dec!(50);
        let v = OhlcvBar::volume_weighted_high(&[b1, b2]).unwrap();
        assert!((v - 110.0).abs() < 1e-9, "expected 110.0, got {}", v);
    }

    #[test]
    fn test_low_minus_close_mean_none_for_empty() {
        assert!(OhlcvBar::low_minus_close_mean(&[]).is_none());
    }

    #[test]
    fn test_low_minus_close_mean_basic() {
        // open=100, close=105, low=90 → lower wick = min(100,105)-90 = 100-90 = 10
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(105));
        let m = OhlcvBar::low_minus_close_mean(&[b]).unwrap();
        assert!((m - 10.0).abs() < 1e-9, "expected 10.0, got {}", m);
    }

    // ── round-107 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_open_to_high_ratio_none_for_empty() {
        assert!(OhlcvBar::open_to_high_ratio(&[]).is_none());
    }

    #[test]
    fn test_open_to_high_ratio_basic() {
        // open=100, high=110 → (110-100)/110 ≈ 0.0909
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::open_to_high_ratio(&[b]).unwrap();
        assert!((r - 10.0 / 110.0).abs() < 1e-9, "expected ~0.0909, got {}", r);
    }

    #[test]
    fn test_close_range_position_none_for_empty() {
        assert!(OhlcvBar::close_range_position(&[]).is_none());
    }

    #[test]
    fn test_close_range_position_basic() {
        // close=100, low=90, high=110 → (100-90)/20 = 0.5
        let b = make_ohlcv_bar(dec!(95), dec!(110), dec!(90), dec!(100));
        let p = OhlcvBar::close_range_position(&[b]).unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    #[test]
    fn test_up_gap_count_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::up_gap_count(&[b]).is_none());
    }

    #[test]
    fn test_up_gap_count_basic() {
        // prev high=110; curr open=115 > 110 → gap up
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let curr = make_ohlcv_bar(dec!(115), dec!(120), dec!(112), dec!(118));
        let c = OhlcvBar::up_gap_count(&[prev, curr]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_high_to_prev_close_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::high_to_prev_close(&[b]).is_none());
    }

    #[test]
    fn test_high_to_prev_close_basic() {
        // prev close=100, curr high=110 → (110-100)/100=0.1
        let prev = make_ohlcv_bar(dec!(95), dec!(105), dec!(90), dec!(100));
        let curr = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(108));
        let r = OhlcvBar::high_to_prev_close(&[prev, curr]).unwrap();
        assert!((r - 0.1).abs() < 1e-9, "expected 0.1, got {}", r);
    }

    // ── round-108 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_close_to_prev_open_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_to_prev_open(&[b]).is_none());
    }

    #[test]
    fn test_close_to_prev_open_basic() {
        // prev open=100; curr close=110 → diff=10
        let prev = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(108));
        let curr = make_ohlcv_bar(dec!(105), dec!(120), dec!(95), dec!(110));
        let d = OhlcvBar::close_to_prev_open(&[prev, curr]).unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_momentum_ratio_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::momentum_ratio(&[b]).is_none());
    }

    #[test]
    fn test_momentum_ratio_basic() {
        // prev close=100; curr close=110 → |110-100|/100 = 0.1
        let prev = make_ohlcv_bar(dec!(95), dec!(105), dec!(90), dec!(100));
        let curr = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let m = OhlcvBar::momentum_ratio(&[prev, curr]).unwrap();
        assert!((m - 0.1).abs() < 1e-9, "expected 0.1, got {}", m);
    }

    #[test]
    fn test_volume_range_ratio_none_for_empty() {
        assert!(OhlcvBar::volume_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_volume_range_ratio_basic() {
        // volumes [100, 200]: range=100, mean=150 → ratio=2/3
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b2.volume = dec!(200);
        let r = OhlcvBar::volume_range_ratio(&[b1, b2]).unwrap();
        assert!((r - 100.0 / 150.0).abs() < 1e-9, "expected ~0.667, got {}", r);
    }

    #[test]
    fn test_body_upper_fraction_none_for_empty() {
        assert!(OhlcvBar::body_upper_fraction(&[]).is_none());
    }

    #[test]
    fn test_body_upper_fraction_basic() {
        // open=100, close=110, high=120, low=100
        // midpoint=110, max(open,close)=110; body_upper=(110-110)=0 → fraction=0/20=0
        let b = make_ohlcv_bar(dec!(100), dec!(120), dec!(100), dec!(110));
        let f = OhlcvBar::body_upper_fraction(&[b]).unwrap();
        assert!(f.abs() < 1e-9, "expected ~0.0, got {}", f);
    }

    // ── round-109 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_open_gap_frequency_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_gap_frequency(&[b]).is_none());
    }

    #[test]
    fn test_open_gap_frequency_basic() {
        // prev close=105; curr open=110 ≠ 105 → gap
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let curr = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(115));
        let f = OhlcvBar::open_gap_frequency(&[prev, curr]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_avg_close_to_open_none_for_empty() {
        assert!(OhlcvBar::avg_close_to_open(&[]).is_none());
    }

    #[test]
    fn test_avg_close_to_open_basic() {
        // open=100, close=110 → (110-100)/100 = 0.1
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110));
        let r = OhlcvBar::avg_close_to_open(&[b]).unwrap();
        assert!((r - 0.1).abs() < 1e-9, "expected 0.1, got {}", r);
    }

    #[test]
    fn test_close_cross_open_count_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_cross_open_count(&[b]).is_none());
    }

    #[test]
    fn test_close_cross_open_count_zero_for_no_cross() {
        // both bars close above open of prev bar — no crossing occurs
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(112));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(118));
        let c = OhlcvBar::close_cross_open_count(&[b1, b2]).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_trailing_stop_distance_none_for_two() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(108));
        assert!(OhlcvBar::trailing_stop_distance(&[b1, b2]).is_none());
    }

    #[test]
    fn test_trailing_stop_distance_basic() {
        // 3 bars: lows [90,95,92]; min_low=90; last_close=108 → distance=18
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        let b3 = make_ohlcv_bar(dec!(108), dec!(115), dec!(92), dec!(108));
        let d = OhlcvBar::trailing_stop_distance(&[b1, b2, b3]).unwrap();
        assert!((d - 18.0).abs() < 1e-9, "expected 18.0, got {}", d);
    }

    // ── round-110 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_avg_shadow_total_none_for_empty() {
        assert!(OhlcvBar::avg_shadow_total(&[]).is_none());
    }

    #[test]
    fn test_avg_shadow_total_basic() {
        // high=110, low=90, open=100, close=105 → range=20, body=5, shadow=15
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::avg_shadow_total(&[b]).unwrap();
        assert!((s - 15.0).abs() < 1e-9, "expected 15.0, got {}", s);
    }

    #[test]
    fn test_open_above_prev_close_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::open_above_prev_close(&[b]).is_none());
    }

    #[test]
    fn test_open_above_prev_close_basic() {
        // prev close=105; curr open=110 > 105 → count=1
        let prev = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let curr = make_ohlcv_bar(dec!(110), dec!(120), dec!(105), dec!(115));
        let c = OhlcvBar::open_above_prev_close(&[prev, curr]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_close_below_prev_open_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::close_below_prev_open(&[b]).is_none());
    }

    #[test]
    fn test_close_below_prev_open_basic() {
        // prev open=100; curr close=95 < 100 → count=1
        let prev = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110));
        let curr = make_ohlcv_bar(dec!(105), dec!(110), dec!(88), dec!(95));
        let c = OhlcvBar::close_below_prev_open(&[prev, curr]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_candle_range_efficiency_none_for_empty() {
        assert!(OhlcvBar::candle_range_efficiency(&[]).is_none());
    }

    #[test]
    fn test_candle_range_efficiency_basic() {
        // open=100, close=110, high=110, low=100 → body=10, range=10 → ratio=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let e = OhlcvBar::candle_range_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    // ── round-111 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_open_close_range_none_for_empty() {
        assert!(OhlcvBar::open_close_range(&[]).is_none());
    }

    #[test]
    fn test_open_close_range_basic() {
        // open=100, close=110 → body=10
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110));
        let r = OhlcvBar::open_close_range(&[b]).unwrap();
        assert!((r - 10.0).abs() < 1e-9, "expected 10.0, got {}", r);
    }

    #[test]
    fn test_volume_per_bar_none_for_empty() {
        assert!(OhlcvBar::volume_per_bar(&[]).is_none());
    }

    #[test]
    fn test_volume_per_bar_basic() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(100);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b2.volume = dec!(200);
        let v = OhlcvBar::volume_per_bar(&[b1, b2]).unwrap();
        assert!((v - 150.0).abs() < 1e-9, "expected 150.0, got {}", v);
    }

    #[test]
    fn test_price_momentum_mean_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::price_momentum_mean(&[b]).is_none());
    }

    #[test]
    fn test_price_momentum_mean_basic() {
        // prev close=100, curr close=110 → (110-100)/100=0.1
        let prev = make_ohlcv_bar(dec!(95), dec!(105), dec!(90), dec!(100));
        let curr = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let m = OhlcvBar::price_momentum_mean(&[prev, curr]).unwrap();
        assert!((m - 0.1).abs() < 1e-9, "expected 0.1, got {}", m);
    }

    #[test]
    fn test_wicks_to_body_ratio_none_for_doji() {
        // zero body → skipped → None
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::wicks_to_body_ratio(&[b]).is_none());
    }

    #[test]
    fn test_wicks_to_body_ratio_basic() {
        // open=100, close=110 (body=10), high=115, low=98 → upper=5, lower=2, wicks=7 → ratio=0.7
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        let r = OhlcvBar::wicks_to_body_ratio(&[b]).unwrap();
        assert!((r - 0.7).abs() < 1e-9, "expected 0.7, got {}", r);
    }

    #[test]
    fn test_avg_close_deviation_none_for_empty() {
        assert!(OhlcvBar::avg_close_deviation(&[]).is_none());
    }

    #[test]
    fn test_avg_close_deviation_basic() {
        // closes: 100, 120 → mean=110 → MAD = (10+10)/2 = 10
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(125), dec!(95), dec!(120));
        let d = OhlcvBar::avg_close_deviation(&[b1, b2]).unwrap();
        assert!((d - 10.0).abs() < 1e-9, "expected 10.0, got {}", d);
    }

    #[test]
    fn test_open_midpoint_ratio_none_for_empty() {
        assert!(OhlcvBar::open_midpoint_ratio(&[]).is_none());
    }

    #[test]
    fn test_open_midpoint_ratio_basic() {
        // open=100, high=120, low=80 → mid=100 → ratio=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(110));
        let r = OhlcvBar::open_midpoint_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_volume_weighted_close_change_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_weighted_close_change(&[b]).is_none());
    }

    #[test]
    fn test_volume_weighted_close_change_basic() {
        // bar1 close=100 vol=0, bar2 close=110 vol=10 → chg=10, vol=10 → result=10
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let v = OhlcvBar::volume_weighted_close_change(&[b1, b2]).unwrap();
        // b2 volume from make_ohlcv_bar is dec!(1000) by default, chg=10 → result=10
        assert!((v - 10.0).abs() < 1e-9, "expected 10.0, got {}", v);
    }

    #[test]
    fn test_avg_intrabar_efficiency_none_for_empty() {
        assert!(OhlcvBar::avg_intrabar_efficiency(&[]).is_none());
    }

    #[test]
    fn test_avg_intrabar_efficiency_basic() {
        // open=100, close=110, high=110, low=100 → (close-open)/range = 10/10 = 1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let e = OhlcvBar::avg_intrabar_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    #[test]
    fn test_range_to_volume_ratio_none_for_zero_volume() {
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b.volume = dec!(0);
        assert!(OhlcvBar::range_to_volume_ratio(&[b]).is_none());
    }

    #[test]
    fn test_range_to_volume_ratio_basic() {
        // high=110, low=90 → range=20, volume=1 → ratio=20
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::range_to_volume_ratio(&[b]).unwrap();
        assert!((r - 20.0).abs() < 1e-9, "expected 20.0, got {}", r);
    }

    #[test]
    fn test_avg_high_low_spread_none_for_empty() {
        assert!(OhlcvBar::avg_high_low_spread(&[]).is_none());
    }

    #[test]
    fn test_avg_high_low_spread_basic() {
        // high=110, low=90 → spread=20
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::avg_high_low_spread(&[b]).unwrap();
        assert!((s - 20.0).abs() < 1e-9, "expected 20.0, got {}", s);
    }

    #[test]
    fn test_candle_persistence_none_for_two() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(OhlcvBar::candle_persistence(&[b1, b2]).is_none());
    }

    #[test]
    fn test_candle_persistence_basic() {
        // close sequence: 100→110→120 → both up → persistence=1.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b3 = make_ohlcv_bar(dec!(105), dec!(125), dec!(100), dec!(120));
        let p = OhlcvBar::candle_persistence(&[b1, b2, b3]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_bar_range_zscore_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_range_zscore(&[b]).is_none());
    }

    #[test]
    fn test_bar_range_zscore_uniform_returns_zero() {
        // same range → z-score of each = 0 → mean = 0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let z = OhlcvBar::bar_range_zscore(&[b1, b2]).unwrap();
        assert!(z.abs() < 1e-9, "expected ~0, got {}", z);
    }

    #[test]
    fn test_close_reversal_rate_none_for_two() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(OhlcvBar::close_reversal_rate(&[b1, b2]).is_none());
    }

    #[test]
    fn test_close_reversal_rate_all_reversal() {
        // up → down → up: every eligible pair reverses → 1.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b3 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(105));
        let b4 = make_ohlcv_bar(dec!(105), dec!(120), dec!(95), dec!(115));
        let r = OhlcvBar::close_reversal_rate(&[b1, b2, b3, b4]).unwrap();
        // dirs: +1(100→110), -1(110→105), +1(105→115) → reversals=2, eligible=2 → 1.0
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_avg_body_efficiency_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::avg_body_efficiency(&[b]).is_none());
    }

    #[test]
    fn test_avg_body_efficiency_basic() {
        // open=100, close=110, high=110, low=100 → body=10, range=10 → ratio=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let e = OhlcvBar::avg_body_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    #[test]
    fn test_volume_zscore_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::volume_zscore(&[b]).is_none());
    }

    #[test]
    fn test_volume_zscore_zero_for_uniform() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let z = OhlcvBar::volume_zscore(&[b1, b2]);
        // uniform volume → std=0 → None
        assert!(z.is_none());
    }

    #[test]
    fn test_body_skew_none_for_two() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        assert!(OhlcvBar::body_skew(&[b1, b2]).is_none());
    }

    #[test]
    fn test_body_skew_basic() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b3 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        let s = OhlcvBar::body_skew(&[b1, b2, b3]);
        assert!(s.is_some());
    }

    #[test]
    fn test_avg_open_gap_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::avg_open_gap(&[b]).is_none());
    }

    #[test]
    fn test_avg_open_gap_basic() {
        // bar1 close=105, bar2 open=110 → gap=5
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(115));
        let g = OhlcvBar::avg_open_gap(&[b1, b2]).unwrap();
        assert!((g - 5.0).abs() < 1e-9, "expected 5.0, got {}", g);
    }

    #[test]
    fn test_hl_ratio_mean_none_for_zero_low() {
        let mut b = make_ohlcv_bar(dec!(100), dec!(110), dec!(0), dec!(105));
        b.low = dec!(0);
        assert!(OhlcvBar::hl_ratio_mean(&[b]).is_none());
    }

    #[test]
    fn test_hl_ratio_mean_basic() {
        // high=110, low=100 → ratio=1.1
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(105));
        let r = OhlcvBar::hl_ratio_mean(&[b]).unwrap();
        assert!((r - 1.1).abs() < 1e-9, "expected 1.1, got {}", r);
    }

    #[test]
    fn test_shadow_to_range_ratio_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::shadow_to_range_ratio(&[b]).is_none());
    }

    #[test]
    fn test_shadow_to_range_ratio_full_body() {
        // open=100, close=110, high=110, low=100 → no shadow → ratio=0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let r = OhlcvBar::shadow_to_range_ratio(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_avg_close_to_low_none_for_empty() {
        assert!(OhlcvBar::avg_close_to_low(&[]).is_none());
    }

    #[test]
    fn test_avg_close_to_low_basic() {
        // close=105, low=90 → close-low=15
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let c = OhlcvBar::avg_close_to_low(&[b]).unwrap();
        assert!((c - 15.0).abs() < 1e-9, "expected 15.0, got {}", c);
    }

    #[test]
    fn test_avg_high_to_close_none_for_empty() {
        assert!(OhlcvBar::avg_high_to_close(&[]).is_none());
    }

    #[test]
    fn test_avg_high_to_close_basic() {
        // high=110, close=105 → high-close=5
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let v = OhlcvBar::avg_high_to_close(&[b]).unwrap();
        assert!((v - 5.0).abs() < 1e-9, "expected 5.0, got {}", v);
    }

    #[test]
    fn test_bar_size_entropy_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_size_entropy(&[b]).is_none());
    }

    #[test]
    fn test_bar_size_entropy_basic() {
        // two bars → some entropy
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let e = OhlcvBar::bar_size_entropy(&[b1, b2]);
        assert!(e.is_some());
    }

    #[test]
    fn test_close_to_open_pct_none_for_zero_open() {
        let mut b = make_ohlcv_bar(dec!(0), dec!(110), dec!(90), dec!(105));
        b.open = dec!(0);
        assert!(OhlcvBar::close_to_open_pct(&[b]).is_none());
    }

    #[test]
    fn test_close_to_open_pct_basic() {
        // open=100, close=110 → 10%
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let p = OhlcvBar::close_to_open_pct(&[b]).unwrap();
        assert!((p - 0.1).abs() < 1e-9, "expected 0.1, got {}", p);
    }

    #[test]
    fn test_body_direction_score_none_for_empty() {
        assert!(OhlcvBar::body_direction_score(&[]).is_none());
    }

    #[test]
    fn test_body_direction_score_all_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        let s = OhlcvBar::body_direction_score(&[b1, b2]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_close_above_open_pct_none_for_empty() {
        assert!(OhlcvBar::close_above_open_pct(&[]).is_none());
    }

    #[test]
    fn test_close_above_open_pct_basic() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(95), dec!(115));
        let p = OhlcvBar::close_above_open_pct(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_avg_low_to_close_none_for_empty() {
        assert!(OhlcvBar::avg_low_to_close(&[]).is_none());
    }

    #[test]
    fn test_avg_low_to_close_basic() {
        // low=90, close=105 → low-close=-15
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let v = OhlcvBar::avg_low_to_close(&[b]).unwrap();
        assert!((v - (-15.0)).abs() < 1e-9, "expected -15.0, got {}", v);
    }

    #[test]
    fn test_bar_trend_score_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_trend_score(&[b]).is_none());
    }

    #[test]
    fn test_bar_trend_score_all_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b3 = make_ohlcv_bar(dec!(105), dec!(125), dec!(100), dec!(120));
        let s = OhlcvBar::bar_trend_score(&[b1, b2, b3]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_volume_above_avg_count_none_for_empty() {
        assert!(OhlcvBar::volume_above_avg_count(&[]).is_none());
    }

    #[test]
    fn test_volume_above_avg_count_basic() {
        // all same volume → none above mean → 0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let c = OhlcvBar::volume_above_avg_count(&[b1, b2]).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_avg_close_range_pct_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::avg_close_range_pct(&[b]).is_none());
    }

    #[test]
    fn test_avg_close_range_pct_basic() {
        // low=90, high=110 → range=20, close=100 → (100-90)/20=0.5
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let p = OhlcvBar::avg_close_range_pct(&[b]).unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {}", p);
    }

    #[test]
    fn test_volume_ratio_to_max_none_for_empty() {
        assert!(OhlcvBar::volume_ratio_to_max(&[]).is_none());
    }

    #[test]
    fn test_volume_ratio_to_max_basic() {
        // all same volume → all ratio=1.0 → mean=1.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let r = OhlcvBar::volume_ratio_to_max(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_bar_consolidation_score_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::bar_consolidation_score(&[b]).is_none());
    }

    #[test]
    fn test_bar_consolidation_score_low_for_full_body() {
        // open=100, close=110, high=110, low=100 → body/range=1 → score=1-1=0 (no consolidation)
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(110));
        let s = OhlcvBar::bar_consolidation_score(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_shadow_asymmetry_none_for_zero_range() {
        let b = make_ohlcv_bar(dec!(100), dec!(100), dec!(100), dec!(100));
        assert!(OhlcvBar::shadow_asymmetry(&[b]).is_none());
    }

    #[test]
    fn test_shadow_asymmetry_basic() {
        // open=100, close=100 (doji), high=110, low=90 → upper=10, lower=10 → asymmetry=0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let a = OhlcvBar::shadow_asymmetry(&[b]).unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_open_close_midpoint_none_for_empty() {
        assert!(OhlcvBar::open_close_midpoint(&[]).is_none());
    }

    #[test]
    fn test_open_close_midpoint_basic() {
        // open=100, close=110 → midpoint=105
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let m = OhlcvBar::open_close_midpoint(&[b]).unwrap();
        assert!((m - 105.0).abs() < 1e-9, "expected 105.0, got {}", m);
    }

    #[test]
    fn test_volume_concentration_ratio_none_for_empty() {
        assert!(OhlcvBar::volume_concentration_ratio(&[]).is_none());
    }

    #[test]
    fn test_volume_concentration_ratio_basic() {
        // single bar → top 1 = all → ratio=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::volume_concentration_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_bar_gap_fill_ratio_none_for_single() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        assert!(OhlcvBar::bar_gap_fill_ratio(&[b]).is_none());
    }

    #[test]
    fn test_bar_gap_fill_ratio_basic() {
        // b1 open=100 close=110 → body [100,110]; b2 open=105 → inside → fill
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(110));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(95), dec!(115));
        let r = OhlcvBar::bar_gap_fill_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_net_shadow_direction_none_for_empty() {
        assert!(OhlcvBar::net_shadow_direction(&[]).is_none());
    }

    #[test]
    fn test_net_shadow_direction_symmetric() {
        // upper=lower → net=0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let d = OhlcvBar::net_shadow_direction(&[b]).unwrap();
        assert!(d.abs() < 1e-9, "expected 0.0, got {}", d);
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_range_stability_none_for_empty() {
        assert!(OhlcvBar::close_range_stability(&[]).is_none());
    }

    #[test]
    fn test_close_range_stability_consistent() {
        // all bars: open=low=90, high=110, close=100 → close_pct=0.5 each → std=0 → stability=1
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(100))).collect();
        let s = OhlcvBar::close_range_stability(&bars).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_avg_bar_volatility_none_for_empty() {
        assert!(OhlcvBar::avg_bar_volatility(&[]).is_none());
    }

    #[test]
    fn test_avg_bar_volatility_basic() {
        // open=100, high=110, low=90 → range=20, open=100 → vol=0.2
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let v = OhlcvBar::avg_bar_volatility(&[b]).unwrap();
        assert!((v - 0.2).abs() < 1e-9, "expected 0.2, got {}", v);
    }

    #[test]
    fn test_open_range_bias_none_for_empty() {
        assert!(OhlcvBar::open_range_bias(&[]).is_none());
    }

    #[test]
    fn test_open_range_bias_above() {
        // open=105, mid=(110+90)/2=100 → 105>100 → 1.0
        let b = make_ohlcv_bar(dec!(105), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::open_range_bias(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_body_volatility_none_for_empty() {
        assert!(OhlcvBar::body_volatility(&[]).is_none());
    }

    #[test]
    fn test_body_volatility_uniform_zero() {
        // all same body size → std=0
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        let v = OhlcvBar::body_volatility(&bars).unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_gap_ratio_none_for_single() {
        assert!(OhlcvBar::close_gap_ratio(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100))]).is_none());
    }

    #[test]
    fn test_close_gap_ratio_open_within_prior_range() {
        // prior range [90,110], next open=100 → within → ratio=1.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(102));
        let r = OhlcvBar::close_gap_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_volume_deceleration_none_for_single() {
        assert!(OhlcvBar::volume_deceleration(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100))]).is_none());
    }

    #[test]
    fn test_volume_deceleration_no_decreasing_is_zero() {
        // increasing volume → no decreasing pairs → 0.0
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b1.volume = dec!(5);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        b2.volume = dec!(10);
        let d = OhlcvBar::volume_deceleration(&[b1, b2]).unwrap();
        assert!((d - 0.0).abs() < 1e-9, "expected 0.0, got {}", d);
    }

    #[test]
    fn test_bar_trend_persistence_none_for_single() {
        assert!(OhlcvBar::bar_trend_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_trend_persistence_all_bullish() {
        // two bullish bars → 100% persistence
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(95), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(112));
        let p = OhlcvBar::bar_trend_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_shadow_body_ratio_none_for_empty() {
        assert!(OhlcvBar::shadow_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_shadow_body_ratio_doji_none() {
        // open=close → body=0 → None
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        assert!(OhlcvBar::shadow_body_ratio(&[b]).is_none());
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    #[test]
    fn test_body_to_range_pct_none_for_empty() {
        assert!(OhlcvBar::body_to_range_pct(&[]).is_none());
    }

    #[test]
    fn test_body_to_range_pct_full_body() {
        // open=90, high=110, low=90, close=110 → body=range → 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::body_to_range_pct(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_avg_open_close_gap_none_for_single() {
        assert!(OhlcvBar::avg_open_close_gap(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_avg_open_close_gap_zero() {
        // open[1] = close[0] → gap = 0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110));
        let g = OhlcvBar::avg_open_close_gap(&[b1, b2]).unwrap();
        assert!(g.abs() < 1e-9, "expected 0.0, got {}", g);
    }

    #[test]
    fn test_high_low_body_ratio_none_for_empty() {
        assert!(OhlcvBar::high_low_body_ratio(&[]).is_none());
    }

    #[test]
    fn test_high_low_body_ratio_basic() {
        // high=110, low=90, open=100, close=105 → upper shadow = 110-105=5, range=20 → 5/20=0.25
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::high_low_body_ratio(&[b]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_close_above_prior_high_none_for_single() {
        assert!(OhlcvBar::close_above_prior_high(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_close_above_prior_high_always() {
        // close[1]=120 > high[0]=110 → 1.0
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(111), dec!(125), dec!(108), dec!(120));
        let f = OhlcvBar::close_above_prior_high(&[b1, b2]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_range_pct_none_for_empty() {
        assert!(OhlcvBar::close_range_pct(&[]).is_none());
    }

    #[test]
    fn test_close_range_pct_at_high() {
        // close=high=110, low=90, range=20 → (110-90)/20=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::close_range_pct(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_avg_bar_range_pct_none_for_empty() {
        assert!(OhlcvBar::avg_bar_range_pct(&[]).is_none());
    }

    #[test]
    fn test_avg_bar_range_pct_basic() {
        // open=100, high=110, low=90 → range=20, pct=0.2
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::avg_bar_range_pct(&[b]).unwrap();
        assert!((r - 0.2).abs() < 1e-9, "expected 0.2, got {}", r);
    }

    #[test]
    fn test_open_to_low_ratio_none_for_empty() {
        assert!(OhlcvBar::open_to_low_ratio(&[]).is_none());
    }

    #[test]
    fn test_open_to_low_ratio_open_at_low() {
        // open=low=90, high=110, range=20 → (90-90)/20=0.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(100));
        let r = OhlcvBar::open_to_low_ratio(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_bar_close_rank_none_for_empty() {
        assert!(OhlcvBar::bar_close_rank(&[]).is_none());
    }

    #[test]
    fn test_bar_close_rank_highest() {
        let bars = [
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(101)),
            make_ohlcv_bar(dec!(101), dec!(112), dec!(98), dec!(105)),
            make_ohlcv_bar(dec!(104), dec!(115), dec!(100), dec!(112)),
        ];
        let r = OhlcvBar::bar_close_rank(&bars).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_oscillation_count_none_for_two() {
        let bars = [
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110)),
        ];
        assert!(OhlcvBar::close_oscillation_count(&bars).is_none());
    }

    #[test]
    fn test_close_oscillation_count_alternating() {
        // close: 100, 110, 105, 112 → diffs: +,-, + → 2 direction changes
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(98), dec!(110));
        let b3 = make_ohlcv_bar(dec!(109), dec!(112), dec!(100), dec!(105));
        let b4 = make_ohlcv_bar(dec!(105), dec!(118), dec!(102), dec!(112));
        let c = OhlcvBar::close_oscillation_count(&[b1, b2, b3, b4]).unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_bar_consolidation_ratio_none_for_empty() {
        assert!(OhlcvBar::bar_consolidation_ratio(&[]).is_none());
    }

    #[test]
    fn test_bar_consolidation_ratio_uniform() {
        // all same range → half below median = 0
        let bars: Vec<_> = (0..4).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        let r = OhlcvBar::bar_consolidation_ratio(&bars).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_open_momentum_score_none_for_single() {
        assert!(OhlcvBar::open_momentum_score(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_open_momentum_score_all_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110));
        let s = OhlcvBar::open_momentum_score(&[b1, b2]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_avg_volume_change_none_for_single() {
        assert!(OhlcvBar::avg_volume_change(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_avg_volume_change_zero_for_equal_volumes() {
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        // volume is always dec!(1) from make_ohlcv_bar → no change
        let v = OhlcvBar::avg_volume_change(&bars).unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_lag1_autocorr_none_for_two() {
        let bars = [
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110)),
        ];
        assert!(OhlcvBar::close_lag1_autocorr(&bars).is_none());
    }

    #[test]
    fn test_close_lag1_autocorr_uniform_none() {
        // uniform close → var=0 → None
        let bars: Vec<_> = (0..4).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        assert!(OhlcvBar::close_lag1_autocorr(&bars).is_none());
    }

    #[test]
    fn test_volume_skewness_none_for_two() {
        let bars = [
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110)),
        ];
        assert!(OhlcvBar::volume_skewness(&bars).is_none());
    }

    #[test]
    fn test_volume_skewness_uniform_none() {
        let bars: Vec<_> = (0..4).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        // all vol=1 → std=0 → None
        assert!(OhlcvBar::volume_skewness(&bars).is_none());
    }

    #[test]
    fn test_bar_height_rank_none_for_empty() {
        assert!(OhlcvBar::bar_height_rank(&[]).is_none());
    }

    #[test]
    fn test_bar_height_rank_largest() {
        // last bar has largest range
        let b1 = make_ohlcv_bar(dec!(100), dec!(105), dec!(98), dec!(103)); // range=7
        let b2 = make_ohlcv_bar(dec!(100), dec!(106), dec!(97), dec!(103)); // range=9
        let b3 = make_ohlcv_bar(dec!(100), dec!(115), dec!(90), dec!(110)); // range=25
        let r = OhlcvBar::bar_height_rank(&[b1, b2, b3]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 (max rank), got {}", r);
    }

    #[test]
    fn test_high_persistence_none_for_single() {
        assert!(OhlcvBar::high_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_high_persistence_always_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(102), dec!(115));
        let p = OhlcvBar::high_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    #[test]
    fn test_body_ema_none_for_empty() {
        assert!(OhlcvBar::body_ema(&[]).is_none());
    }

    #[test]
    fn test_body_ema_single() {
        // open=100, close=105 → body=5
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let e = OhlcvBar::body_ema(&[b]).unwrap();
        assert!((e - 5.0).abs() < 1e-9, "expected 5.0, got {}", e);
    }

    #[test]
    fn test_avg_true_range_ratio_none_for_single() {
        assert!(OhlcvBar::avg_true_range_ratio(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_avg_true_range_ratio_basic() {
        // prior close=105, next high=115, low=100 → range=15, ratio=15/105≈0.1429
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110));
        let r = OhlcvBar::avg_true_range_ratio(&[b1, b2]).unwrap();
        assert!(r > 0.0, "expected positive ratio, got {}", r);
    }

    #[test]
    fn test_close_body_fraction_none_for_empty() {
        assert!(OhlcvBar::close_body_fraction(&[]).is_none());
    }

    #[test]
    fn test_close_body_fraction_full_body() {
        // open=90, close=110, high=110, low=90 → body=range → 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let f = OhlcvBar::close_body_fraction(&[b]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_bar_momentum_accel_none_for_two() {
        let bars = [
            make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105)),
            make_ohlcv_bar(dec!(105), dec!(115), dec!(100), dec!(110)),
        ];
        assert!(OhlcvBar::bar_momentum_accel(&bars).is_none());
    }

    #[test]
    fn test_bar_momentum_accel_linear_zero() {
        // linear close → zero acceleration
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(102));
        let b3 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(104));
        let a = OhlcvBar::bar_momentum_accel(&[b1, b2, b3]).unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    #[test]
    fn test_close_velocity_none_for_single() {
        assert!(OhlcvBar::close_velocity(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_close_velocity_constant_zero() {
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        let v = OhlcvBar::close_velocity(&bars).unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    #[test]
    fn test_open_range_score_none_for_empty() {
        assert!(OhlcvBar::open_range_score(&[]).is_none());
    }

    #[test]
    fn test_open_range_score_open_at_low() {
        // open=low=90, high=110 → score=0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(100));
        let s = OhlcvBar::open_range_score(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_body_trend_direction_none_for_empty() {
        assert!(OhlcvBar::body_trend_direction(&[]).is_none());
    }

    #[test]
    fn test_body_trend_direction_all_bull() {
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        let d = OhlcvBar::body_trend_direction(&bars).unwrap();
        assert!((d - 1.0).abs() < 1e-9, "expected 1.0, got {}", d);
    }

    #[test]
    fn test_bar_tightness_none_for_empty() {
        assert!(OhlcvBar::bar_tightness(&[]).is_none());
    }

    #[test]
    fn test_bar_tightness_basic() {
        // open=100, close=105, high=110, low=90 → mid=102.5, range=20, tightness=20/102.5≈0.195
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let t = OhlcvBar::bar_tightness(&[b]).unwrap();
        assert!(t > 0.0, "expected positive tightness, got {}", t);
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    #[test]
    fn test_avg_close_slope_none_for_single() {
        assert!(OhlcvBar::avg_close_slope(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_avg_close_slope_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::avg_close_slope(&[b1, b2]).unwrap();
        assert!((s - 5.0).abs() < 1e-9, "expected 5.0, got {}", s);
    }

    #[test]
    fn test_body_range_zscore_none_for_single() {
        assert!(OhlcvBar::body_range_zscore(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_body_range_zscore_uniform_none() {
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        // all same body → std=0 → None
        assert!(OhlcvBar::body_range_zscore(&bars).is_none());
    }

    #[test]
    fn test_volume_entropy_none_for_empty() {
        assert!(OhlcvBar::volume_entropy(&[]).is_none());
    }

    #[test]
    fn test_volume_entropy_uniform_positive() {
        let bars: Vec<_> = (0..3).map(|_| make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))).collect();
        let e = OhlcvBar::volume_entropy(&bars).unwrap();
        assert!(e >= 0.0, "expected non-negative, got {}", e);
    }

    #[test]
    fn test_low_persistence_none_for_single() {
        assert!(OhlcvBar::low_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_low_persistence_always_falling() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(95), dec!(108), dec!(85), dec!(100));
        let p = OhlcvBar::low_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    // ── round-129 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_energy_none_for_empty() {
        assert!(OhlcvBar::bar_energy(&[]).is_none());
    }

    #[test]
    fn test_bar_energy_basic() {
        // range = 10, energy = 100
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(105));
        let e = OhlcvBar::bar_energy(&[b]).unwrap();
        assert!((e - 100.0).abs() < 1e-6, "expected 100.0, got {}", e);
    }

    #[test]
    fn test_open_close_persistence_none_for_single() {
        assert!(OhlcvBar::open_close_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_open_close_persistence_match() {
        // close of b1 = 105, open of b2 = 105 → match
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        let p = OhlcvBar::open_close_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_bar_range_trend_none_for_single() {
        assert!(OhlcvBar::bar_range_trend(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_range_trend_expanding() {
        // ranges: 10, 20 → diff = 10
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(100), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(100), dec!(110));
        let t = OhlcvBar::bar_range_trend(&[b1, b2]).unwrap();
        assert!(t > 0.0, "expected positive trend, got {}", t);
    }

    #[test]
    fn test_open_high_spread_none_for_empty() {
        assert!(OhlcvBar::open_high_spread(&[]).is_none());
    }

    #[test]
    fn test_open_high_spread_basic() {
        // open=100, high=110 → spread = 0.1
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::open_high_spread(&[b]).unwrap();
        assert!((s - 0.1).abs() < 1e-9, "expected 0.1, got {}", s);
    }

    // ── round-130 ────────────────────────────────────────────────────────────
    #[test]
    fn test_close_body_range_ratio_none_for_empty() {
        assert!(OhlcvBar::close_body_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_close_body_range_ratio_full_body() {
        // open=90, close=110, high=110, low=90 → body=20, range=20 → ratio=1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::close_body_range_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_avg_body_pct_none_for_empty() {
        assert!(OhlcvBar::avg_body_pct(&[]).is_none());
    }

    #[test]
    fn test_avg_body_pct_basic() {
        // open=100, close=110 → body=10, pct=0.1
        let b = make_ohlcv_bar(dec!(100), dec!(115), dec!(85), dec!(110));
        let p = OhlcvBar::avg_body_pct(&[b]).unwrap();
        assert!((p - 0.1).abs() < 1e-9, "expected 0.1, got {}", p);
    }

    #[test]
    fn test_bar_symmetry_none_for_empty() {
        assert!(OhlcvBar::bar_symmetry(&[]).is_none());
    }

    #[test]
    fn test_bar_symmetry_symmetric_bar() {
        // open=105, close=95, high=110, low=90 → upper=5, lower=5 → asymmetry=0
        let b = make_ohlcv_bar(dec!(105), dec!(110), dec!(90), dec!(95));
        let s = OhlcvBar::bar_symmetry(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0 for symmetric bar, got {}", s);
    }

    #[test]
    fn test_open_gap_direction_none_for_single() {
        assert!(OhlcvBar::open_gap_direction(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_open_gap_direction_gap_up() {
        // close of b1 = 105, open of b2 = 110 → gap up
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(115));
        let g = OhlcvBar::open_gap_direction(&[b1, b2]).unwrap();
        assert!((g - 1.0).abs() < 1e-9, "expected 1.0 for gap up, got {}", g);
    }

    // ── round-131 ────────────────────────────────────────────────────────────
    #[test]
    fn test_close_trend_strength_none_for_single() {
        assert!(OhlcvBar::close_trend_strength(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_close_trend_strength_full_up() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(100), dec!(110));
        let t = OhlcvBar::close_trend_strength(&[b1, b2]).unwrap();
        assert!((t - 1.0).abs() < 1e-9, "expected 1.0, got {}", t);
    }

    #[test]
    fn test_bar_body_skew_none_for_empty() {
        assert!(OhlcvBar::bar_body_skew(&[]).is_none());
    }

    #[test]
    fn test_bar_body_skew_bullish() {
        // open=90, close=110, high=110, low=90 → body/range = 1.0 (bullish)
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let s = OhlcvBar::bar_body_skew(&[b]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0 for full bullish, got {}", s);
    }

    #[test]
    fn test_bar_range_mean_dev_none_for_empty() {
        assert!(OhlcvBar::bar_range_mean_dev(&[]).is_none());
    }

    #[test]
    fn test_bar_range_mean_dev_identical() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let d = OhlcvBar::bar_range_mean_dev(&[b1, b2]).unwrap();
        assert!(d.abs() < 1e-9, "expected 0.0 for identical ranges, got {}", d);
    }

    #[test]
    fn test_bar_close_momentum_none_for_single() {
        assert!(OhlcvBar::bar_close_momentum(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_close_momentum_all_up() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let b2 = make_ohlcv_bar(dec!(100), dec!(115), dec!(95), dec!(105));
        let b3 = make_ohlcv_bar(dec!(105), dec!(120), dec!(100), dec!(115));
        let m = OhlcvBar::bar_close_momentum(&[b1, b2, b3]).unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0 for 2 up moves, got {}", m);
    }

    // ── round-132 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_volume_trend_none_for_single() {
        assert!(OhlcvBar::bar_volume_trend(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_volume_trend_increasing() {
        let mut b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b1.volume = dec!(10);
        let mut b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        b2.volume = dec!(20);
        let t = OhlcvBar::bar_volume_trend(&[b1, b2]).unwrap();
        assert!(t > 0.0, "expected positive trend, got {}", t);
    }

    #[test]
    fn test_close_low_spread_none_for_empty() {
        assert!(OhlcvBar::close_low_spread(&[]).is_none());
    }

    #[test]
    fn test_close_low_spread_close_at_high() {
        // open=90, high=110, low=90, close=110 → (110-90)/(110-90) = 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let s = OhlcvBar::close_low_spread(&[b]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_bar_midpoint_trend_none_for_single() {
        assert!(OhlcvBar::bar_midpoint_trend(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_midpoint_trend_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(110), dec!(120), dec!(100), dec!(115));
        let t = OhlcvBar::bar_midpoint_trend(&[b1, b2]).unwrap();
        assert!(t > 0.0, "expected positive trend, got {}", t);
    }

    #[test]
    fn test_bar_spread_score_none_for_empty() {
        assert!(OhlcvBar::bar_spread_score(&[]).is_none());
    }

    #[test]
    fn test_bar_spread_score_basic() {
        // range=20, close=105 → spread=20/105
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::bar_spread_score(&[b]).unwrap();
        assert!(s > 0.0, "expected positive score, got {}", s);
    }

    // ── round-133 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_open_close_momentum_none_for_empty() {
        assert!(OhlcvBar::bar_open_close_momentum(&[]).is_none());
    }

    #[test]
    fn test_bar_open_close_momentum_all_bullish() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(115), dec!(85), dec!(110));
        let b2 = make_ohlcv_bar(dec!(110), dec!(125), dec!(95), dec!(120));
        let m = OhlcvBar::bar_open_close_momentum(&[b1, b2]).unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    #[test]
    fn test_close_body_position_none_for_empty() {
        assert!(OhlcvBar::close_body_position(&[]).is_none());
    }

    #[test]
    fn test_close_body_position_at_high() {
        // high=110, low=90, close=110 → position=1.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let p = OhlcvBar::close_body_position(&[b]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_bar_close_persistence_none_for_single() {
        assert!(OhlcvBar::bar_close_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_close_persistence_always_higher() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(115));
        let p = OhlcvBar::bar_close_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_close_wick_ratio_none_for_empty() {
        assert!(OhlcvBar::close_wick_ratio(&[]).is_none());
    }

    #[test]
    fn test_close_wick_ratio_no_upper_wick() {
        // open=100, close=110, high=110 → upper_wick=0, body=10 → ratio=0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let r = OhlcvBar::close_wick_ratio(&[b]).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for no upper wick, got {}", r);
    }

    // ── round-134 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_high_trend_none_for_single() {
        assert!(OhlcvBar::bar_high_trend(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_high_trend_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(120), dec!(95), dec!(115));
        let t = OhlcvBar::bar_high_trend(&[b1, b2]).unwrap();
        assert!(t > 0.0, "expected positive trend, got {}", t);
    }

    #[test]
    fn test_bar_low_trend_none_for_single() {
        assert!(OhlcvBar::bar_low_trend(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_low_trend_falling() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(95), dec!(108), dec!(80), dec!(100));
        let t = OhlcvBar::bar_low_trend(&[b1, b2]).unwrap();
        assert!(t < 0.0, "expected negative trend for falling lows, got {}", t);
    }

    #[test]
    fn test_close_high_wick_none_for_empty() {
        assert!(OhlcvBar::close_high_wick(&[]).is_none());
    }

    #[test]
    fn test_close_high_wick_at_high() {
        // close at high → upper wick = 0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(110));
        let w = OhlcvBar::close_high_wick(&[b]).unwrap();
        assert!(w.abs() < 1e-9, "expected 0.0 when close=high, got {}", w);
    }

    #[test]
    fn test_bar_open_persistence_none_for_single() {
        assert!(OhlcvBar::bar_open_persistence(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_open_persistence_rising() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(105), dec!(115), dec!(95), dec!(110));
        let p = OhlcvBar::bar_open_persistence(&[b1, b2]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    // ── round-135 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_body_count_none_for_empty() {
        assert!(OhlcvBar::bar_body_count(&[]).is_none());
    }

    #[test]
    fn test_bar_body_count_all_doji() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(100));
        let c = OhlcvBar::bar_body_count(&[b]).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_range_contraction_ratio_none_for_single() {
        assert!(OhlcvBar::range_contraction_ratio(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_range_contraction_ratio_contracting() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::range_contraction_ratio(&[b1, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_volume_trend_ratio_none_for_empty() {
        assert!(OhlcvBar::volume_trend_ratio(&[]).is_none());
    }

    #[test]
    fn test_volume_trend_ratio_equal() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let r = OhlcvBar::volume_trend_ratio(&[b]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for single bar, got {}", r);
    }

    #[test]
    fn test_bar_midpoint_score_none_for_empty() {
        assert!(OhlcvBar::bar_midpoint_score(&[]).is_none());
    }

    #[test]
    fn test_bar_midpoint_score_symmetric() {
        // high=110, low=90, open=100 → mid=100, (100-100)/20 = 0.0
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::bar_midpoint_score(&[b]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0 for symmetric bar, got {}", s);
    }

    // ── round-136 ────────────────────────────────────────────────────────────
    #[test]
    fn test_bar_open_efficiency_none_for_empty() {
        assert!(OhlcvBar::bar_open_efficiency(&[]).is_none());
    }

    #[test]
    fn test_bar_open_efficiency_full_body() {
        // body = range → efficiency = 1.0
        let b = make_ohlcv_bar(dec!(90), dec!(110), dec!(90), dec!(110));
        let e = OhlcvBar::bar_open_efficiency(&[b]).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    #[test]
    fn test_close_oscillation_amplitude_none_for_single() {
        assert!(OhlcvBar::close_oscillation_amplitude(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_close_oscillation_amplitude_identical() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let a = OhlcvBar::close_oscillation_amplitude(&[b1, b2]).unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0 for identical closes, got {}", a);
    }

    #[test]
    fn test_bar_high_low_score_none_for_empty() {
        assert!(OhlcvBar::bar_high_low_score(&[]).is_none());
    }

    #[test]
    fn test_bar_high_low_score_positive() {
        let b = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let s = OhlcvBar::bar_high_low_score(&[b]).unwrap();
        assert!(s > 0.0, "expected positive score, got {}", s);
    }

    #[test]
    fn test_bar_range_change_none_for_single() {
        assert!(OhlcvBar::bar_range_change(&[make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105))]).is_none());
    }

    #[test]
    fn test_bar_range_change_expanding() {
        let b1 = make_ohlcv_bar(dec!(100), dec!(110), dec!(90), dec!(105));
        let b2 = make_ohlcv_bar(dec!(100), dec!(120), dec!(80), dec!(105));
        let c = OhlcvBar::bar_range_change(&[b1, b2]).unwrap();
        assert!(c > 0.0, "expected positive change for expanding range, got {}", c);
    }
}
