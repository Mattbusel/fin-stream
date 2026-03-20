//! Production-readiness tests.
//!
//! Covers three areas:
//! - Feed health monitoring: mark feed unhealthy, verify health check behaviour.
//! - Tick normalization edge cases: malformed messages, missing fields, all exchanges.
//! - Order book delta application: out-of-order deltas, duplicate sequence numbers.

use fin_stream::book::{BookDelta, BookSide, OrderBook, PriceLevel};
use fin_stream::health::{HealthMonitor, HealthStatus};
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use fin_stream::StreamError;
use rust_decimal_macros::dec;
use serde_json::json;

// ── Feed health monitoring ────────────────────────────────────────────────────

/// Register a feed, force it stale by advancing time past the threshold, and
/// confirm that `check_all` marks it unhealthy and returns an error.
#[test]
fn health_check_marks_feed_stale_after_threshold() {
    let monitor = HealthMonitor::new(2_000); // 2 s threshold
    monitor.register("BTC-USD", None);
    monitor.heartbeat("BTC-USD", 1_000_000).unwrap();

    // 1.5 s elapsed — within threshold, should be healthy.
    let errors = monitor.check_all(1_001_500);
    assert!(errors.is_empty(), "feed should still be healthy at 1.5 s");
    assert_eq!(
        monitor.get("BTC-USD").unwrap().status,
        HealthStatus::Healthy
    );

    // 3 s elapsed — past threshold, should be stale.
    let errors = monitor.check_all(1_003_000);
    assert_eq!(errors.len(), 1);
    assert!(matches!(&errors[0].1, StreamError::StaleFeed { feed_id, .. } if feed_id == "BTC-USD"));
    assert_eq!(monitor.get("BTC-USD").unwrap().status, HealthStatus::Stale);
}

/// After being marked stale, a new heartbeat must restore the feed to Healthy
/// and reset the consecutive_stale counter.
#[test]
fn health_check_recovery_after_stale() {
    let monitor = HealthMonitor::new(1_000);
    monitor.register("ETH-USD", None);
    monitor.heartbeat("ETH-USD", 1_000_000).unwrap();

    // Force stale.
    monitor.check_all(1_002_000);
    assert_eq!(monitor.get("ETH-USD").unwrap().status, HealthStatus::Stale);
    assert!(monitor.get("ETH-USD").unwrap().consecutive_stale > 0);

    // Recovery heartbeat.
    monitor.heartbeat("ETH-USD", 1_003_000).unwrap();
    let h = monitor.get("ETH-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Healthy);
    assert_eq!(h.consecutive_stale, 0);

    // Subsequent check at a time still within threshold should produce no errors.
    let errors = monitor.check_all(1_003_500);
    assert!(errors.is_empty());
}

/// A feed that has never received a heartbeat must NOT be counted as stale,
/// even if many staleness checks are run.
#[test]
fn health_check_unknown_feed_never_flagged_stale() {
    let monitor = HealthMonitor::new(500);
    monitor.register("SOL-USD", None);

    for t in 0..10u64 {
        let errors = monitor.check_all(1_000_000 + t * 1_000);
        assert!(
            errors.is_empty(),
            "unheartbeated feed must not be stale (t={t})"
        );
    }
    assert_eq!(
        monitor.get("SOL-USD").unwrap().status,
        HealthStatus::Unknown
    );
}

/// Circuit breaker opens at the configured consecutive-stale threshold.
#[test]
fn health_circuit_breaker_opens_at_threshold_and_closes_on_heartbeat() {
    let monitor = HealthMonitor::new(1_000).with_circuit_breaker_threshold(3);
    monitor.register("FEED-A", None);
    monitor.heartbeat("FEED-A", 1_000_000).unwrap();

    monitor.check_all(1_002_000); // stale 1
    assert!(!monitor.is_circuit_open("FEED-A"));
    monitor.check_all(1_003_000); // stale 2
    assert!(!monitor.is_circuit_open("FEED-A"));
    monitor.check_all(1_004_000); // stale 3 — circuit opens
    assert!(monitor.is_circuit_open("FEED-A"));

    // Heartbeat closes the circuit.
    monitor.heartbeat("FEED-A", 1_005_000).unwrap();
    assert!(!monitor.is_circuit_open("FEED-A"));
}

/// Multiple feeds with different thresholds — only the appropriate one goes stale.
#[test]
fn health_per_feed_threshold_independent() {
    let monitor = HealthMonitor::new(10_000);
    monitor.register("FAST", Some(500)); // 0.5 s threshold
    monitor.register("SLOW", Some(10_000)); // 10 s threshold

    let base = 2_000_000u64;
    monitor.heartbeat("FAST", base).unwrap();
    monitor.heartbeat("SLOW", base).unwrap();

    // After 1 s: FAST is stale (> 0.5 s), SLOW is healthy (< 10 s).
    let errors = monitor.check_all(base + 1_000);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.to_string().contains("FAST"));
    assert_eq!(monitor.stale_count(), 1);
    assert_eq!(monitor.healthy_count(), 1);
}

/// `healthy_count` and `stale_count` are consistent after mixed transitions.
#[test]
fn health_counts_consistent_after_transitions() {
    let monitor = HealthMonitor::new(3_000);
    monitor.register("A", None);
    monitor.register("B", None);
    monitor.register("C", None);

    monitor.heartbeat("A", 1_000_000).unwrap();
    monitor.heartbeat("B", 1_000_000).unwrap();
    monitor.heartbeat("C", 1_000_000).unwrap();

    assert_eq!(monitor.healthy_count(), 3);
    assert_eq!(monitor.stale_count(), 0);

    // Advance 4 s — all three go stale.
    monitor.check_all(1_004_000);
    assert_eq!(monitor.healthy_count(), 0);
    assert_eq!(monitor.stale_count(), 3);

    // Recover A.
    monitor.heartbeat("A", 1_005_000).unwrap();
    assert_eq!(monitor.healthy_count(), 1);
    assert_eq!(monitor.stale_count(), 2);
}

// ── Tick normalization edge cases ─────────────────────────────────────────────

/// Binance: payload that is entirely empty must return ParseError.
#[test]
fn tick_normalize_binance_empty_payload_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({}),
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "empty Binance payload must return ParseError"
    );
}

/// Coinbase: `price` field is `null` — must return ParseError (not panic).
#[test]
fn tick_normalize_coinbase_null_price_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Coinbase,
        symbol: "BTC-USD".into(),
        payload: json!({ "price": null, "size": "1.0" }),
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "null price field must return ParseError"
    );
}

/// Binance: price is a boolean (not a string or number) — must return ParseError.
#[test]
fn tick_normalize_binance_boolean_price_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": true, "q": "1.0" }),
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "boolean price field must return ParseError"
    );
}

/// Alpaca: price is an array (garbage) — must return ParseError.
#[test]
fn tick_normalize_alpaca_array_price_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Alpaca,
        symbol: "AAPL".into(),
        payload: json!({ "p": [1, 2, 3], "s": "10" }),
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "array price field must return ParseError"
    );
}

/// Polygon: quantity field missing entirely — must return ParseError.
#[test]
fn tick_normalize_polygon_missing_qty_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Polygon,
        symbol: "AAPL".into(),
        payload: json!({ "p": "180.50" }), // "s" (size) is absent
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "missing Polygon size field must return ParseError"
    );
}

/// Binance: price string is non-numeric — must return ParseError.
#[test]
fn tick_normalize_binance_non_numeric_price_string_returns_parse_error() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": "NaN", "q": "1.0" }),
        received_at_ms: 0,
    };
    let result = TickNormalizer::new().normalize(raw);
    assert!(
        matches!(result, Err(StreamError::ParseError { .. })),
        "non-numeric price string must return ParseError"
    );
}

/// Coinbase: `size` is a negative numeric string — Decimal::from_str succeeds
/// but the value is negative. This is a valid parse; downstream validation is
/// the caller's responsibility. Confirm it does not panic.
#[test]
fn tick_normalize_coinbase_negative_size_parses_without_panic() {
    let raw = RawTick {
        exchange: Exchange::Coinbase,
        symbol: "BTC-USD".into(),
        payload: json!({ "price": "50000.00", "size": "-1.0" }),
        received_at_ms: 0,
    };
    // Should not panic regardless of whether it's Ok or Err.
    let _result = TickNormalizer::new().normalize(raw);
}

/// Binance: all optional fields (m, t, T) absent — must succeed and set
/// corresponding fields to None.
#[test]
fn tick_normalize_binance_all_optional_fields_absent() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "ETHUSDT".into(),
        payload: json!({ "p": "3000.00", "q": "0.5" }),
        received_at_ms: 0,
    };
    let tick = TickNormalizer::new().normalize(raw).unwrap();
    assert!(tick.side.is_none());
    assert!(tick.trade_id.is_none());
    assert!(tick.exchange_ts_ms.is_none());
}

/// Binance: price supplied as a JSON integer (not string, not float) — must succeed.
#[test]
fn tick_normalize_binance_integer_price_succeeds() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": 50000u64, "q": 1u64 }),
        received_at_ms: 0,
    };
    let tick = TickNormalizer::new().normalize(raw).unwrap();
    assert!(tick.price > rust_decimal::Decimal::ZERO);
}

// ── Order book delta application edge cases ───────────────────────────────────

/// Applying a delta with an out-of-order sequence number (lower than the last
/// applied) must return `SequenceGap`. Callers should reset the book and
/// request a fresh snapshot when this error is received.
#[test]
fn book_out_of_order_sequence_numbers_return_gap_error() {
    let mut book = OrderBook::new("BTC-USD");

    // Apply seq=3 first.
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(49900), dec!(2)).with_sequence(3))
        .unwrap();
    assert_eq!(book.last_sequence(), Some(3));

    // Apply seq=1 (out of order, expected=4, got=1) → SequenceGap.
    let err = book
        .apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(49800), dec!(1)).with_sequence(1))
        .unwrap_err();
    assert!(
        matches!(err, fin_stream::StreamError::SequenceGap { expected: 4, got: 1, .. }),
        "expected SequenceGap {{ expected: 4, got: 1 }}, got: {err:?}"
    );
    // last_sequence unchanged after rejected delta.
    assert_eq!(book.last_sequence(), Some(3));
}

/// Applying a duplicate sequence number (same seq after it was already applied)
/// is treated as a gap: the book expects the next consecutive sequence number,
/// so receiving the same number again returns `SequenceGap`.
#[test]
fn book_duplicate_sequence_numbers_return_gap_error() {
    let mut book = OrderBook::new("ETH-USD");

    // First delta: add 5 units at 3000 with seq=10.
    book.apply(BookDelta::new("ETH-USD", BookSide::Ask, dec!(3000), dec!(5)).with_sequence(10))
        .unwrap();
    assert_eq!(book.best_ask().unwrap().quantity, dec!(5));

    // Duplicate seq=10: expected=11, got=10 → SequenceGap.
    let err = book
        .apply(BookDelta::new("ETH-USD", BookSide::Ask, dec!(3000), dec!(8)).with_sequence(10))
        .unwrap_err();
    assert!(
        matches!(err, fin_stream::StreamError::SequenceGap { expected: 11, got: 10, .. }),
        "expected SequenceGap {{ expected: 11, got: 10 }}, got: {err:?}"
    );
    // Quantity unchanged after rejected delta.
    assert_eq!(book.best_ask().unwrap().quantity, dec!(5));
}

/// A sequence gap (e.g., seq 5 → seq 10) must return `SequenceGap`; the book
/// is not modified by the rejected delta. The caller must reset the book to
/// recover from a gap before applying further deltas.
#[test]
fn book_sequence_gap_returns_error_and_leaves_book_unchanged() {
    let mut book = OrderBook::new("BTC-USD");

    book.apply(BookDelta::new("BTC-USD", BookSide::Ask, dec!(50100), dec!(3)).with_sequence(5))
        .unwrap();

    // Gap: sequence jumps from 5 to 10 (expected 6, got 10).
    let err = book
        .apply(BookDelta::new("BTC-USD", BookSide::Ask, dec!(50100), dec!(0)).with_sequence(10))
        .unwrap_err();
    assert!(
        matches!(err, fin_stream::StreamError::SequenceGap { expected: 6, got: 10, .. }),
        "expected SequenceGap {{ expected: 6, got: 10 }}, got: {err:?}"
    );
    // Level is NOT removed because the delta was rejected.
    assert_eq!(book.ask_depth(), 1);
    assert_eq!(book.best_ask().unwrap().quantity, dec!(3));
}

/// Applying multiple deltas without any sequence number set must not affect
/// last_sequence (it should remain None).
#[test]
fn book_deltas_without_sequence_leave_last_sequence_none() {
    let mut book = OrderBook::new("BTC-USD");
    book.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Bid,
        dec!(49000),
        dec!(1),
    ))
    .unwrap();
    book.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Bid,
        dec!(48900),
        dec!(2),
    ))
    .unwrap();
    assert!(book.last_sequence().is_none());
}

/// A reset replaces bids and asks with a fresh snapshot; subsequent deltas
/// apply on top of the snapshot regardless of sequence number.
#[test]
fn book_reset_replaces_levels_and_accepts_subsequent_deltas() {
    let mut book = OrderBook::new("BTC-USD");

    // Populate and advance sequence.
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(49000), dec!(5)).with_sequence(100))
        .unwrap();

    // Reset (full snapshot) — this replaces all levels.
    book.reset(
        vec![PriceLevel::new(dec!(50000), dec!(2))],
        vec![PriceLevel::new(dec!(50100), dec!(1))],
    )
    .unwrap();

    // The old level at 49000 must be gone; snapshot levels are in place.
    assert_eq!(book.bid_depth(), 1);
    assert_eq!(book.best_bid().unwrap().price, dec!(50000));

    // Apply a delta with a low sequence number — must succeed regardless.
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(10)).with_sequence(1))
        .unwrap();
    assert_eq!(book.best_bid().unwrap().quantity, dec!(10));
    assert_eq!(book.last_sequence(), Some(1));
}

/// Interleaved bid and ask deltas with consecutive sequences are applied
/// correctly and produce a consistent book state.
#[test]
fn book_interleaved_bid_ask_deltas_consecutive_produce_correct_book() {
    let mut book = OrderBook::new("BTC-USD");

    // Seq 1: add bid at 49000.
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(49000), dec!(3)).with_sequence(1))
        .unwrap();

    // Seq 2: add ask at 51000.
    book.apply(BookDelta::new("BTC-USD", BookSide::Ask, dec!(51000), dec!(2)).with_sequence(2))
        .unwrap();

    // Seq 3: add another bid at 48900.
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(48900), dec!(1)).with_sequence(3))
        .unwrap();

    assert_eq!(book.best_bid().unwrap().price, dec!(49000));
    assert_eq!(book.best_ask().unwrap().price, dec!(51000));
    assert_eq!(book.bid_depth(), 2);
    assert_eq!(book.ask_depth(), 1);
    assert_eq!(book.spread().unwrap(), dec!(2000));

    // A non-consecutive sequence now returns SequenceGap.
    let err = book
        .apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(48800), dec!(1)).with_sequence(10))
        .unwrap_err();
    assert!(matches!(
        err,
        fin_stream::StreamError::SequenceGap { expected: 4, got: 10, .. }
    ));
}
