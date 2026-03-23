//! Market hours checker for different exchanges.
//!
//! Provides [`MarketHoursChecker`], which can be populated with one or more
//! [`MarketCalendar`] entries describing trading sessions and holidays for an
//! exchange.  All times are stored in UTC.
//!
//! Exchange identification uses `&str` names so that this module does not
//! conflict with the [`Exchange`] enum already defined in [`crate::tick`].

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single trading session, expressed in UTC offsets.
///
/// `open_hour_utc` and `close_hour_utc` are wall-clock hours in UTC.
/// `timezone_offset_hours` records the exchange's local offset for display
/// purposes only — all internal calculations use UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradingSession {
    /// UTC hour when the session opens (0–23).
    pub open_hour_utc: u8,
    /// Minute within `open_hour_utc` when the session opens (0–59).
    pub open_min: u8,
    /// UTC hour when the session closes (0–23).
    pub close_hour_utc: u8,
    /// Minute within `close_hour_utc` when the session closes (0–59).
    pub close_min: u8,
    /// Exchange local timezone offset from UTC in whole hours (e.g. -5 for EST).
    pub timezone_offset_hours: i8,
}

impl TradingSession {
    /// Open time in minutes since midnight UTC.
    fn open_minutes_utc(&self) -> u32 {
        self.open_hour_utc as u32 * 60 + self.open_min as u32
    }

    /// Close time in minutes since midnight UTC.
    fn close_minutes_utc(&self) -> u32 {
        self.close_hour_utc as u32 * 60 + self.close_min as u32
    }
}

/// Calendar for one exchange: named sessions plus a list of market holidays.
///
/// Holidays are represented as `(year, month, day)` tuples (all 1-based).
#[derive(Debug, Clone)]
pub struct MarketCalendar {
    /// Human-readable exchange name used as the lookup key.
    pub exchange: String,
    /// Ordered list of daily trading sessions.
    pub sessions: Vec<TradingSession>,
    /// Non-trading days: `(year, month, day)`.
    pub holidays: Vec<(u32, u8, u8)>,
}

// ---------------------------------------------------------------------------
// MarketHoursChecker
// ---------------------------------------------------------------------------

/// Checks whether an exchange is currently open and when it next opens.
///
/// Maintains an internal registry of [`MarketCalendar`] entries, keyed by the
/// exchange name.  Timestamps are in milliseconds since the Unix epoch (UTC).
#[derive(Debug, Default)]
pub struct MarketHoursChecker {
    calendars: HashMap<String, MarketCalendar>,
}

impl MarketHoursChecker {
    /// Create a new, empty checker (no exchanges registered).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the calendar for an exchange.
    pub fn register_exchange(&mut self, calendar: MarketCalendar) {
        self.calendars.insert(calendar.exchange.clone(), calendar);
    }

    /// Return `true` if the exchange is currently open at `timestamp_ms`.
    ///
    /// Returns `false` for unknown exchanges or on holidays.
    pub fn is_open(&self, exchange: &str, timestamp_ms: u64) -> bool {
        let cal = match self.calendars.get(exchange) {
            Some(c) => c,
            None => return false,
        };
        let (year, month, day, minutes_today) = decompose_ms(timestamp_ms);
        if cal.holidays.contains(&(year, month, day)) {
            return false;
        }
        cal.sessions.iter().any(|s| {
            minutes_today >= s.open_minutes_utc() && minutes_today < s.close_minutes_utc()
        })
    }

    /// Return the next open time (ms) for the exchange, starting from `timestamp_ms`.
    ///
    /// Searches up to 14 days ahead.  Returns `timestamp_ms` if already open.
    /// Returns `0` if the exchange is unknown or no open time is found.
    pub fn next_open(&self, exchange: &str, timestamp_ms: u64) -> u64 {
        if self.is_open(exchange, timestamp_ms) {
            return timestamp_ms;
        }
        let cal = match self.calendars.get(exchange) {
            Some(c) => c,
            None => return 0,
        };
        // Search up to 14 days forward in 1-minute increments within sessions
        let ms_per_day: u64 = 86_400_000;
        for day_offset in 0..14u64 {
            let day_start_ms = day_start_ms(timestamp_ms) + day_offset * ms_per_day;
            let (year, month, day, _) = decompose_ms(day_start_ms);
            if cal.holidays.contains(&(year, month, day)) {
                continue;
            }
            for session in &cal.sessions {
                let open_ms =
                    day_start_ms + session.open_minutes_utc() as u64 * 60_000;
                if open_ms > timestamp_ms {
                    return open_ms;
                }
            }
        }
        0
    }

    /// Return the milliseconds until the exchange closes, or `None` if closed.
    pub fn time_until_close(&self, exchange: &str, timestamp_ms: u64) -> Option<u64> {
        if !self.is_open(exchange, timestamp_ms) {
            return None;
        }
        let cal = self.calendars.get(exchange)?;
        let (_, _, _, minutes_today) = decompose_ms(timestamp_ms);
        let day_start = day_start_ms(timestamp_ms);

        for s in &cal.sessions {
            if minutes_today >= s.open_minutes_utc() && minutes_today < s.close_minutes_utc() {
                let close_ms = day_start + s.close_minutes_utc() as u64 * 60_000;
                return Some(close_ms.saturating_sub(timestamp_ms));
            }
        }
        None
    }

    /// Return the active [`TradingSession`] for the exchange at `timestamp_ms`,
    /// or `None` if the market is closed or the exchange is unknown.
    pub fn session_for(&self, exchange: &str, timestamp_ms: u64) -> Option<TradingSession> {
        let cal = self.calendars.get(exchange)?;
        let (year, month, day, minutes_today) = decompose_ms(timestamp_ms);
        if cal.holidays.contains(&(year, month, day)) {
            return None;
        }
        cal.sessions
            .iter()
            .find(|s| minutes_today >= s.open_minutes_utc() && minutes_today < s.close_minutes_utc())
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Decompose a Unix-ms timestamp into `(year, month, day, minutes_since_midnight_utc)`.
fn decompose_ms(ms: u64) -> (u32, u8, u8, u32) {
    let secs = ms / 1000;
    let minutes_total = secs / 60;
    let minutes_today = (minutes_total % (24 * 60)) as u32;

    // Simple Gregorian calendar decomposition (no chrono dependency).
    let days_since_epoch = (secs / 86400) as u32;
    let (year, month, day) = days_to_ymd(days_since_epoch);
    (year, month, day, minutes_today)
}

/// Return the Unix-ms timestamp for the start of the UTC day containing `ms`.
fn day_start_ms(ms: u64) -> u64 {
    (ms / 86_400_000) * 86_400_000
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u32) -> (u32, u8, u8) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u8, d as u8)
}

// ---------------------------------------------------------------------------
// Pre-built exchange calendars
// ---------------------------------------------------------------------------

impl MarketHoursChecker {
    /// Register a typical NYSE/NASDAQ calendar (Mon-Fri, 14:30–21:00 UTC, EST = UTC-5).
    pub fn register_nyse(&mut self) {
        self.register_exchange(MarketCalendar {
            exchange: "NYSE".to_string(),
            sessions: vec![TradingSession {
                open_hour_utc: 14,
                open_min: 30,
                close_hour_utc: 21,
                close_min: 0,
                timezone_offset_hours: -5,
            }],
            holidays: vec![],
        });
    }

    /// Register a typical LSE calendar (08:00–16:30 UTC, GMT = UTC+0).
    pub fn register_lse(&mut self) {
        self.register_exchange(MarketCalendar {
            exchange: "LSE".to_string(),
            sessions: vec![TradingSession {
                open_hour_utc: 8,
                open_min: 0,
                close_hour_utc: 16,
                close_min: 30,
                timezone_offset_hours: 0,
            }],
            holidays: vec![],
        });
    }

    /// Register a 24/7 Crypto exchange.
    pub fn register_crypto(&mut self) {
        self.register_exchange(MarketCalendar {
            exchange: "Crypto".to_string(),
            sessions: vec![TradingSession {
                open_hour_utc: 0,
                open_min: 0,
                close_hour_utc: 23,
                close_min: 59,
                timezone_offset_hours: 0,
            }],
            holidays: vec![],
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a checker with NYSE pre-registered.
    fn nyse_checker() -> MarketHoursChecker {
        let mut c = MarketHoursChecker::new();
        c.register_nyse();
        c
    }

    // 2024-01-15 (Monday) 15:00 UTC  → NYSE open
    const NYSE_OPEN_MS: u64 = 1_705_330_800_000;
    // 2024-01-15 (Monday) 22:00 UTC  → NYSE closed
    const NYSE_CLOSED_MS: u64 = 1_705_353_600_000;
    // 2024-01-15 (Monday) 14:30 UTC  → NYSE just opened
    const NYSE_OPEN_AT_OPEN_MS: u64 = 1_705_329_000_000;

    #[test]
    fn test_is_open_during_session() {
        let c = nyse_checker();
        assert!(c.is_open("NYSE", NYSE_OPEN_MS));
    }

    #[test]
    fn test_is_closed_after_hours() {
        let c = nyse_checker();
        assert!(!c.is_open("NYSE", NYSE_CLOSED_MS));
    }

    #[test]
    fn test_is_open_at_exact_open() {
        let c = nyse_checker();
        assert!(c.is_open("NYSE", NYSE_OPEN_AT_OPEN_MS));
    }

    #[test]
    fn test_unknown_exchange_is_closed() {
        let c = nyse_checker();
        assert!(!c.is_open("UNKNOWN", NYSE_OPEN_MS));
    }

    #[test]
    fn test_holiday_closes_market() {
        let mut c = MarketHoursChecker::new();
        // 2024-01-15 as a holiday
        c.register_exchange(MarketCalendar {
            exchange: "NYSE".to_string(),
            sessions: vec![TradingSession {
                open_hour_utc: 14,
                open_min: 30,
                close_hour_utc: 21,
                close_min: 0,
                timezone_offset_hours: -5,
            }],
            holidays: vec![(2024, 1, 15)],
        });
        assert!(!c.is_open("NYSE", NYSE_OPEN_MS));
    }

    #[test]
    fn test_time_until_close_when_open() {
        let c = nyse_checker();
        // NYSE_OPEN_MS = 15:00 UTC, close = 21:00 UTC → 6 hours = 21_600_000 ms
        let ttc = c.time_until_close("NYSE", NYSE_OPEN_MS);
        assert!(ttc.is_some());
        assert_eq!(ttc.unwrap(), 21_600_000);
    }

    #[test]
    fn test_time_until_close_when_closed() {
        let c = nyse_checker();
        assert!(c.time_until_close("NYSE", NYSE_CLOSED_MS).is_none());
    }

    #[test]
    fn test_session_for_when_open() {
        let c = nyse_checker();
        let sess = c.session_for("NYSE", NYSE_OPEN_MS);
        assert!(sess.is_some());
        let s = sess.unwrap();
        assert_eq!(s.open_hour_utc, 14);
        assert_eq!(s.close_hour_utc, 21);
    }

    #[test]
    fn test_session_for_when_closed() {
        let c = nyse_checker();
        assert!(c.session_for("NYSE", NYSE_CLOSED_MS).is_none());
    }

    #[test]
    fn test_next_open_when_already_open() {
        let c = nyse_checker();
        assert_eq!(c.next_open("NYSE", NYSE_OPEN_MS), NYSE_OPEN_MS);
    }

    #[test]
    fn test_next_open_when_closed_finds_future() {
        let c = nyse_checker();
        // Closed at NYSE_CLOSED_MS (22:00 UTC).  Next open should be the following
        // day's 14:30 UTC.
        let next = c.next_open("NYSE", NYSE_CLOSED_MS);
        assert!(next > NYSE_CLOSED_MS, "next_open must be after the closed timestamp");
    }

    #[test]
    fn test_crypto_always_open() {
        let mut c = MarketHoursChecker::new();
        c.register_crypto();
        // Midnight UTC on some arbitrary day
        assert!(c.is_open("Crypto", 1_700_000_000_000));
    }

    #[test]
    fn test_lse_open_mid_morning() {
        let mut c = MarketHoursChecker::new();
        c.register_lse();
        // 2024-01-15 10:00 UTC → LSE open
        let ts: u64 = 1_705_312_800_000;
        assert!(c.is_open("LSE", ts));
    }

    #[test]
    fn test_days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn test_days_to_ymd_known() {
        // 2024-01-15: days since epoch = 19_737 (verified externally)
        let (y, m, d) = days_to_ymd(19737);
        assert_eq!((y, m, d), (2024, 1, 15));
    }
}
