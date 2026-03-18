//! Integration tests for the fin-stream multi-stage pipeline.
//!
//! A "pipeline" in fin-stream is the composition:
//!
//! ```text
//! SpscRing (tick source)
//!     → OhlcvAggregator (bar construction)
//!     → MinMaxNormalizer (coordinate normalization)
//!     → LorentzTransform (spacetime feature engineering)
//! ```
//!
//! These tests verify end-to-end data flow, correct stage ordering, and
//! the no-panic contract for empty or minimal inputs.

use fin_stream::error::StreamError;
use fin_stream::lorentz::{LorentzTransform, SpacetimePoint};
use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::tick::{Exchange, NormalizedTick};
use rust_decimal_macros::dec;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_tick(
    symbol: &str,
    price: rust_decimal::Decimal,
    qty: rust_decimal::Decimal,
    ts_ms: u64,
) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Binance,
        symbol: symbol.to_string(),
        price,
        quantity: qty,
        side: None,
        trade_id: None,
        exchange_ts_ms: None,
        received_at_ms: ts_ms,
    }
}

// ── test_pipeline_processes_all_stages ───────────────────────────────────────

/// Feed ticks through the full pipeline: ring → OHLCV aggregator →
/// normalizer → Lorentz transform. Verify that each stage produces the
/// expected output type with no errors.
#[test]
fn test_pipeline_processes_all_stages() {
    // Stage 1: SPSC ring buffer (source of ticks).
    let ring: SpscRing<NormalizedTick, 32> = SpscRing::new();

    // Push three ticks into the same 1-minute bar, then a tick in the next
    // minute to trigger bar completion.
    ring.push(make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
        .unwrap();
    ring.push(make_tick("BTC-USD", dec!(50100), dec!(2), 60_500))
        .unwrap();
    ring.push(make_tick("BTC-USD", dec!(49900), dec!(1), 60_999))
        .unwrap();
    ring.push(make_tick("BTC-USD", dec!(50200), dec!(1), 120_000))
        .unwrap(); // new bar

    // Stage 2: OHLCV aggregator.
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    let mut completed_bars = Vec::new();

    while let Ok(tick) = ring.pop() {
        let bars = agg.feed(&tick).unwrap();
        completed_bars.extend(bars);
    }

    // The first three ticks were in minute 1; the fourth (ts 120_000) triggered
    // completion of the minute-1 bar.
    assert_eq!(
        completed_bars.len(),
        1,
        "exactly one bar should be completed"
    );
    let bar = &completed_bars[0];
    assert!(bar.is_complete);
    assert_eq!(bar.open, dec!(50000));
    assert_eq!(bar.high, dec!(50100));
    assert_eq!(bar.low, dec!(49900));
    assert_eq!(bar.close, dec!(49900));
    assert_eq!(bar.trade_count, 3);
    assert_eq!(bar.bar_start_ms, 60_000);

    // Stage 3: MinMax normalization of the close price.
    let mut normalizer = MinMaxNormalizer::new(4);
    // Seed the window with the known price range.
    normalizer.update(49000.0);
    normalizer.update(51000.0);

    let close_f64 = bar.close.to_string().parse::<f64>().unwrap();
    let normalized = normalizer.normalize(close_f64).unwrap();
    assert!(
        normalized >= 0.0 && normalized <= 1.0,
        "normalized close must be in [0, 1], got {normalized}"
    );

    // Stage 4: Lorentz transform on (time, price) coordinates.
    let transform = LorentzTransform::new(0.1).unwrap();
    let t = bar.bar_start_ms as f64 / 1_000.0; // seconds
    let x = close_f64;
    let point = SpacetimePoint { t, x };
    let boosted = transform.transform(point);
    // Boosted coordinates must be finite numbers — no NaN/Inf.
    assert!(boosted.t.is_finite(), "boosted t must be finite");
    assert!(boosted.x.is_finite(), "boosted x must be finite");
}

// ── test_pipeline_empty_input_no_panic ───────────────────────────────────────

/// An empty tick stream must not cause any stage to panic. Each stage should
/// gracefully return `None` or an empty collection.
#[test]
fn test_pipeline_empty_input_no_panic() {
    // Empty ring: pop must return an error (not panic).
    let ring: SpscRing<NormalizedTick, 8> = SpscRing::new();
    let err = ring.pop().unwrap_err();
    assert!(matches!(err, StreamError::RingBufferEmpty));

    // Aggregator with no ticks: flush must return None (not panic).
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Minutes(1)).unwrap();
    assert!(
        agg.flush().is_none(),
        "flush on empty aggregator must return None"
    );
    assert!(agg.current_bar().is_none());

    // Normalizer with empty window: normalize must return an error (not panic).
    let mut norm = MinMaxNormalizer::new(4);
    let err = norm.normalize(1.0).unwrap_err();
    assert!(matches!(err, StreamError::NormalizationError { .. }));

    // Lorentz transform with beta = 0 is the identity — must not panic.
    let transform = LorentzTransform::new(0.0).unwrap();
    let p = SpacetimePoint { t: 0.0, x: 0.0 };
    let out = transform.transform(p);
    assert!(out.t.is_finite());
    assert!(out.x.is_finite());
}

// ── Additional pipeline tests ─────────────────────────────────────────────────

/// Verify that the ring preserves tick order through to the aggregator.
#[test]
fn test_pipeline_ring_to_ohlcv_fifo_ordering() {
    let ring: SpscRing<NormalizedTick, 16> = SpscRing::new();
    // Five ascending-price ticks, all in the same 30-second window.
    for i in 0u64..5 {
        ring.push(make_tick(
            "ETH-USD",
            dec!(3000) + rust_decimal::Decimal::from(i * 100),
            dec!(1),
            30_000 + i * 100,
        ))
        .unwrap();
    }
    // Flush tick to complete the bar.
    ring.push(make_tick("ETH-USD", dec!(3500), dec!(1), 60_000))
        .unwrap();

    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Seconds(30)).unwrap();
    let mut bars = Vec::new();
    while let Ok(tick) = ring.pop() {
        bars.extend(agg.feed(&tick).unwrap());
    }

    assert_eq!(bars.len(), 1);
    let bar = &bars[0];
    // Open must be the first tick's price (3000), close the last tick in the bar (3400).
    assert_eq!(bar.open, dec!(3000));
    assert_eq!(
        bar.close,
        dec!(3400),
        "close should be last tick in the bar"
    );
    assert_eq!(bar.high, dec!(3400));
    assert_eq!(bar.low, dec!(3000));
}

/// Normalization of a sequence of close prices must produce values in [0, 1].
#[test]
fn test_pipeline_normalization_bounds() {
    let closes = [50000.0_f64, 50100.0, 49900.0, 50050.0, 50200.0];
    let mut norm = MinMaxNormalizer::new(closes.len());
    for &c in &closes {
        norm.update(c);
    }
    for &c in &closes {
        let v = norm.normalize(c).unwrap();
        assert!(v >= 0.0 && v <= 1.0, "normalized value {v} out of [0, 1]");
    }
}

/// The Lorentz transform with beta = 0 is the identity: (t', x') == (t, x).
#[test]
fn test_pipeline_lorentz_identity_at_zero_beta() {
    let transform = LorentzTransform::new(0.0).unwrap();
    let p = SpacetimePoint { t: 3.14, x: 2.718 };
    let out = transform.transform(p);
    assert!((out.t - p.t).abs() < 1e-9, "identity: t' should equal t");
    assert!((out.x - p.x).abs() < 1e-9, "identity: x' should equal x");
}

/// The Lorentz transform with beta >= 1 must be rejected (would produce NaN/Inf).
#[test]
fn test_pipeline_lorentz_invalid_beta_returns_error() {
    assert!(
        LorentzTransform::new(1.0).is_err(),
        "beta = 1.0 must be rejected"
    );
    assert!(
        LorentzTransform::new(1.5).is_err(),
        "beta > 1.0 must be rejected"
    );
    assert!(
        LorentzTransform::new(-0.1).is_err(),
        "negative beta must be rejected"
    );
}

/// A multi-bar sequence through the pipeline produces bars in the correct order.
#[test]
fn test_pipeline_multi_bar_sequence() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    let mut norm = MinMaxNormalizer::new(10);

    let tick_groups: &[(rust_decimal::Decimal, u64)] = &[
        (dec!(50000), 60_000),  // bar 1
        (dec!(50500), 60_500),  // bar 1
        (dec!(51000), 120_000), // bar 2 — completes bar 1
        (dec!(50800), 120_500), // bar 2
        (dec!(52000), 180_000), // bar 3 — completes bar 2
    ];

    let mut completed = Vec::new();
    for &(price, ts) in tick_groups {
        let tick = make_tick("BTC-USD", price, dec!(1), ts);
        let bars = agg.feed(&tick).unwrap();
        for bar in bars {
            norm.update(bar.close.to_string().parse::<f64>().unwrap());
            completed.push(bar);
        }
    }

    // Two bars should be completed (bar 1 and bar 2).
    assert_eq!(completed.len(), 2, "two bars should be completed");
    assert_eq!(completed[0].bar_start_ms, 60_000);
    assert_eq!(completed[1].bar_start_ms, 120_000);

    // Normalizer has two observations; both should normalize cleanly.
    for bar in &completed {
        let c = bar.close.to_string().parse::<f64>().unwrap();
        let v = norm.normalize(c).unwrap();
        assert!(v >= 0.0 && v <= 1.0);
    }
}
