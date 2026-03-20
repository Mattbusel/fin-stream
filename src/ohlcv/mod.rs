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
}
