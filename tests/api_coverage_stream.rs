//! Additional tests covering public API paths not exercised by the existing test suite.
//!
//! Targets:
//! - `HealthMonitor::with_circuit_breaker_threshold` builder method
//! - `SessionAwareness::new` + `status` for all three `MarketSession` variants
//! - `LorentzTransform::beta()` and `gamma()` accessors
//! - `SpacetimePoint::new` field round-trip
//! - `MinMaxNormalizer::window_size()`, `is_empty()`, `reset()`
//! - `OhlcvAggregator::with_emit_empty_bars`
//! - `ReconnectPolicy::backoff_for_attempt` exponential growth
//! - `BookDelta::with_sequence` optional sequence number
//! - `TickNormalizer` returns error for unknown exchange via `FromStr`
//! - `Exchange::Display` and `FromStr` round-trip

use fin_stream::book::{BookDelta, BookSide};
use fin_stream::health::HealthMonitor;
use fin_stream::lorentz::{LorentzTransform, SpacetimePoint};
use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::session::{MarketSession, SessionAwareness, TradingStatus};
use fin_stream::tick::Exchange;
use fin_stream::ws::ReconnectPolicy;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::Duration;

// ── HealthMonitor ──────────────────────────────────────────────────────────

#[test]
fn test_health_monitor_circuit_breaker_threshold_zero_disables() {
    let m = HealthMonitor::new(1_000).with_circuit_breaker_threshold(0);
    m.register("feed-a", None);
    m.heartbeat("feed-a", 0).unwrap();
    // Stale many times — circuit must never open when threshold is 0.
    for i in 1..=20 {
        m.check_all(i * 5_000);
    }
    assert!(!m.is_circuit_open("feed-a"));
}

#[test]
fn test_health_monitor_circuit_opens_at_custom_threshold() {
    let m = HealthMonitor::new(1_000).with_circuit_breaker_threshold(2);
    m.register("feed-b", None);
    m.heartbeat("feed-b", 0).unwrap();
    m.check_all(5_000); // stale count = 1
    assert!(!m.is_circuit_open("feed-b"));
    m.check_all(10_000); // stale count = 2 — circuit opens
    assert!(m.is_circuit_open("feed-b"));
}

// ── SessionAwareness ───────────────────────────────────────────────────────

#[test]
fn test_session_crypto_always_open() {
    let sa = SessionAwareness::new(MarketSession::Crypto);
    // Any timestamp should return Open for crypto.
    assert_eq!(sa.status(0).unwrap(), TradingStatus::Open);
    assert_eq!(sa.status(u64::MAX / 2).unwrap(), TradingStatus::Open);
}

#[test]
fn test_session_forex_open_on_weekday() {
    // Monday 2026-03-16 12:00 UTC = 1_773_662_400_000 ms — Forex is open Mon-Fri.
    let mon_noon_utc_ms: u64 = 1_773_662_400_000;
    let sa = SessionAwareness::new(MarketSession::Forex);
    let status = sa.status(mon_noon_utc_ms).unwrap();
    assert_ne!(status, TradingStatus::Closed);
}

#[test]
fn test_session_us_equity_closed_on_weekend() {
    // Saturday 2026-03-14 12:00 UTC = 1_773_489_600_000 ms
    let sat_noon_utc_ms: u64 = 1_773_489_600_000;
    let sa = SessionAwareness::new(MarketSession::UsEquity);
    let status = sa.status(sat_noon_utc_ms).unwrap();
    assert_eq!(status, TradingStatus::Closed);
}

// ── LorentzTransform ───────────────────────────────────────────────────────

#[test]
fn test_lorentz_beta_accessor() {
    let lt = LorentzTransform::new(0.6).unwrap();
    assert!((lt.beta() - 0.6).abs() < 1e-12);
}

#[test]
fn test_lorentz_gamma_accessor() {
    // gamma = 1 / sqrt(1 - 0.6^2) = 1 / sqrt(0.64) = 1 / 0.8 = 1.25
    let lt = LorentzTransform::new(0.6).unwrap();
    assert!((lt.gamma() - 1.25).abs() < 1e-10);
}

#[test]
fn test_lorentz_beta_zero_is_identity() {
    let lt = LorentzTransform::new(0.0).unwrap();
    assert!((lt.beta() - 0.0).abs() < f64::EPSILON);
    assert!((lt.gamma() - 1.0).abs() < f64::EPSILON);
    let p = SpacetimePoint::new(3.0, 4.0);
    let transformed = lt.transform(p);
    assert!((transformed.t - p.t).abs() < 1e-10);
    assert!((transformed.x - p.x).abs() < 1e-10);
}

#[test]
fn test_lorentz_invalid_beta_rejected() {
    assert!(LorentzTransform::new(1.0).is_err());
    assert!(LorentzTransform::new(1.5).is_err());
    assert!(LorentzTransform::new(-0.1).is_err());
    assert!(LorentzTransform::new(f64::NAN).is_err());
}

// ── SpacetimePoint ─────────────────────────────────────────────────────────

#[test]
fn test_spacetime_point_fields_round_trip() {
    let p = SpacetimePoint::new(1.5, 2.7);
    assert!((p.t - 1.5).abs() < f64::EPSILON);
    assert!((p.x - 2.7).abs() < f64::EPSILON);
}

// ── MinMaxNormalizer ───────────────────────────────────────────────────────

#[test]
fn test_min_max_normalizer_window_size_accessor() {
    let n = MinMaxNormalizer::new(8).unwrap();
    assert_eq!(n.window_size(), 8);
}

#[test]
fn test_min_max_normalizer_is_empty_before_update() {
    let n = MinMaxNormalizer::new(4).unwrap();
    assert!(n.is_empty());
}

#[test]
fn test_min_max_normalizer_not_empty_after_update() {
    let mut n = MinMaxNormalizer::new(4).unwrap();
    n.update(dec!(10));
    assert!(!n.is_empty());
}

#[test]
fn test_min_max_normalizer_reset_clears_state() {
    let mut n = MinMaxNormalizer::new(4).unwrap();
    n.update(dec!(10));
    n.update(dec!(20));
    assert!(!n.is_empty());
    n.reset();
    assert!(n.is_empty());
    // After reset, normalize must fail (empty window).
    assert!(n.normalize(dec!(15)).is_err());
}

#[test]
fn test_min_max_normalizer_normalize_empty_returns_error() {
    let mut n = MinMaxNormalizer::new(4).unwrap();
    assert!(n.normalize(dec!(5)).is_err());
}

// ── OhlcvAggregator ────────────────────────────────────────────────────────

#[test]
fn test_ohlcv_aggregator_with_emit_empty_bars_flag() {
    // Constructing with emit_empty_bars should not panic; the flag is tested
    // separately in integration tests. Here we just verify the builder works.
    let _agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
        .expect("valid aggregator")
        .with_emit_empty_bars(true);
}

// ── ReconnectPolicy ────────────────────────────────────────────────────────

#[test]
fn test_reconnect_policy_backoff_grows_exponentially() {
    let p =
        ReconnectPolicy::new(10, Duration::from_millis(100), Duration::from_secs(60), 2.0).unwrap();
    let b0 = p.backoff_for_attempt(0);
    let b1 = p.backoff_for_attempt(1);
    let b2 = p.backoff_for_attempt(2);
    assert_eq!(b0, Duration::from_millis(100));
    assert_eq!(b1, Duration::from_millis(200));
    assert_eq!(b2, Duration::from_millis(400));
}

#[test]
fn test_reconnect_policy_backoff_capped_at_max() {
    let p = ReconnectPolicy::new(
        10,
        Duration::from_millis(100),
        Duration::from_millis(300),
        2.0,
    )
    .unwrap();
    // Attempt 10 would be 100 * 2^10 = 102400ms — must be capped at 300ms.
    let b = p.backoff_for_attempt(10);
    assert_eq!(b, Duration::from_millis(300));
}

#[test]
fn test_reconnect_policy_invalid_multiplier_rejected() {
    assert!(
        ReconnectPolicy::new(5, Duration::from_millis(100), Duration::from_secs(10), 0.5,).is_err()
    );
}

#[test]
fn test_reconnect_policy_zero_max_attempts_rejected() {
    assert!(
        ReconnectPolicy::new(0, Duration::from_millis(100), Duration::from_secs(10), 2.0,).is_err()
    );
}

// ── BookDelta ──────────────────────────────────────────────────────────────

#[test]
fn test_book_delta_with_sequence_attaches_sequence_number() {
    let d = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)).with_sequence(42);
    assert_eq!(d.sequence, Some(42));
}

#[test]
fn test_book_delta_without_sequence_is_none() {
    let d = BookDelta::new("BTC-USD", BookSide::Ask, dec!(50001), dec!(0));
    assert!(d.sequence.is_none());
}

// ── Exchange ───────────────────────────────────────────────────────────────

#[test]
fn test_exchange_display_round_trips_from_str() {
    for ex in [
        Exchange::Binance,
        Exchange::Coinbase,
        Exchange::Alpaca,
        Exchange::Polygon,
    ] {
        let s = ex.to_string();
        let parsed = Exchange::from_str(&s).unwrap();
        assert_eq!(parsed, ex);
    }
}

#[test]
fn test_exchange_from_str_case_insensitive() {
    assert_eq!(Exchange::from_str("BINANCE").unwrap(), Exchange::Binance);
    assert_eq!(Exchange::from_str("coinbase").unwrap(), Exchange::Coinbase);
}

#[test]
fn test_exchange_from_str_unknown_returns_error() {
    assert!(Exchange::from_str("kraken").is_err());
    assert!(Exchange::from_str("").is_err());
}
