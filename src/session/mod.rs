//! Market session awareness — trading hours, holidays, status transitions.
//!
//! ## Responsibility
//! Classify a UTC timestamp into a market trading status for a given session
//! (equity, crypto, forex). Enables downstream filtering of ticks by session.
//!
//! ## Guarantees
//! - Pure functions: SessionAwareness::status() is deterministic and stateless
//! - Non-panicking: all operations return Result or TradingStatus

use crate::error::StreamError;

/// Broad category of market session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MarketSession {
    /// US equity market (NYSE/NASDAQ) — 9:30–16:00 ET Mon–Fri.
    UsEquity,
    /// Crypto market — 24/7/365.
    Crypto,
    /// Forex market — 24/5, Sunday 17:00 ET – Friday 17:00 ET.
    Forex,
}

/// Trading status at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TradingStatus {
    /// Regular trading hours.
    Open,
    /// Pre-market or after-hours for equity; equivalent to Open for crypto.
    Extended,
    /// Market is closed.
    Closed,
}

/// Determines trading status for a market session.
pub struct SessionAwareness {
    session: MarketSession,
}

impl SessionAwareness {
    pub fn new(session: MarketSession) -> Self {
        Self { session }
    }

    /// Classify a UTC timestamp (ms) into a trading status.
    pub fn status(&self, utc_ms: u64) -> Result<TradingStatus, StreamError> {
        match self.session {
            MarketSession::Crypto => Ok(TradingStatus::Open),
            MarketSession::UsEquity => self.us_equity_status(utc_ms),
            MarketSession::Forex => self.forex_status(utc_ms),
        }
    }

    pub fn session(&self) -> MarketSession { self.session }

    fn us_equity_status(&self, utc_ms: u64) -> Result<TradingStatus, StreamError> {
        // ET = UTC - 5h (EST). Compute time-of-day in ET by taking UTC time-within-day
        // and subtracting 5h with modular wraparound (avoids epoch-day boundary errors).
        // For production DST support, integrate chrono-tz.
        const DAY_MS: u64 = 24 * 3600 * 1000;
        const ET_OFFSET_MS: u64 = 5 * 3600 * 1000; // EST = UTC-5
        let utc_day_ms = utc_ms % DAY_MS;
        let day_ms = (utc_day_ms + DAY_MS - ET_OFFSET_MS) % DAY_MS;
        let day_of_week = (utc_ms / DAY_MS + 4) % 7; // 0=Sun, 1=Mon, ..., 6=Sat

        // Weekend: Saturday=6, Sunday=0
        if day_of_week == 0 || day_of_week == 6 {
            return Ok(TradingStatus::Closed);
        }

        let open_ms = (9 * 3600 + 30 * 60) * 1000;   // 9:30 ET
        let close_ms = 16 * 3600 * 1000;              // 16:00 ET
        let pre_ms = 4 * 3600 * 1000;                 // 4:00 ET
        let post_ms = 20 * 3600 * 1000;               // 20:00 ET

        if day_ms >= open_ms && day_ms < close_ms {
            Ok(TradingStatus::Open)
        } else if (day_ms >= pre_ms && day_ms < open_ms) || (day_ms >= close_ms && day_ms < post_ms) {
            Ok(TradingStatus::Extended)
        } else {
            Ok(TradingStatus::Closed)
        }
    }

    fn forex_status(&self, utc_ms: u64) -> Result<TradingStatus, StreamError> {
        // Forex: open Sunday 22:00 UTC – Friday 22:00 UTC (approximately)
        let day_of_week = (utc_ms / (24 * 3600 * 1000) + 4) % 7; // 0=Sun, 1=Mon, ..., 6=Sat
        let day_ms = utc_ms % (24 * 3600 * 1000);
        let hour_22_ms = 22 * 3600 * 1000;

        // Fully closed: Saturday entire day, Sunday before 22:00
        if day_of_week == 6 {
            return Ok(TradingStatus::Closed);
        }
        if day_of_week == 0 && day_ms < hour_22_ms {
            return Ok(TradingStatus::Closed);
        }
        // Friday after 22:00 UTC — also closed
        if day_of_week == 5 && day_ms >= hour_22_ms {
            return Ok(TradingStatus::Closed);
        }
        Ok(TradingStatus::Open)
    }
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

    // Reference Monday 2024-01-08 14:30 UTC = 09:30 ET (market open)
    // UTC ms: 1704724200000 — Mon Jan 08 2024 14:30:00 UTC
    const MON_OPEN_UTC_MS: u64 = 1704724200000;
    // Monday 21:00 UTC = 16:00 ET (market close, extended hours start)
    const MON_CLOSE_UTC_MS: u64 = 1704747600000;
    // Saturday 2024-01-13
    const SAT_UTC_MS: u64 = 1705104000000;
    // Sunday 2024-01-07 10:00 UTC (before 22:00 UTC, forex closed)
    const SUN_BEFORE_UTC_MS: u64 = 1704621600000;

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
    fn test_us_equity_open_during_market_hours() {
        let sa = sa(MarketSession::UsEquity);
        // 14:30 UTC = 09:30 ET Monday
        assert_eq!(sa.status(MON_OPEN_UTC_MS).unwrap(), TradingStatus::Open);
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
        // Monday 09:00 UTC = 04:00 ET (pre-market) = 1704672000000 + 9*3600*1000
        let pre_ms: u64 = 1704704400000; // Mon Jan 08 2024 09:00 UTC
        let status = sa.status(pre_ms).unwrap();
        assert!(status == TradingStatus::Extended || status == TradingStatus::Open);
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
}
