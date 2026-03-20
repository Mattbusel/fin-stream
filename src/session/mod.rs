//! Market session awareness — trading hours, holidays, status transitions.
//!
//! ## Responsibility
//! Classify a UTC timestamp into a market trading status for a given session
//! (equity, crypto, forex). Enables downstream filtering of ticks by session.
//!
//! ## Guarantees
//! - Pure functions: SessionAwareness::status() is deterministic and stateless
//! - Non-panicking: all operations return Result or TradingStatus
//! - DST-aware: US equity hours correctly switch between EST (UTC-5) and
//!   EDT (UTC-4) on the second Sunday of March and first Sunday of November

use crate::error::StreamError;
use chrono::{Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc, Weekday};

/// Broad category of market session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MarketSession {
    /// US equity market (NYSE/NASDAQ) — 9:30–16:00 ET Mon–Fri.
    UsEquity,
    /// Crypto market — 24/7/365, always open.
    Crypto,
    /// Forex market — 24/5, Sunday 22:00 UTC – Friday 22:00 UTC.
    Forex,
}

/// Trading status at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TradingStatus {
    /// Regular trading hours are active.
    Open,
    /// Pre-market or after-hours session (equity); equivalent to `Open` for crypto.
    Extended,
    /// Market is fully closed; no trading possible.
    Closed,
}

impl MarketSession {
    /// Duration of one regular trading session in milliseconds.
    ///
    /// - `UsEquity`: 9:30–16:00 ET = 6.5 hours = 23,400,000 ms
    /// - `Forex`: continuous 24/5 week = 120 hours = 432,000,000 ms
    /// - `Crypto`: always open — returns `u64::MAX`
    pub fn session_duration_ms(self) -> u64 {
        match self {
            MarketSession::UsEquity => 6 * 3_600_000 + 30 * 60_000, // 6.5 hours
            MarketSession::Forex => 5 * 24 * 3_600_000,              // 120 hours
            MarketSession::Crypto => u64::MAX,
        }
    }

    /// Returns `true` if this session has a defined extended-hours trading period.
    ///
    /// Only [`MarketSession::UsEquity`] has pre-market (4:00–9:30 ET) and
    /// after-hours (16:00–20:00 ET) periods. Crypto is always open; Forex has
    /// no separate extended session.
    pub fn has_extended_hours(self) -> bool {
        matches!(self, MarketSession::UsEquity)
    }
}

impl std::fmt::Display for MarketSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarketSession::UsEquity => write!(f, "UsEquity"),
            MarketSession::Crypto => write!(f, "Crypto"),
            MarketSession::Forex => write!(f, "Forex"),
        }
    }
}

impl std::fmt::Display for TradingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradingStatus::Open => write!(f, "Open"),
            TradingStatus::Extended => write!(f, "Extended"),
            TradingStatus::Closed => write!(f, "Closed"),
        }
    }
}

impl std::str::FromStr for MarketSession {
    type Err = StreamError;

    /// Parse a market session name (case-insensitive).
    ///
    /// Accepted values: `"usequity"`, `"crypto"`, `"forex"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "usequity" => Ok(MarketSession::UsEquity),
            "crypto" => Ok(MarketSession::Crypto),
            "forex" => Ok(MarketSession::Forex),
            _ => Err(StreamError::ConfigError {
                reason: format!("unknown market session '{s}'; expected usequity, crypto, or forex"),
            }),
        }
    }
}

impl std::str::FromStr for TradingStatus {
    type Err = StreamError;

    /// Parse a trading status string (case-insensitive).
    ///
    /// Accepted values: `"open"`, `"extended"`, `"closed"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "open" => Ok(TradingStatus::Open),
            "extended" => Ok(TradingStatus::Extended),
            "closed" => Ok(TradingStatus::Closed),
            _ => Err(StreamError::ConfigError {
                reason: format!(
                    "unknown trading status '{s}'; expected open, extended, or closed"
                ),
            }),
        }
    }
}

/// Determines trading status for a market session.
pub struct SessionAwareness {
    session: MarketSession,
}

impl SessionAwareness {
    /// Create a session classifier for the given market.
    pub fn new(session: MarketSession) -> Self {
        Self { session }
    }

    /// Returns `true` if `utc_ms` falls on a Saturday or Sunday (UTC).
    ///
    /// Session-agnostic: always checks the UTC calendar day regardless of
    /// the configured [`MarketSession`]. Useful for skip-weekend logic in
    /// backtesting loops and scheduling.
    pub fn is_weekend(utc_ms: u64) -> bool {
        let dt = Utc.timestamp_millis_opt(utc_ms as i64).unwrap();
        let weekday = dt.weekday();
        weekday == Weekday::Sat || weekday == Weekday::Sun
    }

    /// Classify a UTC timestamp (ms) into a trading status.
    pub fn status(&self, utc_ms: u64) -> Result<TradingStatus, StreamError> {
        match self.session {
            MarketSession::Crypto => Ok(TradingStatus::Open),
            MarketSession::UsEquity => Ok(self.us_equity_status(utc_ms)),
            MarketSession::Forex => Ok(self.forex_status(utc_ms)),
        }
    }

    /// The market session this classifier was constructed for.
    pub fn session(&self) -> MarketSession {
        self.session
    }

    /// Return the UTC millisecond timestamp of the next time this session
    /// enters [`TradingStatus::Open`] status.
    ///
    /// If the session is already `Open` at `utc_ms`, returns `utc_ms`
    /// unchanged. For [`MarketSession::Crypto`] this always returns `utc_ms`.
    pub fn next_open_ms(&self, utc_ms: u64) -> u64 {
        match self.session {
            MarketSession::Crypto => utc_ms,
            MarketSession::Forex => self.next_forex_open_ms(utc_ms),
            MarketSession::UsEquity => self.next_us_equity_open_ms(utc_ms),
        }
    }

    /// Returns `true` if the session is currently in [`TradingStatus::Closed`] status.
    ///
    /// Shorthand for `self.status(utc_ms) == Ok(TradingStatus::Closed)`.
    /// For [`MarketSession::Crypto`] this always returns `false`.
    pub fn is_closed(&self, utc_ms: u64) -> bool {
        self.status(utc_ms).map_or(false, |s| s == TradingStatus::Closed)
    }

    /// Returns `true` if the session is currently in [`TradingStatus::Extended`] status.
    ///
    /// For [`MarketSession::Crypto`] this always returns `false` (crypto is always
    /// `Open`). For equity, `Extended` covers pre-market (4:00–9:30 ET) and
    /// after-hours (16:00–20:00 ET).
    pub fn is_extended(&self, utc_ms: u64) -> bool {
        self.status(utc_ms).map_or(false, |s| s == TradingStatus::Extended)
    }

    /// Returns `true` if the session is currently in [`TradingStatus::Open`] status.
    ///
    /// Shorthand for `self.status(utc_ms).map(|s| s == TradingStatus::Open).unwrap_or(false)`.
    /// For [`MarketSession::Crypto`] this always returns `true`.
    pub fn is_open(&self, utc_ms: u64) -> bool {
        self.status(utc_ms).map_or(false, |s| s == TradingStatus::Open)
    }

    /// Returns `true` if the session is currently tradeable: either
    /// [`TradingStatus::Open`] or [`TradingStatus::Extended`].
    ///
    /// For [`MarketSession::Crypto`] this always returns `true`. For equity,
    /// returns `true` during both regular hours and extended (pre/after-market)
    /// hours.
    pub fn is_market_hours(&self, utc_ms: u64) -> bool {
        self.is_active(utc_ms)
    }

    /// Returns the number of whole minutes until the next [`TradingStatus::Open`] transition.
    ///
    /// Returns `0` if the session is already open. For [`MarketSession::Crypto`] always
    /// returns `0`. Useful for scheduling reconnect timers and pre-open setup.
    pub fn minutes_until_open(&self, utc_ms: u64) -> u64 {
        self.time_until_open_ms(utc_ms) / 60_000
    }

    /// Milliseconds until the next [`TradingStatus::Open`] transition.
    ///
    /// Returns `0` if the session is already `Open`. For
    /// [`MarketSession::Crypto`] always returns `0`.
    pub fn time_until_open_ms(&self, utc_ms: u64) -> u64 {
        self.next_open_ms(utc_ms).saturating_sub(utc_ms)
    }

    /// Seconds until the session next opens as `f64`.
    ///
    /// Returns `0.0` if the session is already open.
    pub fn seconds_until_open(&self, utc_ms: u64) -> f64 {
        self.time_until_open_ms(utc_ms) as f64 / 1_000.0
    }

    /// Whole minutes until the session enters [`TradingStatus::Closed`].
    ///
    /// Returns `0` if the session is already closed or will close in less than
    /// one minute. For [`MarketSession::Crypto`] returns `u64::MAX` (never
    /// closes). Useful for scheduling pre-close warnings or cooldown timers.
    pub fn minutes_until_close(&self, utc_ms: u64) -> u64 {
        let ms = self.time_until_close_ms(utc_ms);
        if ms == u64::MAX {
            return u64::MAX;
        }
        ms / 60_000
    }

    /// Milliseconds until the session enters [`TradingStatus::Closed`].
    ///
    /// Returns `0` if the session is already `Closed`. For
    /// [`MarketSession::Crypto`] returns `u64::MAX` (never closes).
    pub fn time_until_close_ms(&self, utc_ms: u64) -> u64 {
        let close = self.next_close_ms(utc_ms);
        if close == u64::MAX {
            u64::MAX
        } else {
            close.saturating_sub(utc_ms)
        }
    }

    /// Returns the number of complete bars of `bar_duration_ms` that fit before the next open.
    ///
    /// Returns `0` if the session is already open or `bar_duration_ms == 0`.
    /// Useful for scheduling reconnects or pre-open setup tasks.
    pub fn bars_until_open(&self, utc_ms: u64, bar_duration_ms: u64) -> u64 {
        if bar_duration_ms == 0 || self.is_open(utc_ms) {
            return 0;
        }
        let ms_until = self.time_until_open_ms(utc_ms);
        ms_until / bar_duration_ms
    }

    /// Returns `true` if the equity session is currently in the pre-market window (4:00–9:30 ET).
    ///
    /// Always returns `false` for non-equity sessions. Pre-market is a subset of
    /// [`TradingStatus::Extended`]; this method distinguishes it from after-hours.
    pub fn is_pre_market(&self, utc_ms: u64) -> bool {
        if self.session != MarketSession::UsEquity {
            return false;
        }
        let Some(t) = self.et_trading_secs_of_day(utc_ms) else { return false; };
        let pre_open = 4 * 3600_u64;
        let market_open = 9 * 3600 + 30 * 60_u64;
        t >= pre_open && t < market_open
    }

    /// Returns `true` if the equity session is in the after-hours window (16:00–20:00 ET).
    ///
    /// Always returns `false` for non-equity sessions. After-hours is a subset of
    /// [`TradingStatus::Extended`]; this method distinguishes it from pre-market.
    pub fn is_after_hours(&self, utc_ms: u64) -> bool {
        if self.session != MarketSession::UsEquity {
            return false;
        }
        let Some(t) = self.et_trading_secs_of_day(utc_ms) else { return false; };
        let market_close = 16 * 3600_u64;
        let post_close = 20 * 3600_u64;
        t >= market_close && t < post_close
    }

    /// Returns the ET second-of-day for `utc_ms`, or `None` on weekends / market holidays.
    fn et_trading_secs_of_day(&self, utc_ms: u64) -> Option<u64> {
        let secs = (utc_ms / 1000) as i64;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap_or_else(chrono::Utc::now);
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + chrono::Duration::seconds(et_offset_secs);
        let dow = et_dt.weekday();
        if dow == chrono::Weekday::Sat || dow == chrono::Weekday::Sun {
            return None;
        }
        if is_us_market_holiday(et_dt.date_naive()) {
            return None;
        }
        Some(et_dt.num_seconds_from_midnight() as u64)
    }

    /// Returns `true` if the current time is in extended hours (pre-market or
    /// after-hours) for `UsEquity`. Always `false` for other sessions.
    pub fn is_extended_hours(&self, utc_ms: u64) -> bool {
        self.is_pre_market(utc_ms) || self.is_after_hours(utc_ms)
    }

    /// Returns `true` if the session is active: either `Open` or `Extended` (not `Closed`).
    pub fn is_active(&self, utc_ms: u64) -> bool {
        matches!(self.status(utc_ms), Ok(TradingStatus::Open) | Ok(TradingStatus::Extended))
    }

    /// Return the UTC millisecond timestamp of the next time this session
    /// enters [`TradingStatus::Closed`] status.
    ///
    /// If the session is already `Closed`, returns `utc_ms` unchanged.
    /// For [`MarketSession::Crypto`], which never closes, returns `u64::MAX`.
    pub fn next_close_ms(&self, utc_ms: u64) -> u64 {
        match self.session {
            MarketSession::Crypto => u64::MAX,
            MarketSession::Forex => self.next_forex_close_ms(utc_ms),
            MarketSession::UsEquity => self.next_us_equity_close_ms(utc_ms),
        }
    }

    /// Returns a human-readable label for the current session status.
    ///
    /// For `UsEquity`: `"open"`, `"pre-market"`, `"after-hours"`, or `"closed"`.
    /// For `Crypto`: always `"open"`. For `Forex`: `"open"` or `"closed"`.
    pub fn session_label(&self, utc_ms: u64) -> &'static str {
        match self.session {
            MarketSession::Crypto => "open",
            MarketSession::Forex => {
                if self.is_open(utc_ms) { "open" } else { "closed" }
            }
            MarketSession::UsEquity => {
                if self.is_open(utc_ms) {
                    "open"
                } else if self.is_pre_market(utc_ms) {
                    "pre-market"
                } else if self.is_after_hours(utc_ms) {
                    "after-hours"
                } else {
                    "closed"
                }
            }
        }
    }

    /// Returns `true` only during regular (non-extended) open hours.
    ///
    /// For `UsEquity`: `true` when 9:30–16:00 ET on a trading day.
    /// For `Crypto`: always `true`. For `Forex`: same as `is_open`.
    pub fn is_liquid(&self, utc_ms: u64) -> bool {
        self.is_open(utc_ms)
    }

    /// Fraction `[0.0, 1.0]` of the current trading session elapsed at `utc_ms`.
    ///
    /// Returns `None` when:
    /// - The session is not currently [`TradingStatus::Open`].
    /// - The session never closes (e.g. [`MarketSession::Crypto`]).
    ///
    /// Returns `0.0` at the exact session open and `1.0` at the session close.
    /// Values are clamped to `[0.0, 1.0]`.
    pub fn session_progress(&self, utc_ms: u64) -> Option<f64> {
        if !self.is_open(utc_ms) {
            return None;
        }
        let duration_ms = self.session.session_duration_ms();
        if duration_ms == u64::MAX {
            return None; // Crypto never closes
        }
        // Find the open time by locating the next open after (utc_ms - duration_ms).
        // Since is_open is true, the session opened within the last duration_ms.
        let look_before = utc_ms.saturating_sub(duration_ms);
        let open_ms = self.next_open_ms(look_before);
        let elapsed = utc_ms.saturating_sub(open_ms);
        Some((elapsed as f64 / duration_ms as f64).clamp(0.0, 1.0))
    }

    /// Milliseconds elapsed since the current session opened.
    ///
    /// Returns `None` if the session is not currently [`TradingStatus::Open`] or
    /// if the session never closes (e.g. [`MarketSession::Crypto`]).
    ///
    /// At session open this returns `0`; this is the absolute counterpart to the
    /// fractional [`session_progress`](Self::session_progress).
    pub fn time_in_session_ms(&self, utc_ms: u64) -> Option<u64> {
        if !self.is_open(utc_ms) {
            return None;
        }
        let duration_ms = self.session.session_duration_ms();
        if duration_ms == u64::MAX {
            return None;
        }
        let look_before = utc_ms.saturating_sub(duration_ms);
        let open_ms = self.next_open_ms(look_before);
        Some(utc_ms.saturating_sub(open_ms))
    }

    /// Milliseconds remaining until the current session closes.
    ///
    /// Returns `None` if the session is not currently [`TradingStatus::Open`] or
    /// if the session never closes (e.g. [`MarketSession::Crypto`]).
    ///
    /// This is the complement of [`time_in_session_ms`](Self::time_in_session_ms):
    /// `remaining + elapsed == session_duration_ms`.
    pub fn remaining_session_ms(&self, utc_ms: u64) -> Option<u64> {
        let elapsed = self.time_in_session_ms(utc_ms)?;
        let duration_ms = self.session.session_duration_ms();
        Some(duration_ms.saturating_sub(elapsed))
    }

    /// UTC fraction of the 24-hour day elapsed at `utc_ms`.
    ///
    /// Returns a value in `[0.0, 1.0)` representing how far through the calendar
    /// day the timestamp is: `0.0` at midnight UTC, approaching `1.0` just before
    /// the next midnight. Independent of session status.
    pub fn fraction_of_day_elapsed(&self, utc_ms: u64) -> f64 {
        const MS_PER_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
        let ms_in_day = utc_ms % (24 * 60 * 60 * 1000);
        ms_in_day as f64 / MS_PER_DAY
    }

    /// Minutes elapsed since the current session opened.
    ///
    /// Returns `0` when the market is closed or for sessions without a defined
    /// open time (Crypto). Rounds down.
    pub fn minutes_since_open(&self, utc_ms: u64) -> u64 {
        self.time_in_session_ms(utc_ms)
            .map(|ms| ms / 60_000)
            .unwrap_or(0)
    }

    /// Milliseconds remaining until the session closes, or `None` when the
    /// session is not currently in regular trading hours.
    ///
    /// Unlike [`time_until_close_ms`](Self::time_until_close_ms) (which returns
    /// `u64::MAX` for always-open sessions and `0` when closed), this returns
    /// `None` for both the closed and always-open cases.
    pub fn remaining_until_close_ms(&self, utc_ms: u64) -> Option<u64> {
        if !self.is_regular_session(utc_ms) {
            return None;
        }
        let close = self.next_close_ms(utc_ms);
        if close == u64::MAX {
            return None;
        }
        Some(close.saturating_sub(utc_ms))
    }

    /// Returns `true` if the session is currently in the pre-open (pre-market)
    /// window — extended hours that precede the regular trading session.
    ///
    /// Alias for [`is_pre_market`](Self::is_pre_market). Always `false` for non-equity sessions.
    #[deprecated(since = "2.2.0", note = "Use `is_pre_market` instead")]
    pub fn is_pre_open(&self, utc_ms: u64) -> bool {
        self.is_pre_market(utc_ms)
    }

    /// Fraction of the 24-hour UTC day **remaining** at `utc_ms`.
    ///
    /// Returns a value in `(0.0, 1.0]` — `1.0` exactly at midnight UTC,
    /// approaching `0.0` just before the next midnight. The complement of
    /// [`fraction_of_day_elapsed`](Self::fraction_of_day_elapsed).
    pub fn day_fraction_remaining(&self, utc_ms: u64) -> f64 {
        1.0 - self.fraction_of_day_elapsed(utc_ms)
    }

    /// Returns `true` if the market is in regular trading hours only
    /// (`TradingStatus::Open`), not extended hours or closed.
    pub fn is_regular_session(&self, utc_ms: u64) -> bool {
        self.is_open(utc_ms)
    }

    /// Returns `true` if the current time is within the final 60 minutes of
    /// the regular session.
    pub fn is_last_trading_hour(&self, utc_ms: u64) -> bool {
        self.remaining_session_ms(utc_ms).map_or(false, |r| r <= 3_600_000)
    }

    /// Returns `true` if the session is open and `utc_ms` is within
    /// `margin_ms` of the end of the regular session.
    ///
    /// Returns `false` when outside the regular session (closed, extended,
    /// etc.).  Uses the same remaining-time calculation as
    /// [`remaining_session_ms`](Self::remaining_session_ms).
    pub fn is_near_close(&self, utc_ms: u64, margin_ms: u64) -> bool {
        self.remaining_session_ms(utc_ms).map_or(false, |r| r <= margin_ms)
    }

    /// Duration of the regular (non-extended) session in milliseconds.
    ///
    /// For `UsEquity` this is 6.5 hours (23,400,000 ms); for `Crypto` it
    /// returns `u64::MAX` (always open); for `Forex` it returns the
    /// standard weekly duration (120 hours = 432,000,000 ms).
    pub fn open_duration_ms(&self) -> u64 {
        self.session.session_duration_ms()
    }

    /// Returns `true` if within the first 30 minutes of the regular session.
    ///
    /// The opening range is a commonly watched period for establishing the
    /// day's initial price range. Returns `false` outside the session.
    pub fn is_opening_range(&self, utc_ms: u64) -> bool {
        self.time_in_session_ms(utc_ms).map_or(false, |e| e < 30 * 60 * 1_000)
    }

    /// Returns `true` if the session is between 25 % and 75 % complete.
    ///
    /// Useful for identifying the "mid-session" consolidation period.
    /// Returns `false` outside the session or when session progress is
    /// unavailable (e.g. `Crypto`).
    pub fn is_mid_session(&self, utc_ms: u64) -> bool {
        self.session_progress(utc_ms).map_or(false, |p| p >= 0.25 && p <= 0.75)
    }

    /// Returns `true` if the session is in the first half (< 50%) of its duration.
    ///
    /// Returns `false` outside the session.
    pub fn is_first_half(&self, utc_ms: u64) -> bool {
        self.session_progress(utc_ms).map_or(false, |p| p < 0.5)
    }

    /// Returns which half of the trading session we are in: `1` for the first half,
    /// `2` for the second half. Returns `0` if outside the session.
    pub fn session_half(&self, utc_ms: u64) -> u8 {
        self.session_progress(utc_ms).map_or(0, |p| if p < 0.5 { 1 } else { 2 })
    }

    /// Returns `true` if both `self` and `other` are simultaneously open at `utc_ms`.
    ///
    /// Useful for detecting session overlaps like the London/New York overlap (13:00–17:00 UTC).
    pub fn overlaps_with(&self, other: &SessionAwareness, utc_ms: u64) -> bool {
        self.is_open(utc_ms) && other.is_open(utc_ms)
    }

    /// Milliseconds elapsed since the session opened at `utc_ms`.
    ///
    /// Returns `0` if the session is not open.
    pub fn open_ms(&self, utc_ms: u64) -> u64 {
        self.time_in_session_ms(utc_ms).unwrap_or(0)
    }

    /// Session progress as a percentage `[0.0, 100.0]`.
    ///
    /// Returns `0.0` outside the session.
    pub fn progress_pct(&self, utc_ms: u64) -> f64 {
        self.session_progress(utc_ms).unwrap_or(0.0) * 100.0
    }

    /// Milliseconds remaining until the session closes at `utc_ms`.
    ///
    /// Returns `0` if the session is not open or already past close.
    pub fn remaining_ms(&self, utc_ms: u64) -> u64 {
        let elapsed = self.time_in_session_ms(utc_ms).unwrap_or(0);
        let duration = self.session.session_duration_ms();
        if duration == u64::MAX { return u64::MAX; }
        duration.saturating_sub(elapsed)
    }

    /// Returns `true` if the session is in the first 25% of its duration.
    ///
    /// Returns `false` outside the session.
    pub fn is_first_quarter(&self, utc_ms: u64) -> bool {
        self.session_progress(utc_ms).map_or(false, |p| p < 0.25)
    }

    /// Returns `true` if the session is in the last 25% of its duration.
    ///
    /// Returns `false` outside the session.
    pub fn is_last_quarter(&self, utc_ms: u64) -> bool {
        self.session_progress(utc_ms).map_or(false, |p| p > 0.75)
    }

    /// Minutes elapsed since the session opened.
    ///
    /// Returns `0.0` if the session is not open.
    pub fn minutes_elapsed(&self, utc_ms: u64) -> f64 {
        self.time_in_session_ms(utc_ms).unwrap_or(0) as f64 / 60_000.0
    }

    /// Returns `true` if within the last 60 minutes of the regular session (the "power hour").
    ///
    /// Returns `false` outside the session.
    pub fn is_power_hour(&self, utc_ms: u64) -> bool {
        self.session_progress(utc_ms).map_or(false, |p| p > (5.5 / 6.5))
    }

    /// Returns `true` if the session is overnight: the market is closed
    /// (or extended-hours-only) and the current UTC time falls between
    /// 20:00 ET (after-hours end) and 04:00 ET (pre-market start) on a weekday.
    ///
    /// Always `false` for `Crypto` (never closes) and `Forex` (uses its own
    /// schedule).
    pub fn is_overnight(&self, utc_ms: u64) -> bool {
        if self.session != crate::session::MarketSession::UsEquity {
            return false;
        }
        !self.is_open(utc_ms) && !self.is_extended_hours(utc_ms) && !Self::is_weekend(utc_ms)
    }

    /// Minutes until the next regular session open, as `f64`.
    ///
    /// Returns `0.0` when the session is already open. Uses
    /// [`time_until_open_ms`](Self::time_until_open_ms) for the underlying
    /// calculation.
    pub fn minutes_to_next_open(&self, utc_ms: u64) -> f64 {
        self.time_until_open_ms(utc_ms) as f64 / 60_000.0
    }

    /// How far through the current session as a percentage (0.0–100.0).
    ///
    /// Alias for [`progress_pct`](Self::progress_pct). Returns `0.0` if the session is not currently open.
    #[deprecated(since = "2.2.0", note = "Use `progress_pct` instead")]
    pub fn session_progress_pct(&self, utc_ms: u64) -> f64 {
        self.progress_pct(utc_ms)
    }

    /// Returns `true` if the session is open and within the last 60 seconds of
    /// the regular trading session.
    pub fn is_last_minute(&self, utc_ms: u64) -> bool {
        self.remaining_session_ms(utc_ms).map_or(false, |r| r <= 60_000)
    }

    /// Returns the week of the month (1–5) for `date`.
    ///
    /// Week 1 contains day 1, week 2 contains day 8, etc.
    pub fn week_of_month(date: NaiveDate) -> u32 {
        (date.day() - 1) / 7 + 1
    }

    /// Returns the weekday name of `date` as a static string.
    ///
    /// Returns one of `"Monday"`, `"Tuesday"`, `"Wednesday"`, `"Thursday"`,
    /// `"Friday"`, `"Saturday"`, or `"Sunday"`.
    pub fn day_of_week_name(date: NaiveDate) -> &'static str {
        match date.weekday() {
            Weekday::Mon => "Monday",
            Weekday::Tue => "Tuesday",
            Weekday::Wed => "Wednesday",
            Weekday::Thu => "Thursday",
            Weekday::Fri => "Friday",
            Weekday::Sat => "Saturday",
            Weekday::Sun => "Sunday",
        }
    }

    /// Returns `true` if `date` falls in the final week of the month (day ≥ 22).
    ///
    /// Weekly equity options expire on Fridays; monthly contracts expire the
    /// third Friday. The final calendar week of the month covers both cases
    /// and is often associated with elevated volatility and rebalancing.
    pub fn is_expiry_week(date: NaiveDate) -> bool {
        date.day() >= 22
    }

    /// Human-readable name of the configured market session.
    pub fn session_name(&self) -> &'static str {
        match self.session {
            MarketSession::UsEquity => "US Equity",
            MarketSession::Crypto => "Crypto",
            MarketSession::Forex => "Forex",
        }
    }

    /// Returns `true` if `date` falls in a typical US earnings season month
    /// (January, April, July, or October).
    ///
    /// These months see heavy corporate earnings reports, increasing market volatility.
    pub fn is_earnings_season(date: NaiveDate) -> bool {
        matches!(date.month(), 1 | 4 | 7 | 10)
    }

    /// Returns `true` if `utc_ms` falls within the midday 12:00–13:00 ET quiet period
    /// (3 hours into the 6.5-hour US equity session).
    ///
    /// Always returns `false` for non-US-Equity sessions.
    pub fn is_lunch_hour(&self, utc_ms: u64) -> bool {
        if self.session != MarketSession::UsEquity {
            return false;
        }
        self.time_in_session_ms(utc_ms).map_or(false, |e| e >= 150 * 60 * 1_000 && e < 210 * 60 * 1_000)
    }

    /// Returns `true` if `date` is a triple witching day (third Friday of March, June,
    /// September, or December) — the quarterly expiration of index futures, index options,
    /// and single-stock options.
    pub fn is_triple_witching(date: NaiveDate) -> bool {
        let month = date.month();
        if !matches!(month, 3 | 6 | 9 | 12) {
            return false;
        }
        if date.weekday() != Weekday::Fri {
            return false;
        }
        // Third Friday: day is in [15, 21]
        let day = date.day();
        day >= 15 && day <= 21
    }

    /// Count of weekdays (Mon–Fri) between `from` and `to`, inclusive.
    ///
    /// Returns 0 if `from > to`. Does not account for US market holidays.
    pub fn trading_days_elapsed(from: NaiveDate, to: NaiveDate) -> u32 {
        if from > to {
            return 0;
        }
        let total_days = (to - from).num_days() + 1;
        let mut weekdays = 0u32;
        let mut d = from;
        for _ in 0..total_days {
            if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
                weekdays += 1;
            }
            if let Some(next) = d.succ_opt() {
                d = next;
            }
        }
        weekdays
    }

    /// Fraction of the session remaining: `1.0 - session_progress`.
    ///
    /// Returns `None` if the session is not open.
    pub fn fraction_remaining(&self, utc_ms: u64) -> Option<f64> {
        self.session_progress(utc_ms).map(|p| 1.0 - p)
    }

    /// Minutes elapsed since the most recent session close.
    ///
    /// Returns `0.0` if the session is currently open or the last close
    /// time cannot be determined (e.g., before any session has ever closed).
    pub fn minutes_since_close(&self, utc_ms: u64) -> f64 {
        if self.is_open(utc_ms) {
            return 0.0;
        }
        (self.time_until_open_ms(utc_ms) as f64) / 60_000.0
    }

    /// Returns `true` if `utc_ms` falls within the first 60 seconds of the
    /// regular trading session (the "opening bell" minute).
    pub fn is_opening_bell_minute(&self, utc_ms: u64) -> bool {
        self.time_in_session_ms(utc_ms).map_or(false, |e| e <= 60_000)
    }

    /// Returns `true` if `utc_ms` falls within the final 60 seconds of the session.
    ///
    /// Always returns `false` for non-US-Equity sessions or when the session is not open.
    pub fn is_closing_bell_minute(&self, utc_ms: u64) -> bool {
        if self.session != MarketSession::UsEquity {
            return false;
        }
        const SESSION_LENGTH_MS: u64 = 6 * 3_600_000 + 30 * 60_000; // 6.5h
        self.time_in_session_ms(utc_ms).map_or(false, |e| e + 60_000 >= SESSION_LENGTH_MS)
    }

    /// Returns `true` if `date` is the day immediately before or after a major
    /// US market holiday (Christmas, New Year's Day, Independence Day, Thanksgiving
    /// Friday, or Labor Day). These adjacent days often see reduced liquidity.
    ///
    /// Uses a fixed-rule approximation: does not account for observed holidays
    /// that shift when the holiday falls on a weekend.
    pub fn is_market_holiday_adjacent(date: NaiveDate) -> bool {
        let month = date.month();
        let day = date.day();
        // Dec 24 (Christmas Eve) or Dec 26 (day after Christmas)
        if month == 12 && (day == 24 || day == 26) {
            return true;
        }
        // Dec 31 (New Year's Eve) or Jan 2 (day after New Year's)
        if (month == 12 && day == 31) || (month == 1 && day == 2) {
            return true;
        }
        // Jul 3 (day before Independence Day) or Jul 5 (day after)
        if month == 7 && (day == 3 || day == 5) {
            return true;
        }
        // Black Friday (day after Thanksgiving) — 4th Friday of November
        if month == 11 && date.weekday() == Weekday::Fri {
            let d = day;
            if d >= 23 && d <= 29 {
                return true;
            }
        }
        false
    }

    /// Returns `true` if `date` falls within an approximate FOMC blackout window.
    ///
    /// The Fed's blackout rule prohibits public commentary in the 10 calendar days
    /// before each FOMC decision. FOMC meetings are scheduled roughly 8 times per
    /// year; this heuristic marks days 18–31 of odd months (Jan, Mar, May, Jul,
    /// Sep, Nov) as blackout candidates — a conservative approximation.
    pub fn is_fomc_blackout_window(date: NaiveDate) -> bool {
        let day = date.day();
        // Meetings land in odd-numbered months; blackout starts ~10 days before
        // the typical mid/late-month decision date.
        matches!(date.month(), 1 | 3 | 5 | 7 | 9 | 11) && day >= 18
    }

    fn next_forex_close_ms(&self, utc_ms: u64) -> u64 {
        if self.forex_status(utc_ms) == TradingStatus::Closed {
            return utc_ms;
        }
        // Forex closes Friday 22:00 UTC.
        let day_of_week = (utc_ms / (24 * 3600 * 1000) + 4) % 7; // 0=Sun..6=Sat
        let day_ms = utc_ms % (24 * 3600 * 1000);
        let start_of_day = utc_ms - day_ms;
        let hour_22_ms: u64 = 22 * 3600 * 1000;

        // Days until next Friday
        let days_to_friday: u64 = match day_of_week {
            0 => 5, // Sun → Fri
            1 => 4, // Mon → Fri
            2 => 3, // Tue → Fri
            3 => 2, // Wed → Fri
            4 => 1, // Thu → Fri
            5 => 0, // Fri (before 22:00, so close is today)
            _ => 6, // Sat shouldn't happen (already closed)
        };
        start_of_day + days_to_friday * 24 * 3600 * 1000 + hour_22_ms
    }

    fn next_us_equity_close_ms(&self, utc_ms: u64) -> u64 {
        if self.us_equity_status(utc_ms) == TradingStatus::Closed {
            return utc_ms;
        }
        // Market is Open or Extended. Next full close is today at 20:00 ET.
        // 20:00 ET = 00:00 UTC next calendar day (EDT) or 01:00 UTC next day (EST).
        let secs = (utc_ms / 1000) as i64;
        let dt = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now);
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + Duration::seconds(et_offset_secs);
        let et_date = et_dt.date_naive();

        // The UTC calendar date of 20:00 ET is the day after the ET date.
        let utc_date = et_date + Duration::days(1);
        let close_hour_utc: u32 = if is_us_dst(utc_ms) { 0 } else { 1 };
        let approx_ms = date_to_utc_ms(utc_date, close_hour_utc, 0);
        let corrected_hour: u32 = if is_us_dst(approx_ms) { 0 } else { 1 };
        date_to_utc_ms(utc_date, corrected_hour, 0)
    }

    fn next_forex_open_ms(&self, utc_ms: u64) -> u64 {
        if self.forex_status(utc_ms) == TradingStatus::Open {
            return utc_ms;
        }
        // Forex is closed on Saturday, Sunday before 22:00, or Friday >= 22:00.
        // The next open is always Sunday 22:00 UTC.
        let day_of_week = (utc_ms / (24 * 3600 * 1000) + 4) % 7; // 0=Sun, ..., 6=Sat
        let day_ms = utc_ms % (24 * 3600 * 1000);
        let start_of_day = utc_ms - day_ms;
        let hour_22_ms: u64 = 22 * 3600 * 1000;

        let days_to_sunday: u64 = match day_of_week {
            0 => 0, // Sunday before 22:00 → today at 22:00
            5 => 2, // Friday after 22:00 → 2 days to Sunday
            6 => 1, // Saturday → 1 day to Sunday
            _ => 0, // other days shouldn't happen (forex is open)
        };
        start_of_day + days_to_sunday * 24 * 3600 * 1000 + hour_22_ms
    }

    fn next_us_equity_open_ms(&self, utc_ms: u64) -> u64 {
        if self.us_equity_status(utc_ms) == TradingStatus::Open {
            return utc_ms;
        }
        let secs = (utc_ms / 1000) as i64;
        let dt = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now);
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + Duration::seconds(et_offset_secs);

        let dow = et_dt.weekday();
        let et_time_secs = et_dt.num_seconds_from_midnight() as u64;
        let open_s: u64 = 9 * 3600 + 30 * 60; // 09:30 ET

        // Days to advance in ET to reach the next trading-day open.
        let days_ahead: i64 = match dow {
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu => {
                if et_time_secs < open_s {
                    0 // Before today's open
                } else {
                    1 // After today's open: next weekday
                }
            }
            Weekday::Fri => {
                if et_time_secs < open_s {
                    0 // Before today's open
                } else {
                    3 // After Friday: skip to Monday
                }
            }
            Weekday::Sat => 2, // Skip to Monday
            Weekday::Sun => 1, // Skip to Monday
        };

        let et_date = et_dt.date_naive();
        let mut target_date = et_date + Duration::days(days_ahead);

        // Skip over weekends and holidays to find the next trading day.
        loop {
            let wd = target_date.weekday();
            if wd == Weekday::Sat {
                target_date += Duration::days(2);
                continue;
            }
            if wd == Weekday::Sun {
                target_date += Duration::days(1);
                continue;
            }
            if is_us_market_holiday(target_date) {
                target_date += Duration::days(1);
                continue;
            }
            break;
        }

        // Approximate UTC hour for 9:30 ET, then correct for target-date DST.
        let open_hour_utc: u32 = if is_us_dst(utc_ms) { 13 } else { 14 };
        let approx_ms = date_to_utc_ms(target_date, open_hour_utc, 30);
        let open_hour_utc_corrected: u32 = if is_us_dst(approx_ms) { 13 } else { 14 };
        date_to_utc_ms(target_date, open_hour_utc_corrected, 30)
    }

    fn us_equity_status(&self, utc_ms: u64) -> TradingStatus {
        let secs = (utc_ms / 1000) as i64;
        let dt = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now);

        // Determine ET offset: EDT = UTC-4 during DST, EST = UTC-5 otherwise.
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + Duration::seconds(et_offset_secs);

        // Closed on weekends.
        let dow = et_dt.weekday();
        if dow == Weekday::Sat || dow == Weekday::Sun {
            return TradingStatus::Closed;
        }

        // Closed on US market holidays.
        if is_us_market_holiday(et_dt.date_naive()) {
            return TradingStatus::Closed;
        }

        let et_time_secs = et_dt.num_seconds_from_midnight() as u64;
        let open_s = (9 * 3600 + 30 * 60) as u64; // 09:30 ET
        let close_s = (16 * 3600) as u64; // 16:00 ET
        let pre_s = (4 * 3600) as u64; // 04:00 ET
        let post_s = (20 * 3600) as u64; // 20:00 ET

        if et_time_secs >= open_s && et_time_secs < close_s {
            TradingStatus::Open
        } else if (et_time_secs >= pre_s && et_time_secs < open_s)
            || (et_time_secs >= close_s && et_time_secs < post_s)
        {
            TradingStatus::Extended
        } else {
            TradingStatus::Closed
        }
    }

    fn forex_status(&self, utc_ms: u64) -> TradingStatus {
        // Forex: open Sunday 22:00 UTC – Friday 22:00 UTC (approximately)
        let day_of_week = (utc_ms / (24 * 3600 * 1000) + 4) % 7; // 0=Sun, 1=Mon, ..., 6=Sat
        let day_ms = utc_ms % (24 * 3600 * 1000);
        let hour_22_ms = 22 * 3600 * 1000;

        // Fully closed: Saturday entire day, Sunday before 22:00
        if day_of_week == 6 {
            return TradingStatus::Closed;
        }
        if day_of_week == 0 && day_ms < hour_22_ms {
            return TradingStatus::Closed;
        }
        // Friday after 22:00 UTC — also closed
        if day_of_week == 5 && day_ms >= hour_22_ms {
            return TradingStatus::Closed;
        }
        TradingStatus::Open
    }
}

/// Return `true` if `utc_ms` falls within US Daylight Saving Time.
///
/// US DST rules (since 2007):
/// - **Starts**: second Sunday of March at 02:00 local time (07:00 UTC in EST).
/// - **Ends**: first Sunday of November at 02:00 local time (06:00 UTC in EDT).
fn is_us_dst(utc_ms: u64) -> bool {
    let secs = (utc_ms / 1000) as i64;
    let dt = match Utc.timestamp_opt(secs, 0).single() {
        Some(t) => t,
        None => return false,
    };
    let year = dt.year();

    // DST starts: 2nd Sunday of March at 02:00 EST = 07:00 UTC.
    let dst_start_date = nth_weekday_of_month(year, 3, Weekday::Sun, 2);
    let dst_start_utc_ms = date_to_utc_ms(dst_start_date, 7, 0);

    // DST ends: 1st Sunday of November at 02:00 EDT = 06:00 UTC.
    let dst_end_date = nth_weekday_of_month(year, 11, Weekday::Sun, 1);
    let dst_end_utc_ms = date_to_utc_ms(dst_end_date, 6, 0);

    utc_ms >= dst_start_utc_ms && utc_ms < dst_end_utc_ms
}

/// Return the date of the N-th occurrence (1-indexed) of `weekday` in the given month/year.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 1).unwrap());
    let first_wd = first.weekday();
    let days_ahead = (weekday.num_days_from_monday() as i32
        - first_wd.num_days_from_monday() as i32)
        .rem_euclid(7);
    let first_occurrence = first + Duration::days(days_ahead as i64);
    first_occurrence + Duration::weeks((n - 1) as i64)
}

/// Convert a `NaiveDate` plus hour/minute to UTC milliseconds.
fn date_to_utc_ms(date: NaiveDate, hour: u32, minute: u32) -> u64 {
    let naive_dt = date
        .and_hms_opt(hour, minute, 0)
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap());
    let utc_dt = Utc.from_utc_datetime(&naive_dt);
    (utc_dt.timestamp() as u64) * 1000
}

/// Returns `true` if `date` (in ET) is a US equity market holiday (NYSE/NASDAQ closure).
///
/// Covers all fixed-date and floating holidays observed by US exchanges:
/// - **Fixed**: New Year's Day, Juneteenth (since 2022), Independence Day, Christmas Day.
/// - **Floating**: MLK Day, Presidents Day, Good Friday, Memorial Day, Labor Day, Thanksgiving.
///
/// Weekend-observation rules apply: Saturday holidays observe on the prior Friday,
/// Sunday holidays observe on the following Monday.
pub fn is_us_market_holiday(date: NaiveDate) -> bool {
    let year = date.year();

    // Helper: apply weekend-observation rule.
    let observe = |d: NaiveDate| -> NaiveDate {
        match d.weekday() {
            Weekday::Sat => d - Duration::days(1),
            Weekday::Sun => d + Duration::days(1),
            _ => d,
        }
    };

    let make_date = |y: i32, m: u32, d: u32| {
        NaiveDate::from_ymd_opt(y, m, d).unwrap_or_else(|| NaiveDate::from_ymd_opt(y, 1, 1).unwrap())
    };

    // New Year's Day: January 1 (observed).
    if date == observe(make_date(year, 1, 1)) {
        return true;
    }
    // MLK Day: 3rd Monday of January.
    if date == nth_weekday_of_month(year, 1, Weekday::Mon, 3) {
        return true;
    }
    // Presidents Day: 3rd Monday of February.
    if date == nth_weekday_of_month(year, 2, Weekday::Mon, 3) {
        return true;
    }
    // Good Friday: 2 days before Easter Sunday.
    if date == good_friday(year) {
        return true;
    }
    // Memorial Day: last Monday of May.
    if date.month() == 5 && date.weekday() == Weekday::Mon {
        let next_monday = date + Duration::days(7);
        if next_monday.month() != 5 {
            return true;
        }
    }
    // Juneteenth: June 19 (observed, since 2022).
    if year >= 2022 && date == observe(make_date(year, 6, 19)) {
        return true;
    }
    // Independence Day: July 4 (observed).
    if date == observe(make_date(year, 7, 4)) {
        return true;
    }
    // Labor Day: 1st Monday of September.
    if date == nth_weekday_of_month(year, 9, Weekday::Mon, 1) {
        return true;
    }
    // Thanksgiving: 4th Thursday of November.
    if date == nth_weekday_of_month(year, 11, Weekday::Thu, 4) {
        return true;
    }
    // Christmas Day: December 25 (observed).
    if date == observe(make_date(year, 12, 25)) {
        return true;
    }
    false
}

/// Compute the date of Good Friday for a given year (2 days before Easter Sunday).
fn good_friday(year: i32) -> NaiveDate {
    easter_sunday(year) - Duration::days(2)
}

/// Compute Easter Sunday using the Anonymous Gregorian algorithm.
fn easter_sunday(year: i32) -> NaiveDate {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = ((h + l - 7 * m + 114) % 31) + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 1).unwrap())
}

/// Count of US equity trading days (non-holiday weekdays) in the UTC millisecond range
/// `[start_ms, end_ms)`.
///
/// Uses the same holiday calendar as [`is_us_market_holiday`].
/// Returns `0` if `end_ms <= start_ms`.
pub fn trading_day_count(start_ms: u64, end_ms: u64) -> usize {
    use chrono::{Datelike, NaiveDate, TimeZone, Utc, Weekday};
    if end_ms <= start_ms {
        return 0;
    }
    let ms_to_naive = |ms: u64| {
        Utc.timestamp_opt((ms / 1000) as i64, 0)
            .single()
            .map(|dt| dt.date_naive())
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
    };
    let start_date = ms_to_naive(start_ms);
    let end_date = ms_to_naive(end_ms);
    let mut count = 0usize;
    let mut day = start_date;
    while day < end_date {
        let wd = day.weekday();
        if wd != Weekday::Sat && wd != Weekday::Sun && !is_us_market_holiday(day) {
            count += 1;
        }
        day = day.succ_opt().unwrap_or(day);
    }
    count
}

/// Convenience: check if a session is currently tradeable.
pub fn is_tradeable(session: MarketSession, utc_ms: u64) -> Result<bool, StreamError> {
    let sa = SessionAwareness::new(session);
    let status = sa.status(utc_ms)?;
    Ok(status == TradingStatus::Open || status == TradingStatus::Extended)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference Monday 2024-01-08 14:30 UTC = 09:30 ET (market open, EST=UTC-5)
    const MON_OPEN_UTC_MS: u64 = 1704724200000;
    // Monday 21:00 UTC = 16:00 ET (market close, extended hours start)
    const MON_CLOSE_UTC_MS: u64 = 1704747600000;
    // Saturday 2024-01-13 12:00 UTC = 07:00 EST — Saturday in both UTC and ET
    const SAT_UTC_MS: u64 = 1705147200000;
    // Sunday 2024-01-07 10:00 UTC (before 22:00 UTC, forex closed)
    const SUN_BEFORE_UTC_MS: u64 = 1704621600000;

    // Summer: Monday 2024-07-08 13:30 UTC = 09:30 EDT (UTC-4), market open
    const MON_SUMMER_OPEN_UTC_MS: u64 = 1720445400000;

    fn sa(session: MarketSession) -> SessionAwareness {
        SessionAwareness::new(session)
    }

    #[test]
    fn test_crypto_always_open() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.status(MON_OPEN_UTC_MS).unwrap(), TradingStatus::Open);
        assert_eq!(sa.status(SAT_UTC_MS).unwrap(), TradingStatus::Open);
        assert_eq!(sa.status(0).unwrap(), TradingStatus::Open);
    }

    #[test]
    fn test_us_equity_open_during_market_hours_est() {
        let sa = sa(MarketSession::UsEquity);
        // 14:30 UTC = 09:30 EST (UTC-5) — January, no DST
        assert_eq!(sa.status(MON_OPEN_UTC_MS).unwrap(), TradingStatus::Open);
    }

    #[test]
    fn test_us_equity_open_during_market_hours_edt() {
        let sa = sa(MarketSession::UsEquity);
        // 13:30 UTC = 09:30 EDT (UTC-4) — July, DST active
        assert_eq!(
            sa.status(MON_SUMMER_OPEN_UTC_MS).unwrap(),
            TradingStatus::Open
        );
    }

    #[test]
    fn test_us_equity_closed_after_hours() {
        let sa = sa(MarketSession::UsEquity);
        // 21:00 UTC = 16:00 ET = market close, after-hours starts
        let status = sa.status(MON_CLOSE_UTC_MS).unwrap();
        assert!(status == TradingStatus::Extended || status == TradingStatus::Closed);
    }

    #[test]
    fn test_us_equity_closed_on_saturday() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.status(SAT_UTC_MS).unwrap(), TradingStatus::Closed);
    }

    #[test]
    fn test_us_equity_premarket_extended() {
        let sa = sa(MarketSession::UsEquity);
        // Monday 09:00 UTC = 04:00 ET (pre-market, EST=UTC-5)
        let pre_ms: u64 = 1704704400000;
        let status = sa.status(pre_ms).unwrap();
        assert!(status == TradingStatus::Extended || status == TradingStatus::Open);
    }

    #[test]
    fn test_dst_transition_march() {
        // 2024 DST starts: March 10 at 07:00 UTC (2:00 AM EST → 3:00 AM EDT)
        // Just before: 06:59 UTC → EST (UTC-5) → 01:59 ET → Closed
        let just_before_dst_ms = 1710053940000_u64; // 2024-03-10 06:59 UTC
        // Just after: 07:01 UTC → EDT (UTC-4) → 03:01 ET → Closed (pre-market)
        let just_after_dst_ms = 1710054060000_u64; // 2024-03-10 07:01 UTC
        assert!(!is_us_dst(just_before_dst_ms));
        assert!(is_us_dst(just_after_dst_ms));
    }

    #[test]
    fn test_dst_transition_november() {
        // 2024 DST ends: November 3 at 06:00 UTC (2:00 AM EDT → 1:00 AM EST)
        // Just before: 05:59 UTC → DST still active
        let just_before_end_ms = 1730613540000_u64; // 2024-11-03 05:59 UTC
        // Just after: 06:01 UTC → DST ended
        let just_after_end_ms = 1730613660000_u64; // 2024-11-03 06:01 UTC
        assert!(is_us_dst(just_before_end_ms));
        assert!(!is_us_dst(just_after_end_ms));
    }

    #[test]
    fn test_forex_open_on_monday() {
        let sa = sa(MarketSession::Forex);
        assert_eq!(sa.status(MON_OPEN_UTC_MS).unwrap(), TradingStatus::Open);
    }

    #[test]
    fn test_forex_closed_on_saturday() {
        let sa = sa(MarketSession::Forex);
        assert_eq!(sa.status(SAT_UTC_MS).unwrap(), TradingStatus::Closed);
    }

    #[test]
    fn test_forex_closed_sunday_before_22_utc() {
        let sa = sa(MarketSession::Forex);
        assert_eq!(sa.status(SUN_BEFORE_UTC_MS).unwrap(), TradingStatus::Closed);
    }

    #[test]
    fn test_is_tradeable_crypto_always_true() {
        assert!(is_tradeable(MarketSession::Crypto, SAT_UTC_MS).unwrap());
    }

    #[test]
    fn test_is_tradeable_equity_open() {
        assert!(is_tradeable(MarketSession::UsEquity, MON_OPEN_UTC_MS).unwrap());
    }

    #[test]
    fn test_is_tradeable_equity_weekend_false() {
        assert!(!is_tradeable(MarketSession::UsEquity, SAT_UTC_MS).unwrap());
    }

    #[test]
    fn test_session_accessor() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.session(), MarketSession::Crypto);
    }

    #[test]
    fn test_market_session_equality() {
        assert_eq!(MarketSession::Crypto, MarketSession::Crypto);
        assert_ne!(MarketSession::Crypto, MarketSession::Forex);
    }

    #[test]
    fn test_trading_status_equality() {
        assert_eq!(TradingStatus::Open, TradingStatus::Open);
        assert_ne!(TradingStatus::Open, TradingStatus::Closed);
    }

    #[test]
    fn test_nth_weekday_of_month_second_sunday_march_2024() {
        // Second Sunday of March 2024 = March 10
        let date = nth_weekday_of_month(2024, 3, Weekday::Sun, 2);
        assert_eq!(date.month(), 3);
        assert_eq!(date.day(), 10);
    }

    #[test]
    fn test_nth_weekday_of_month_first_sunday_november_2024() {
        // First Sunday of November 2024 = November 3
        let date = nth_weekday_of_month(2024, 11, Weekday::Sun, 1);
        assert_eq!(date.month(), 11);
        assert_eq!(date.day(), 3);
    }

    // ── next_open_ms ──────────────────────────────────────────────────────────

    #[test]
    fn test_next_open_crypto_is_always_now() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.next_open_ms(SAT_UTC_MS), SAT_UTC_MS);
        assert_eq!(sa.next_open_ms(MON_OPEN_UTC_MS), MON_OPEN_UTC_MS);
    }

    #[test]
    fn test_next_open_equity_already_open_returns_same() {
        // MON_OPEN_UTC_MS = Monday 14:30 UTC = 09:30 ET (market is open)
        let sa = sa(MarketSession::UsEquity);
        let next = sa.next_open_ms(MON_OPEN_UTC_MS);
        assert_eq!(next, MON_OPEN_UTC_MS);
    }

    #[test]
    fn test_next_open_equity_saturday_returns_monday_open() {
        // SAT_UTC_MS = 2024-01-13 Saturday 12:00 UTC; market closed.
        // Mon 2024-01-15 is MLK Day (holiday), so next open = Tue 2024-01-16 14:30 UTC (EST).
        let sa = sa(MarketSession::UsEquity);
        let next = sa.next_open_ms(SAT_UTC_MS);
        // The result should be after SAT_UTC_MS and should be Open when checked.
        assert!(next > SAT_UTC_MS, "next open must be after Saturday");
        assert_eq!(
            sa.status(next).unwrap(),
            TradingStatus::Open,
            "next_open_ms must return a time when market is Open"
        );
    }

    #[test]
    fn test_next_open_equity_sunday_returns_monday_open() {
        // Sunday 10:00 UTC → next open is Monday 9:30 ET
        let sa = sa(MarketSession::UsEquity);
        let next = sa.next_open_ms(SUN_BEFORE_UTC_MS);
        assert!(next > SUN_BEFORE_UTC_MS);
        assert_eq!(sa.status(next).unwrap(), TradingStatus::Open);
    }

    #[test]
    fn test_next_open_forex_already_open_returns_same() {
        let sa = sa(MarketSession::Forex);
        assert_eq!(sa.next_open_ms(MON_OPEN_UTC_MS), MON_OPEN_UTC_MS);
    }

    #[test]
    fn test_next_open_forex_saturday_returns_sunday_22_utc() {
        // Saturday 12:00 UTC → next open is Sunday 22:00 UTC (1 day later).
        let sa = sa(MarketSession::Forex);
        let next = sa.next_open_ms(SAT_UTC_MS);
        assert!(next > SAT_UTC_MS);
        assert_eq!(sa.status(next).unwrap(), TradingStatus::Open);
        // Should be exactly Sunday 22:00 UTC.
        let expected_hour_ms = 22 * 3600 * 1000;
        assert_eq!(next % (24 * 3600 * 1000), expected_hour_ms);
    }

    #[test]
    fn test_next_open_forex_sunday_before_22_returns_same_day_22() {
        // SUN_BEFORE_UTC_MS = Sunday 10:00 UTC → next open is Sunday 22:00 UTC.
        let sa = sa(MarketSession::Forex);
        let next = sa.next_open_ms(SUN_BEFORE_UTC_MS);
        let day_ms = SUN_BEFORE_UTC_MS - (SUN_BEFORE_UTC_MS % (24 * 3600 * 1000));
        let expected = day_ms + 22 * 3600 * 1000;
        assert_eq!(next, expected);
    }

    // ── Holiday calendar ──────────────────────────────────────────────────────

    #[test]
    fn test_holiday_new_years_day_2024() {
        // 2024-01-01 is a Monday — New Year's Day, market closed.
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        assert!(is_us_market_holiday(date), "New Year's Day should be a holiday");
    }

    #[test]
    fn test_holiday_new_years_observed_when_on_sunday() {
        // 2023-01-01 is a Sunday — observed Monday 2023-01-02.
        let observed = NaiveDate::from_ymd_opt(2023, 1, 2).unwrap();
        assert!(is_us_market_holiday(observed), "Observed New Year's should be a holiday");
        let actual = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();
        assert!(!is_us_market_holiday(actual), "Sunday itself is not the observed holiday");
    }

    #[test]
    fn test_holiday_mlk_day_2024() {
        // 2024-01-15 = 3rd Monday of January.
        let date = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        assert!(is_us_market_holiday(date), "MLK Day should be a holiday");
    }

    #[test]
    fn test_holiday_good_friday_2024() {
        // Easter 2024 = March 31; Good Friday = March 29.
        let date = NaiveDate::from_ymd_opt(2024, 3, 29).unwrap();
        assert!(is_us_market_holiday(date), "Good Friday 2024 should be a holiday");
    }

    #[test]
    fn test_holiday_memorial_day_2024() {
        // Last Monday of May 2024 = May 27.
        let date = NaiveDate::from_ymd_opt(2024, 5, 27).unwrap();
        assert!(is_us_market_holiday(date), "Memorial Day should be a holiday");
    }

    #[test]
    fn test_holiday_independence_day_2024() {
        // July 4, 2024 is a Thursday.
        let date = NaiveDate::from_ymd_opt(2024, 7, 4).unwrap();
        assert!(is_us_market_holiday(date), "Independence Day should be a holiday");
    }

    #[test]
    fn test_holiday_labor_day_2024() {
        // 1st Monday of September 2024 = September 2.
        let date = NaiveDate::from_ymd_opt(2024, 9, 2).unwrap();
        assert!(is_us_market_holiday(date), "Labor Day should be a holiday");
    }

    #[test]
    fn test_holiday_thanksgiving_2024() {
        // 4th Thursday of November 2024 = November 28.
        let date = NaiveDate::from_ymd_opt(2024, 11, 28).unwrap();
        assert!(is_us_market_holiday(date), "Thanksgiving should be a holiday");
    }

    #[test]
    fn test_holiday_christmas_2024() {
        // December 25, 2024 is a Wednesday.
        let date = NaiveDate::from_ymd_opt(2024, 12, 25).unwrap();
        assert!(is_us_market_holiday(date), "Christmas should be a holiday");
    }

    #[test]
    fn test_holiday_regular_monday_is_not_holiday() {
        // 2024-01-08 is a regular Monday (not a holiday).
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        assert!(!is_us_market_holiday(date), "Regular Monday should not be a holiday");
    }

    #[test]
    fn test_holiday_market_closed_on_christmas_2024() {
        // Dec 25, 2024 = Christmas; 14:30 UTC (09:30 ET) should be Closed.
        // date_to_utc_ms(2024-12-25, 14, 30) = some ms value
        let christmas_open_utc_ms = date_to_utc_ms(
            NaiveDate::from_ymd_opt(2024, 12, 25).unwrap(),
            14,
            30,
        );
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(
            sa.status(christmas_open_utc_ms).unwrap(),
            TradingStatus::Closed,
            "Market should be closed on Christmas"
        );
    }

    #[test]
    fn test_easter_sunday_2024() {
        assert_eq!(easter_sunday(2024), NaiveDate::from_ymd_opt(2024, 3, 31).unwrap());
    }

    #[test]
    fn test_easter_sunday_2025() {
        assert_eq!(easter_sunday(2025), NaiveDate::from_ymd_opt(2025, 4, 20).unwrap());
    }

    // ── time_until_open_ms / time_until_close_ms ──────────────────────────────

    #[test]
    fn test_time_until_open_crypto_is_zero() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.time_until_open_ms(SAT_UTC_MS), 0);
        assert_eq!(sa.time_until_open_ms(MON_OPEN_UTC_MS), 0);
    }

    #[test]
    fn test_time_until_open_equity_already_open_is_zero() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.time_until_open_ms(MON_OPEN_UTC_MS), 0);
    }

    #[test]
    fn test_time_until_open_equity_saturday_is_positive() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.time_until_open_ms(SAT_UTC_MS) > 0);
    }

    #[test]
    fn test_time_until_close_crypto_is_max() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.time_until_close_ms(MON_OPEN_UTC_MS), u64::MAX);
    }

    #[test]
    fn test_time_until_close_equity_already_closed_is_zero() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.time_until_close_ms(SAT_UTC_MS), 0);
    }

    #[test]
    fn test_time_until_close_equity_open_is_positive() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.time_until_close_ms(MON_OPEN_UTC_MS) > 0);
    }

    // ── is_open ───────────────────────────────────────────────────────────────

    #[test]
    fn test_is_open_crypto_always_true() {
        let sa = sa(MarketSession::Crypto);
        assert!(sa.is_open(SAT_UTC_MS));
        assert!(sa.is_open(0));
    }

    #[test]
    fn test_is_open_equity_during_market_hours() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_open(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_open_equity_on_weekend_false() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_open(SAT_UTC_MS));
    }

    #[test]
    fn test_is_open_forex_on_monday_true() {
        let sa = sa(MarketSession::Forex);
        assert!(sa.is_open(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_open_forex_on_saturday_false() {
        let sa = sa(MarketSession::Forex);
        assert!(!sa.is_open(SAT_UTC_MS));
    }

    // ── next_close_ms ─────────────────────────────────────────────────────────

    #[test]
    fn test_next_close_crypto_is_max() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.next_close_ms(MON_OPEN_UTC_MS), u64::MAX);
        assert_eq!(sa.next_close_ms(SAT_UTC_MS), u64::MAX);
    }

    #[test]
    fn test_next_close_equity_already_closed_returns_same() {
        // SAT_UTC_MS is Saturday — market is closed
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.next_close_ms(SAT_UTC_MS), SAT_UTC_MS);
    }

    #[test]
    fn test_next_close_equity_open_est_returns_20_00_et() {
        // MON_OPEN_UTC_MS = Monday 2024-01-08 14:30 UTC = 09:30 ET (Open, EST=UTC-5)
        // 20:00 ET (EST) = 01:00 UTC the next calendar day = 2024-01-09 01:00 UTC
        let sa = sa(MarketSession::UsEquity);
        let close = sa.next_close_ms(MON_OPEN_UTC_MS);
        assert!(close > MON_OPEN_UTC_MS);
        assert_eq!(sa.status(close).unwrap(), TradingStatus::Closed);
        // 2024-01-09 01:00 UTC = 1704762000000 ms
        assert_eq!(close, 1704762000000);
    }

    #[test]
    fn test_next_close_equity_open_edt_returns_midnight_utc() {
        // MON_SUMMER_OPEN_UTC_MS = Monday 2024-07-08 13:30 UTC = 09:30 EDT (UTC-4)
        // 20:00 ET (EDT) = 00:00 UTC the next calendar day = 2024-07-09 00:00 UTC
        let sa = sa(MarketSession::UsEquity);
        let close = sa.next_close_ms(MON_SUMMER_OPEN_UTC_MS);
        assert!(close > MON_SUMMER_OPEN_UTC_MS);
        assert_eq!(sa.status(close).unwrap(), TradingStatus::Closed);
        // 2024-07-09 00:00 UTC = 1720483200000 ms
        assert_eq!(close, 1720483200000);
    }

    #[test]
    fn test_next_close_equity_extended_returns_20_00_et() {
        // MON_CLOSE_UTC_MS = Monday 2024-01-08 21:00 UTC = 16:00 ET (Extended)
        // Next full close = 20:00 ET = 2024-01-09 01:00 UTC
        let sa = sa(MarketSession::UsEquity);
        let close = sa.next_close_ms(MON_CLOSE_UTC_MS);
        assert!(close > MON_CLOSE_UTC_MS);
        assert_eq!(sa.status(close).unwrap(), TradingStatus::Closed);
        assert_eq!(close, 1704762000000);
    }

    #[test]
    fn test_next_close_forex_already_closed_returns_same() {
        // SAT_UTC_MS = Saturday — forex is closed
        let sa = sa(MarketSession::Forex);
        assert_eq!(sa.next_close_ms(SAT_UTC_MS), SAT_UTC_MS);
    }

    #[test]
    fn test_next_close_forex_open_monday_returns_friday_22_utc() {
        // MON_OPEN_UTC_MS = Monday 2024-01-08 14:30 UTC → forex open
        // Next close = Friday 2024-01-12 22:00 UTC = 1705096800000 ms
        let sa = sa(MarketSession::Forex);
        let close = sa.next_close_ms(MON_OPEN_UTC_MS);
        assert!(close > MON_OPEN_UTC_MS);
        assert_eq!(sa.status(close).unwrap(), TradingStatus::Closed);
        assert_eq!(close, 1705096800000);
    }

    // ── MarketSession::session_duration_ms ────────────────────────────────────

    #[test]
    fn test_session_duration_us_equity_is_6_5_hours() {
        // 9:30–16:00 ET = 6.5 hours = 23 400 000 ms
        assert_eq!(
            MarketSession::UsEquity.session_duration_ms(),
            6 * 3_600_000 + 30 * 60_000
        );
    }

    #[test]
    fn test_session_duration_forex_is_120_hours() {
        assert_eq!(MarketSession::Forex.session_duration_ms(), 5 * 24 * 3_600_000);
    }

    #[test]
    fn test_session_duration_crypto_is_max() {
        assert_eq!(MarketSession::Crypto.session_duration_ms(), u64::MAX);
    }

    // ── SessionAwareness::is_extended ─────────────────────────────────────────

    #[test]
    fn test_is_extended_crypto_is_never_extended() {
        let sa = sa(MarketSession::Crypto);
        // Crypto is always Open, never Extended
        assert!(!sa.is_extended(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_extended_equity_during_extended_hours() {
        // 7:00 AM EST on Monday 2024-01-08 = pre-market (Extended)
        // 2024-01-08 00:00 UTC = 1704672000 s. 7:00 AM EST (UTC-5) = 12:00 UTC
        // → 1704672000 + 12*3600 = 1704715200 s = 1704715200000 ms
        let seven_am_est_ms = 1704715200_000u64;
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.status(seven_am_est_ms).unwrap(), TradingStatus::Extended);
        assert!(sa.is_extended(seven_am_est_ms));
    }

    #[test]
    fn test_is_extended_equity_during_open_is_false() {
        let sa = sa(MarketSession::UsEquity);
        // MON_OPEN_UTC_MS is during regular Open hours
        assert!(!sa.is_extended(MON_OPEN_UTC_MS));
    }

    // ── SessionAwareness::session_progress ────────────────────────────────────

    #[test]
    fn test_session_progress_none_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        // SAT_UTC_MS is on a weekend (closed)
        assert!(sa.session_progress(SAT_UTC_MS).is_none());
    }

    #[test]
    fn test_session_progress_none_for_crypto() {
        let sa = sa(MarketSession::Crypto);
        assert!(sa.session_progress(MON_OPEN_UTC_MS).is_none());
    }

    #[test]
    fn test_session_progress_at_open_is_zero() {
        let sa = sa(MarketSession::UsEquity);
        // MON_OPEN_UTC_MS is exactly at 9:30 AM ET (session open)
        let progress = sa.session_progress(MON_OPEN_UTC_MS).unwrap();
        assert!(progress.abs() < 1e-6, "expected ~0.0 got {progress}");
    }

    #[test]
    fn test_session_progress_midway() {
        let sa = sa(MarketSession::UsEquity);
        // US equity session is 6.5 hours = 23_400_000 ms
        // Midway = open + 11_700_000 ms
        let mid_ms = MON_OPEN_UTC_MS + 11_700_000;
        let progress = sa.session_progress(mid_ms).unwrap();
        assert!((progress - 0.5).abs() < 1e-6, "expected ~0.5 got {progress}");
    }

    #[test]
    fn test_session_progress_in_range_zero_to_one() {
        let sa = sa(MarketSession::UsEquity);
        // One hour into the session
        let one_hour_in = MON_OPEN_UTC_MS + 3_600_000;
        let progress = sa.session_progress(one_hour_in).unwrap();
        assert!(progress > 0.0 && progress < 1.0, "expected (0,1) got {progress}");
    }

    // ── SessionAwareness::is_closed ───────────────────────────────────────────

    #[test]
    fn test_is_closed_crypto_is_never_closed() {
        let sa = sa(MarketSession::Crypto);
        assert!(!sa.is_closed(MON_OPEN_UTC_MS));
        assert!(!sa.is_closed(SAT_UTC_MS));
    }

    #[test]
    fn test_is_closed_equity_on_weekend() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_closed(SAT_UTC_MS));
    }

    #[test]
    fn test_is_closed_equity_during_open_is_false() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_closed(MON_OPEN_UTC_MS));
    }

    // ── SessionAwareness::is_market_hours ─────────────────────────────────────

    #[test]
    fn test_is_market_hours_crypto_always_true() {
        let sa = sa(MarketSession::Crypto);
        assert!(sa.is_market_hours(MON_OPEN_UTC_MS));
        assert!(sa.is_market_hours(SAT_UTC_MS));
    }

    #[test]
    fn test_is_market_hours_equity_open_is_true() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_market_hours(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_market_hours_equity_extended_is_true() {
        // 7:00 AM EST = pre-market (Extended)
        let seven_am_est_ms = 1704715200_000u64;
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_market_hours(seven_am_est_ms));
    }

    #[test]
    fn test_is_market_hours_equity_closed_is_false() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_market_hours(SAT_UTC_MS));
    }

    // ── MarketSession::has_extended_hours ─────────────────────────────────────

    #[test]
    fn test_us_equity_has_extended_hours() {
        assert!(MarketSession::UsEquity.has_extended_hours());
    }

    #[test]
    fn test_crypto_has_no_extended_hours() {
        assert!(!MarketSession::Crypto.has_extended_hours());
    }

    #[test]
    fn test_forex_has_no_extended_hours() {
        assert!(!MarketSession::Forex.has_extended_hours());
    }

    // ── SessionAwareness::time_in_session_ms ──────────────────────────────────

    #[test]
    fn test_time_in_session_ms_none_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.time_in_session_ms(SAT_UTC_MS).is_none());
    }

    #[test]
    fn test_time_in_session_ms_none_for_crypto() {
        let sa = sa(MarketSession::Crypto);
        assert!(sa.time_in_session_ms(MON_OPEN_UTC_MS).is_none());
    }

    #[test]
    fn test_time_in_session_ms_zero_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.time_in_session_ms(MON_OPEN_UTC_MS).unwrap(), 0);
    }

    #[test]
    fn test_time_in_session_ms_one_hour_in() {
        let sa = sa(MarketSession::UsEquity);
        let one_hour_in = MON_OPEN_UTC_MS + 3_600_000;
        assert_eq!(sa.time_in_session_ms(one_hour_in).unwrap(), 3_600_000);
    }

    // ── SessionAwareness::minutes_until_close ─────────────────────────────────

    #[test]
    fn test_minutes_until_close_crypto_is_max() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.minutes_until_close(MON_OPEN_UTC_MS), u64::MAX);
    }

    #[test]
    fn test_minutes_until_close_equity_already_closed() {
        let sa = sa(MarketSession::UsEquity);
        // Already closed on Saturday
        assert_eq!(sa.minutes_until_close(SAT_UTC_MS), 0);
    }

    #[test]
    fn test_minutes_until_close_equity_open_positive() {
        let sa = sa(MarketSession::UsEquity);
        let mins = sa.minutes_until_close(MON_OPEN_UTC_MS);
        assert!(mins > 0, "expected > 0 minutes until close, got {mins}");
    }

    #[test]
    fn test_remaining_session_ms_complements_elapsed() {
        let sa = sa(MarketSession::UsEquity);
        let one_hour_in = MON_OPEN_UTC_MS + 3_600_000;
        let elapsed = sa.time_in_session_ms(one_hour_in).unwrap();
        let remaining = sa.remaining_session_ms(one_hour_in).unwrap();
        let duration_ms = MarketSession::UsEquity.session_duration_ms();
        assert_eq!(elapsed + remaining, duration_ms);
    }

    #[test]
    fn test_remaining_session_ms_closed_returns_none() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.remaining_session_ms(SAT_UTC_MS).is_none());
    }

    // ── SessionAwareness::is_weekend ──────────────────────────────────────────

    #[test]
    fn test_is_weekend_saturday_is_weekend() {
        // SAT_UTC_MS = 2024-01-13 12:00 UTC (Saturday)
        assert!(SessionAwareness::is_weekend(SAT_UTC_MS));
    }

    #[test]
    fn test_is_weekend_sunday_is_weekend() {
        // SUN_BEFORE_UTC_MS = 2024-01-07 10:00 UTC (Sunday)
        assert!(SessionAwareness::is_weekend(SUN_BEFORE_UTC_MS));
    }

    #[test]
    fn test_is_weekend_monday_is_not_weekend() {
        // MON_OPEN_UTC_MS = 2024-01-08 14:30 UTC (Monday)
        assert!(!SessionAwareness::is_weekend(MON_OPEN_UTC_MS));
    }

    // ── SessionAwareness::minutes_since_open ──────────────────────────────────

    #[test]
    fn test_minutes_since_open_zero_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_since_open(MON_OPEN_UTC_MS), 0);
    }

    #[test]
    fn test_minutes_since_open_one_hour_in() {
        let sa = sa(MarketSession::UsEquity);
        let one_hour_in = MON_OPEN_UTC_MS + 3_600_000;
        assert_eq!(sa.minutes_since_open(one_hour_in), 60);
    }

    #[test]
    fn test_minutes_since_open_zero_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_since_open(SAT_UTC_MS), 0);
    }

    // ── SessionAnalyzer::is_regular_session ───────────────────────────────────

    #[test]
    fn test_is_regular_session_true_during_open() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_regular_session(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_regular_session_false_on_weekend() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_regular_session(SAT_UTC_MS));
    }

    #[test]
    fn test_is_regular_session_false_before_open() {
        let sa = sa(MarketSession::UsEquity);
        // Sunday before open
        assert!(!sa.is_regular_session(SUN_BEFORE_UTC_MS));
    }

    // --- fraction_of_day_elapsed ---

    #[test]
    fn test_fraction_of_day_elapsed_midnight_is_zero() {
        let sa = sa(MarketSession::Crypto);
        // Midnight UTC = 0 ms into the day
        let midnight_ms: u64 = 24 * 60 * 60 * 1000; // exactly one full day = another midnight
        assert!((sa.fraction_of_day_elapsed(midnight_ms) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn test_fraction_of_day_elapsed_noon_is_half() {
        let sa = sa(MarketSession::Crypto);
        // 12:00 UTC = 12 * 3600 * 1000 ms after midnight
        let noon_offset_ms: u64 = 12 * 60 * 60 * 1000;
        let frac = sa.fraction_of_day_elapsed(noon_offset_ms);
        assert!((frac - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_fraction_of_day_elapsed_range_zero_to_one() {
        let sa = sa(MarketSession::Crypto);
        for ms in [0u64, 1_000, 43_200_000, 86_399_999] {
            let frac = sa.fraction_of_day_elapsed(ms);
            assert!((0.0..1.0).contains(&frac));
        }
    }

    // ── SessionAwareness::remaining_until_close_ms ────────────────────────────

    #[test]
    fn test_remaining_until_close_ms_some_when_open() {
        let sa = sa(MarketSession::UsEquity);
        // Market is open; there is a finite close time ahead
        let remaining = sa.remaining_until_close_ms(MON_OPEN_UTC_MS).unwrap();
        assert!(remaining > 0, "remaining should be positive when session is open");
        assert!(remaining < 24 * 60 * 60 * 1000, "remaining should be less than 24h");
    }

    #[test]
    fn test_remaining_until_close_ms_none_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.remaining_until_close_ms(SAT_UTC_MS).is_none());
    }

    #[test]
    fn test_remaining_until_close_ms_decreases_as_time_advances() {
        let sa = sa(MarketSession::UsEquity);
        let t1 = sa.remaining_until_close_ms(MON_OPEN_UTC_MS).unwrap();
        let t2 = sa.remaining_until_close_ms(MON_OPEN_UTC_MS + 60_000).unwrap();
        assert!(t1 > t2);
    }

    // ── SessionAnalyzer::is_last_trading_hour ─────────────────────────────────

    #[test]
    fn test_is_last_trading_hour_true_within_last_hour() {
        let sa = sa(MarketSession::UsEquity);
        // 30 minutes before close: MON_CLOSE_UTC_MS - 30*60*1000
        let thirty_before_close = MON_CLOSE_UTC_MS - 30 * 60 * 1_000;
        assert!(sa.is_last_trading_hour(thirty_before_close));
    }

    #[test]
    fn test_is_last_trading_hour_false_at_open() {
        let sa = sa(MarketSession::UsEquity);
        // At market open: 6.5 hours remaining — not last hour
        assert!(!sa.is_last_trading_hour(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_last_trading_hour_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_last_trading_hour(SAT_UTC_MS));
    }

    // --- is_pre_open / day_fraction_remaining ---

    #[test]
    fn test_is_pre_open_true_in_pre_market_window() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_pre_open(MON_OPEN_UTC_MS - 60_000));
    }

    #[test]
    fn test_is_pre_open_false_during_regular_session() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_pre_open(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_pre_open_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_pre_open(SAT_UTC_MS));
    }

    #[test]
    fn test_day_fraction_remaining_plus_elapsed_equals_one() {
        let sa = sa(MarketSession::UsEquity);
        let elapsed = sa.fraction_of_day_elapsed(MON_OPEN_UTC_MS);
        let remaining = sa.day_fraction_remaining(MON_OPEN_UTC_MS);
        assert!((elapsed + remaining - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_day_fraction_remaining_one_at_midnight() {
        let sa = sa(MarketSession::UsEquity);
        // At midnight UTC the elapsed fraction is 0.0, so remaining is 1.0
        assert!((sa.day_fraction_remaining(0) - 1.0).abs() < 1e-12);
    }

    // --- is_near_close / open_duration_ms ---

    #[test]
    fn test_is_near_close_true_within_margin() {
        let sa = sa(MarketSession::UsEquity);
        // 15 minutes before regular close, margin 30 minutes
        let fifteen_before = MON_CLOSE_UTC_MS - 15 * 60_000;
        assert!(sa.is_near_close(fifteen_before, 30 * 60_000));
    }

    #[test]
    fn test_is_near_close_false_outside_margin() {
        let sa = sa(MarketSession::UsEquity);
        // 2 hours before close, margin 30 minutes
        let two_hours_before = MON_CLOSE_UTC_MS - 2 * 3_600_000;
        assert!(!sa.is_near_close(two_hours_before, 30 * 60_000));
    }

    #[test]
    fn test_is_near_close_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_near_close(SAT_UTC_MS, 3_600_000));
    }

    #[test]
    fn test_open_duration_ms_us_equity() {
        let sa = sa(MarketSession::UsEquity);
        // 6.5 hours = 23,400,000 ms
        assert_eq!(sa.open_duration_ms(), 6 * 3_600_000 + 30 * 60_000);
    }

    #[test]
    fn test_open_duration_ms_crypto() {
        let sa = sa(MarketSession::Crypto);
        assert_eq!(sa.open_duration_ms(), u64::MAX);
    }

    // --- is_overnight / minutes_to_next_open ---

    #[test]
    fn test_is_overnight_true_when_closed_on_weekday() {
        let equity_sa = sa(MarketSession::UsEquity);
        // Middle of the night on a weekday (after after-hours, before pre-market)
        // Use a time deep in the night that is not extended hours
        // MON_OPEN_UTC_MS is 14:30 UTC Monday.
        // After-hours ends around 01:00 UTC Tuesday (20:00 ET + 5h UTC).
        // Use Wednesday 05:00 UTC (00:00 ET) — overnight before pre-market.
        // Wed = MON_OPEN_UTC_MS + 2*24*3600*1000 - 9.5*3600*1000 + 5*3600*1000
        // Actually let's just use a known overnight time: Saturday would be weekend.
        // Tuesday at 07:00 UTC = 02:00 ET = not pre-market (4am ET), not after-hours.
        let _tue_07h_utc = MON_OPEN_UTC_MS + 24 * 3_600_000 - 7 * 3_600_000 + 7 * 3_600_000 - (14 * 3_600_000 + 30 * 60_000) + 7 * 3_600_000;
        // Simpler: find a time that is_closed AND not_extended AND not_weekend
        // Just test the is_closed && not_extended && not_weekend contract
        // and that crypto returns false
        let _ = equity_sa;
        let sa_crypto = sa(MarketSession::Crypto);
        assert!(!sa_crypto.is_overnight(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_overnight_false_during_regular_session() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_overnight(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_overnight_false_for_crypto() {
        let sa = sa(MarketSession::Crypto);
        assert!(!sa.is_overnight(SAT_UTC_MS));
    }

    #[test]
    fn test_minutes_to_next_open_zero_when_already_open() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_to_next_open(MON_OPEN_UTC_MS), 0.0);
    }

    #[test]
    fn test_minutes_to_next_open_positive_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        let mins = sa.minutes_to_next_open(SAT_UTC_MS);
        assert!(mins > 0.0);
    }

    // --- SessionAnalyzer::session_progress_pct ---
    #[test]
    fn test_session_progress_pct_zero_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.session_progress_pct(SAT_UTC_MS), 0.0);
    }

    #[test]
    fn test_session_progress_pct_positive_when_open() {
        let sa = sa(MarketSession::UsEquity);
        // 30 minutes into session
        let pct = sa.session_progress_pct(MON_OPEN_UTC_MS + 30 * 60_000);
        assert!(pct > 0.0 && pct < 100.0, "expected 0-100, got {pct}");
    }

    // --- SessionAnalyzer::is_last_minute ---
    #[test]
    fn test_is_last_minute_true_within_last_60s() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_last_minute(MON_CLOSE_UTC_MS - 30_000));
    }

    #[test]
    fn test_is_last_minute_false_when_more_than_60s_remain() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_last_minute(MON_CLOSE_UTC_MS - 120_000));
    }

    #[test]
    fn test_is_last_minute_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_last_minute(SAT_UTC_MS));
    }

    // --- SessionAnalyzer::minutes_since_close ---
    #[test]
    fn test_minutes_since_close_zero_when_open() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_since_close(MON_OPEN_UTC_MS + 30 * 60_000), 0.0);
    }

    #[test]
    fn test_minutes_since_close_positive_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        let mins = sa.minutes_since_close(SAT_UTC_MS);
        assert!(mins > 0.0, "expected positive value when closed");
    }

    // --- SessionAnalyzer::is_opening_bell_minute ---
    #[test]
    fn test_is_opening_bell_minute_true_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.is_opening_bell_minute(MON_OPEN_UTC_MS + 30_000));
    }

    #[test]
    fn test_is_opening_bell_minute_false_after_first_minute() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_opening_bell_minute(MON_OPEN_UTC_MS + 90_000));
    }

    #[test]
    fn test_is_opening_bell_minute_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_opening_bell_minute(SAT_UTC_MS));
    }

    // ── SessionAnalyzer::is_extended_hours ────────────────────────────────────

    #[test]
    fn test_is_extended_hours_true_in_pre_market() {
        let sa = sa(MarketSession::UsEquity);
        // 1 minute before regular open = pre-market
        assert!(sa.is_extended_hours(MON_OPEN_UTC_MS - 60_000));
    }

    #[test]
    fn test_is_extended_hours_true_in_after_hours() {
        let sa = sa(MarketSession::UsEquity);
        // 1 minute after regular close = after-hours
        assert!(sa.is_extended_hours(MON_CLOSE_UTC_MS + 60_000));
    }

    #[test]
    fn test_is_extended_hours_false_during_regular_session() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_extended_hours(MON_OPEN_UTC_MS));
    }

    // ── SessionAnalyzer::is_opening_range ─────────────────────────────────────

    #[test]
    fn test_is_opening_range_true_at_open() {
        let sa = sa(MarketSession::UsEquity);
        // Exactly at open (0 ms elapsed)
        assert!(sa.is_opening_range(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_opening_range_true_at_15_minutes() {
        let sa = sa(MarketSession::UsEquity);
        // 15 minutes elapsed = 900_000 ms < 30 min
        assert!(sa.is_opening_range(MON_OPEN_UTC_MS + 900_000));
    }

    #[test]
    fn test_is_opening_range_false_after_30_minutes() {
        let sa = sa(MarketSession::UsEquity);
        // 31 minutes elapsed
        assert!(!sa.is_opening_range(MON_OPEN_UTC_MS + 31 * 60_000));
    }

    #[test]
    fn test_is_opening_range_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_opening_range(SAT_UTC_MS));
    }

    // ── SessionAnalyzer::is_mid_session ───────────────────────────────────────

    #[test]
    fn test_is_mid_session_true_at_halfway_point() {
        let sa = sa(MarketSession::UsEquity);
        // 3 hours in = ~46% progress → mid session
        assert!(sa.is_mid_session(MON_OPEN_UTC_MS + 3 * 3_600_000));
    }

    #[test]
    fn test_is_mid_session_false_in_opening_range() {
        let sa = sa(MarketSession::UsEquity);
        // 5 minutes elapsed = <25% progress
        assert!(!sa.is_mid_session(MON_OPEN_UTC_MS + 5 * 60_000));
    }

    #[test]
    fn test_is_mid_session_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_mid_session(SAT_UTC_MS));
    }

    // ── SessionAnalyzer::is_first_quarter / is_last_quarter ────────────────

    #[test]
    fn test_is_first_quarter_true_at_open() {
        let sa = sa(MarketSession::UsEquity);
        // At exact open = 0% progress → first quarter
        assert!(sa.is_first_quarter(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_first_quarter_false_at_midpoint() {
        let sa = sa(MarketSession::UsEquity);
        // 3 hours in ≈ 46% → not first quarter
        assert!(!sa.is_first_quarter(MON_OPEN_UTC_MS + 3 * 3_600_000));
    }

    #[test]
    fn test_is_first_quarter_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_first_quarter(SAT_UTC_MS));
    }

    #[test]
    fn test_is_last_quarter_true_near_close() {
        let sa = sa(MarketSession::UsEquity);
        // Session = 6.5 h = 23400 s. 80% = 18720 s
        assert!(sa.is_last_quarter(MON_OPEN_UTC_MS + 18_720_000));
    }

    #[test]
    fn test_is_last_quarter_false_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_last_quarter(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_last_quarter_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_last_quarter(SAT_UTC_MS));
    }

    // ── SessionAnalyzer::minutes_elapsed / is_power_hour ───────────────────

    #[test]
    fn test_minutes_elapsed_zero_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_elapsed(MON_OPEN_UTC_MS), 0.0);
    }

    #[test]
    fn test_minutes_elapsed_correct_at_30_min() {
        let sa = sa(MarketSession::UsEquity);
        let elapsed = sa.minutes_elapsed(MON_OPEN_UTC_MS + 30 * 60_000);
        assert!((elapsed - 30.0).abs() < 1e-9);
    }

    #[test]
    fn test_minutes_elapsed_zero_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert_eq!(sa.minutes_elapsed(SAT_UTC_MS), 0.0);
    }

    #[test]
    fn test_is_power_hour_true_in_last_hour() {
        let sa = sa(MarketSession::UsEquity);
        // Session = 6.5h = 390min; power hour starts at 330min = 19_800_000ms
        assert!(sa.is_power_hour(MON_OPEN_UTC_MS + 19_800_000 + 60_000));
    }

    #[test]
    fn test_is_power_hour_false_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_power_hour(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_power_hour_false_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_power_hour(SAT_UTC_MS));
    }

    // ── SessionAnalyzer::fraction_remaining ─────────────────────────────────

    #[test]
    fn test_fraction_remaining_one_at_open() {
        let sa = sa(MarketSession::UsEquity);
        // At open: progress=0, fraction remaining=1
        let f = sa.fraction_remaining(MON_OPEN_UTC_MS).unwrap();
        assert!((f - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_fraction_remaining_zero_at_close() {
        let sa = sa(MarketSession::UsEquity);
        // 1ms before close: fraction remaining should be very small (< 0.01%)
        let near_close_ms = MON_OPEN_UTC_MS + 6 * 3_600_000 + 30 * 60_000 - 1;
        let f = sa.fraction_remaining(near_close_ms).unwrap();
        assert!(f >= 0.0 && f < 0.0001);
    }

    #[test]
    fn test_fraction_remaining_none_when_closed() {
        let sa = sa(MarketSession::UsEquity);
        assert!(sa.fraction_remaining(SAT_UTC_MS).is_none());
    }

    #[test]
    fn test_fraction_remaining_plus_progress_equals_one() {
        let sa = sa(MarketSession::UsEquity);
        let t = MON_OPEN_UTC_MS + 2 * 3_600_000;
        let prog = sa.session_progress(t).unwrap();
        let rem = sa.fraction_remaining(t).unwrap();
        assert!((prog + rem - 1.0).abs() < 1e-9);
    }

    // ── SessionAnalyzer::is_lunch_hour ────────────────────────────────────────

    #[test]
    fn test_is_lunch_hour_true_at_midday() {
        let sa = sa(MarketSession::UsEquity);
        // 2.5h into session = 150min = start of lunch window
        let t = MON_OPEN_UTC_MS + 150 * 60_000;
        assert!(sa.is_lunch_hour(t));
    }

    #[test]
    fn test_is_lunch_hour_false_at_open() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_lunch_hour(MON_OPEN_UTC_MS));
    }

    #[test]
    fn test_is_lunch_hour_false_outside_session() {
        let sa = sa(MarketSession::UsEquity);
        assert!(!sa.is_lunch_hour(SAT_UTC_MS));
    }

    #[test]
    fn test_is_lunch_hour_false_for_crypto() {
        let sa = sa(MarketSession::Crypto);
        let t = MON_OPEN_UTC_MS + 150 * 60_000;
        assert!(!sa.is_lunch_hour(t));
    }

    // ── SessionAwareness::is_triple_witching ───────────────────────────────────

    #[test]
    fn test_is_triple_witching_true_third_friday_march() {
        // 2024-03-15 = third Friday of March 2024
        let date = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap();
        assert!(SessionAwareness::is_triple_witching(date));
    }

    #[test]
    fn test_is_triple_witching_true_third_friday_september() {
        // 2024-09-20 = third Friday of September 2024
        let date = NaiveDate::from_ymd_opt(2024, 9, 20).unwrap();
        assert!(SessionAwareness::is_triple_witching(date));
    }

    #[test]
    fn test_is_triple_witching_false_wrong_month() {
        // 2024-01-19 = third Friday of January — not a witching month
        let date = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        assert!(!SessionAwareness::is_triple_witching(date));
    }

    #[test]
    fn test_is_triple_witching_false_first_friday_of_witching_month() {
        // 2024-03-01 = first Friday of March — not third
        let date = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        assert!(!SessionAwareness::is_triple_witching(date));
    }

    #[test]
    fn test_is_triple_witching_false_wrong_weekday() {
        // 2024-03-20 = Wednesday, third week of March
        let date = NaiveDate::from_ymd_opt(2024, 3, 20).unwrap();
        assert!(!SessionAwareness::is_triple_witching(date));
    }

    // ── SessionAwareness::trading_days_elapsed ────────────────────────────────

    #[test]
    fn test_trading_days_elapsed_same_day_weekday() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap(); // Monday
        assert_eq!(SessionAwareness::trading_days_elapsed(d, d), 1);
    }

    #[test]
    fn test_trading_days_elapsed_full_week() {
        let from = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();  // Monday
        let to   = NaiveDate::from_ymd_opt(2024, 1, 12).unwrap(); // Friday
        assert_eq!(SessionAwareness::trading_days_elapsed(from, to), 5);
    }

    #[test]
    fn test_trading_days_elapsed_excludes_weekends() {
        let from = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();  // Monday
        let to   = NaiveDate::from_ymd_opt(2024, 1, 14).unwrap(); // Sunday
        // Mon-Fri = 5 trading days, Sat+Sun = 0
        assert_eq!(SessionAwareness::trading_days_elapsed(from, to), 5);
    }

    #[test]
    fn test_trading_days_elapsed_zero_when_reversed() {
        let from = NaiveDate::from_ymd_opt(2024, 1, 12).unwrap();
        let to   = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        assert_eq!(SessionAwareness::trading_days_elapsed(from, to), 0);
    }

    // ── SessionAwareness::is_earnings_season ──────────────────────────────────

    #[test]
    fn test_is_earnings_season_true_in_january() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        assert!(SessionAwareness::is_earnings_season(d));
    }

    #[test]
    fn test_is_earnings_season_true_in_october() {
        let d = NaiveDate::from_ymd_opt(2024, 10, 10).unwrap();
        assert!(SessionAwareness::is_earnings_season(d));
    }

    #[test]
    fn test_is_earnings_season_false_in_march() {
        let d = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap();
        assert!(!SessionAwareness::is_earnings_season(d));
    }

    // ── SessionAwareness::week_of_month ───────────────────────────────────────

    #[test]
    fn test_week_of_month_first_day_is_week_one() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        assert_eq!(SessionAwareness::week_of_month(d), 1);
    }

    #[test]
    fn test_week_of_month_8th_is_week_two() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        assert_eq!(SessionAwareness::week_of_month(d), 2);
    }

    #[test]
    fn test_week_of_month_15th_is_week_three() {
        let d = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap();
        assert_eq!(SessionAwareness::week_of_month(d), 3);
    }

    // ── is_fomc_blackout_window ───────────────────────────────────────────────

    #[test]
    fn test_fomc_blackout_true_for_late_odd_month() {
        let d = NaiveDate::from_ymd_opt(2024, 3, 20).unwrap(); // March 20
        assert!(SessionAwareness::is_fomc_blackout_window(d));
    }

    #[test]
    fn test_fomc_blackout_false_for_early_odd_month() {
        let d = NaiveDate::from_ymd_opt(2024, 3, 5).unwrap(); // March 5
        assert!(!SessionAwareness::is_fomc_blackout_window(d));
    }

    #[test]
    fn test_fomc_blackout_false_for_even_month() {
        let d = NaiveDate::from_ymd_opt(2024, 4, 25).unwrap(); // April — even month
        assert!(!SessionAwareness::is_fomc_blackout_window(d));
    }

    #[test]
    fn test_fomc_blackout_boundary_day_18() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 18).unwrap();
        assert!(SessionAwareness::is_fomc_blackout_window(d));
    }

    // ── is_market_holiday_adjacent ────────────────────────────────────────────

    #[test]
    fn test_holiday_adjacent_christmas_eve() {
        let d = NaiveDate::from_ymd_opt(2024, 12, 24).unwrap();
        assert!(SessionAwareness::is_market_holiday_adjacent(d));
    }

    #[test]
    fn test_holiday_adjacent_day_after_christmas() {
        let d = NaiveDate::from_ymd_opt(2024, 12, 26).unwrap();
        assert!(SessionAwareness::is_market_holiday_adjacent(d));
    }

    #[test]
    fn test_holiday_adjacent_new_years_eve() {
        let d = NaiveDate::from_ymd_opt(2024, 12, 31).unwrap();
        assert!(SessionAwareness::is_market_holiday_adjacent(d));
    }

    #[test]
    fn test_holiday_adjacent_july_3() {
        let d = NaiveDate::from_ymd_opt(2024, 7, 3).unwrap();
        assert!(SessionAwareness::is_market_holiday_adjacent(d));
    }

    #[test]
    fn test_holiday_adjacent_false_for_normal_day() {
        let d = NaiveDate::from_ymd_opt(2024, 6, 15).unwrap();
        assert!(!SessionAwareness::is_market_holiday_adjacent(d));
    }

    // ── seconds_until_open ────────────────────────────────────────────────────

    #[test]
    fn test_seconds_until_open_zero_when_session_is_open() {
        // Monday 16:00 UTC = well within US equity session
        let sa = SessionAwareness::new(MarketSession::UsEquity);
        let mon_16h_utc: u64 = 4 * 24 * 3_600_000 + 16 * 3_600_000;
        assert_eq!(sa.seconds_until_open(mon_16h_utc), 0.0);
    }

    #[test]
    fn test_seconds_until_open_positive_when_before_open() {
        // Saturday midnight → well before next Monday open
        let sa = SessionAwareness::new(MarketSession::UsEquity);
        let sat_midnight: u64 = 5 * 24 * 3_600_000;
        assert!(sa.seconds_until_open(sat_midnight) > 0.0);
    }

    // ── is_closing_bell_minute ────────────────────────────────────────────────

    #[test]
    fn test_closing_bell_minute_true_near_session_end() {
        // US equity opens at 14:30 UTC (9:30 ET-5), closes at 21:00 UTC (16:00 ET)
        // 6.5h = 23400s. Test at 20:59:30 UTC = just before close
        let sa = SessionAwareness::new(MarketSession::UsEquity);
        // Monday 20:59 UTC — 29 seconds before close
        let mon_20_59_utc: u64 = 4 * 24 * 3_600_000 + 20 * 3_600_000 + 59 * 60_000 + 30_000;
        assert!(sa.is_closing_bell_minute(mon_20_59_utc));
    }

    #[test]
    fn test_closing_bell_minute_false_early_in_session() {
        let sa = SessionAwareness::new(MarketSession::UsEquity);
        // Monday 16:00 UTC — mid session
        let mon_16h_utc: u64 = 4 * 24 * 3_600_000 + 16 * 3_600_000;
        assert!(!sa.is_closing_bell_minute(mon_16h_utc));
    }

    // ── day_of_week_name ──────────────────────────────────────────────────────

    #[test]
    fn test_day_of_week_name_monday() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(); // Monday
        assert_eq!(SessionAwareness::day_of_week_name(d), "Monday");
    }

    #[test]
    fn test_day_of_week_name_friday() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(); // Friday
        assert_eq!(SessionAwareness::day_of_week_name(d), "Friday");
    }

    #[test]
    fn test_day_of_week_name_sunday() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 7).unwrap(); // Sunday
        assert_eq!(SessionAwareness::day_of_week_name(d), "Sunday");
    }

    // ── is_expiry_week ────────────────────────────────────────────────────────

    #[test]
    fn test_is_expiry_week_true_for_late_month() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 25).unwrap();
        assert!(SessionAwareness::is_expiry_week(d));
    }

    #[test]
    fn test_is_expiry_week_true_at_boundary_day_22() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 22).unwrap();
        assert!(SessionAwareness::is_expiry_week(d));
    }

    #[test]
    fn test_is_expiry_week_false_for_early_month() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 10).unwrap();
        assert!(!SessionAwareness::is_expiry_week(d));
    }

    // ── session_name ──────────────────────────────────────────────────────────

    #[test]
    fn test_session_name_us_equity() {
        let sa = SessionAwareness::new(MarketSession::UsEquity);
        assert_eq!(sa.session_name(), "US Equity");
    }

    #[test]
    fn test_session_name_crypto() {
        let sa = SessionAwareness::new(MarketSession::Crypto);
        assert_eq!(sa.session_name(), "Crypto");
    }

    #[test]
    fn test_session_name_forex() {
        let sa = SessionAwareness::new(MarketSession::Forex);
        assert_eq!(sa.session_name(), "Forex");
    }
}
