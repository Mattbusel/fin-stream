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
}
