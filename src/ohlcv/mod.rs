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

impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Timeframe::Seconds(s) => write!(f, "{s}s"),
            Timeframe::Minutes(m) => write!(f, "{m}m"),
            Timeframe::Hours(h) => write!(f, "{h}h"),
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
        self.high - self.open.max(self.close)
    }

    /// Lower wick (shadow) length: `min(open, close) - low`.
    ///
    /// The lower wick is the portion of the candle below the body.
    pub fn wick_lower(&self) -> Decimal {
        self.open.min(self.close) - self.low
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
        let pct = (self.close - self.open) / self.open * Decimal::from(100);
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
        let hl = self.high - self.low;
        let hpc = (self.high - prev_close).abs();
        let lpc = (self.low - prev_close).abs();
        hl.max(hpc).max(lpc)
    }

    /// Returns `true` if this bar is an inside bar relative to `prev`.
    ///
    /// An inside bar has `high < prev.high` and `low > prev.low` — its full
    /// range is contained within the prior bar's range. Used in price action
    /// trading as a consolidation signal.
    pub fn inside_bar(&self, prev: &OhlcvBar) -> bool {
        self.high < prev.high && self.low > prev.low
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

    /// Absolute price change over the bar: `|close − open|`.
    pub fn price_change_abs(&self) -> Decimal {
        (self.close - self.open).abs()
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
        let self_lo = self.open.min(self.close);
        let self_hi = self.open.max(self.close);
        let prev_lo = prev.open.min(prev.close);
        let prev_hi = prev.open.max(prev.close);
        self_lo < prev_lo && self_hi > prev_hi
    }

    /// Returns `true` if this bar is a harami: its body is entirely contained
    /// within the previous bar's body.
    ///
    /// A harami is the opposite of an engulfing pattern. Neither bar needs to
    /// be bullish or bearish — only the body ranges are compared.
    pub fn is_harami(&self, prev: &OhlcvBar) -> bool {
        let self_lo = self.open.min(self.close);
        let self_hi = self.open.max(self.close);
        let prev_lo = prev.open.min(prev.close);
        let prev_hi = prev.open.max(prev.close);
        self_lo > prev_lo && self_hi < prev_hi
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
    pub fn body_size(&self) -> Decimal {
        (self.close - self.open).abs()
    }

    /// High-low range as a percentage of the open price: `(high - low) / open * 100`.
    ///
    /// Returns `None` if open is zero.
    pub fn range_pct(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() {
            return None;
        }
        let range = (self.high - self.low) / self.open;
        range.to_f64().map(|v| v * 100.0)
    }

    /// Returns `true` if this bar is an outside bar (engulfs `prev`'s range).
    ///
    /// An outside bar has a higher high AND lower low than the previous bar.
    pub fn is_outside_bar(&self, prev: &OhlcvBar) -> bool {
        self.high > prev.high && self.low < prev.low
    }

    /// Midpoint of the high-low range: `(high + low) / 2`.
    pub fn high_low_midpoint(&self) -> Decimal {
        (self.high + self.low) / Decimal::TWO
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
    pub fn is_flat(&self) -> bool {
        self.open == self.close && self.high == self.low && self.open == self.high
    }

    /// True range: `max(high - low, |high - prev_close|, |low - prev_close|)`.
    ///
    /// This is the ATR building block. Without a previous close, returns `high - low`.
    pub fn true_range_with_prev(&self, prev_close: Decimal) -> Decimal {
        let hl = self.high - self.low;
        let hc = (self.high - prev_close).abs();
        let lc = (self.low - prev_close).abs();
        hl.max(hc).max(lc)
    }

    /// Returns the ratio of close to high, or `None` if high is zero.
    pub fn close_to_high_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.high.is_zero() { return None; }
        (self.close / self.high).to_f64()
    }

    /// Returns the ratio of close to open, or `None` if open is zero.
    pub fn close_open_ratio(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if self.open.is_zero() { return None; }
        (self.close / self.open).to_f64()
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
                        vwap: None,
                    });
                    gap_start += dur;
                }
            }
        }

        // Update price_volume_sum before the match to avoid borrow conflicts.
        let tick_value = tick.price * tick.quantity;
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
        assert!(curr.inside_bar(&prev));
    }

    #[test]
    fn test_inside_bar_false_when_not_contained() {
        let prev = make_bar(dec!(9), dec!(15), dec!(5), dec!(12));
        let curr = make_bar(dec!(10), dec!(16), dec!(6), dec!(11));
        assert!(!curr.inside_bar(&prev));
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
}
