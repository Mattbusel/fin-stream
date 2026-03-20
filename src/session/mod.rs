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

/// Determines trading status for a market session.
pub struct SessionAwareness {
    session: MarketSession,
}

impl SessionAwareness {
    /// Create a session classifier for the given market.
    pub fn new(session: MarketSession) -> Self {
        Self { session }
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
        self.status(utc_ms)
            .map(|s| s == TradingStatus::Closed)
            .unwrap_or(false)
    }

    /// Returns `true` if the session is currently in [`TradingStatus::Extended`] status.
    ///
    /// For [`MarketSession::Crypto`] this always returns `false` (crypto is always
    /// `Open`). For equity, `Extended` covers pre-market (4:00–9:30 ET) and
    /// after-hours (16:00–20:00 ET).
    pub fn is_extended(&self, utc_ms: u64) -> bool {
        self.status(utc_ms)
            .map(|s| s == TradingStatus::Extended)
            .unwrap_or(false)
    }

    /// Returns `true` if the session is currently in [`TradingStatus::Open`] status.
    ///
    /// Shorthand for `self.status(utc_ms).map(|s| s == TradingStatus::Open).unwrap_or(false)`.
    /// For [`MarketSession::Crypto`] this always returns `true`.
    pub fn is_open(&self, utc_ms: u64) -> bool {
        self.status(utc_ms)
            .map(|s| s == TradingStatus::Open)
            .unwrap_or(false)
    }

    /// Returns `true` if the session is currently tradeable: either
    /// [`TradingStatus::Open`] or [`TradingStatus::Extended`].
    ///
    /// For [`MarketSession::Crypto`] this always returns `true`. For equity,
    /// returns `true` during both regular hours and extended (pre/after-market)
    /// hours.
    pub fn is_market_hours(&self, utc_ms: u64) -> bool {
        self.status(utc_ms)
            .map(|s| s == TradingStatus::Open || s == TradingStatus::Extended)
            .unwrap_or(false)
    }

    /// Returns the number of whole minutes until the next [`TradingStatus::Open`] transition.
    ///
    /// Returns `0` if the session is already open. For [`MarketSession::Crypto`] always
    /// returns `0`. Useful for scheduling reconnect timers and pre-open setup.
    pub fn minutes_until_open(&self, utc_ms: u64) -> u64 {
        if self.is_open(utc_ms) {
            return 0;
        }
        let next = self.next_open_ms(utc_ms);
        if next <= utc_ms {
            return 0;
        }
        (next - utc_ms) / 60_000
    }

    /// Milliseconds until the next [`TradingStatus::Open`] transition.
    ///
    /// Returns `0` if the session is already `Open`. For
    /// [`MarketSession::Crypto`] always returns `0`.
    pub fn time_until_open_ms(&self, utc_ms: u64) -> u64 {
        self.next_open_ms(utc_ms).saturating_sub(utc_ms)
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

    /// Returns `true` if the equity session is currently in the pre-market window (4:00–9:30 ET).
    ///
    /// Always returns `false` for non-equity sessions. Pre-market is a subset of
    /// [`TradingStatus::Extended`]; this method distinguishes it from after-hours.
    pub fn is_pre_market(&self, utc_ms: u64) -> bool {
        if self.session != MarketSession::UsEquity {
            return false;
        }
        let secs = (utc_ms / 1000) as i64;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap_or_else(chrono::Utc::now);
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + chrono::Duration::seconds(et_offset_secs);
        let dow = et_dt.weekday();
        if dow == chrono::Weekday::Sat || dow == chrono::Weekday::Sun {
            return false;
        }
        if is_us_market_holiday(et_dt.date_naive()) {
            return false;
        }
        let t = et_dt.num_seconds_from_midnight() as u64;
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
        let secs = (utc_ms / 1000) as i64;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap_or_else(chrono::Utc::now);
        let et_offset_secs: i64 = if is_us_dst(utc_ms) { -4 * 3600 } else { -5 * 3600 };
        let et_dt = dt + chrono::Duration::seconds(et_offset_secs);
        let dow = et_dt.weekday();
        if dow == chrono::Weekday::Sat || dow == chrono::Weekday::Sun {
            return false;
        }
        if is_us_market_holiday(et_dt.date_naive()) {
            return false;
        }
        let t = et_dt.num_seconds_from_midnight() as u64;
        let market_close = 16 * 3600_u64;
        let post_close = 20 * 3600_u64;
        t >= market_close && t < post_close
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
}
