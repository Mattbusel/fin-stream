//! Tests for tick normalization: [0,1] range, rolling window resets, all exchanges.

use fin_stream::norm::MinMaxNormalizer;
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::str::FromStr;

// ── Normalized price values are in [0, 1] ────────────────────────────────────

/// After feeding a known sequence, every value normalized through the window
/// must land in [0.0, 1.0].
#[test]
fn test_normalized_prices_in_unit_interval() {
    let mut norm = MinMaxNormalizer::new(20).unwrap();

    // Simulate 20 sequential close prices
    let prices: Vec<Decimal> = (0..20)
        .map(|i| dec!(50000) + Decimal::from(i) * dec!(100))
        .collect();
    for &p in &prices {
        norm.update(p);
    }

    for &p in &prices {
        let v = norm.normalize(p).unwrap();
        assert!(
            (0.0..=1.0).contains(&v),
            "normalize({p}) = {v} is out of [0, 1]"
        );
    }
}

/// Values outside the window range are clamped to [0, 1].
#[test]
fn test_normalized_value_clamped_outside_window() {
    let mut norm = MinMaxNormalizer::new(5).unwrap();
    for i in 0..5 {
        norm.update(Decimal::from(i) * dec!(10)); // window: [0, 10, 20, 30, 40]
    }

    // Below min
    assert_eq!(norm.normalize(dec!(-100)).unwrap(), 0.0);
    // Above max
    assert_eq!(norm.normalize(dec!(1000)).unwrap(), 1.0);
    // At boundaries
    assert_eq!(norm.normalize(dec!(0)).unwrap(), 0.0);
    assert_eq!(norm.normalize(dec!(40)).unwrap(), 1.0);
}

// ── Rolling window resets ─────────────────────────────────────────────────────

/// After `reset()`, the window is empty and `normalize()` returns an error.
#[test]
fn test_rolling_window_reset_makes_window_empty() {
    let mut norm = MinMaxNormalizer::new(5).unwrap();
    for i in 0..5 {
        norm.update(Decimal::from(i));
    }
    assert!(!norm.is_empty());
    norm.reset();
    assert!(norm.is_empty());
    assert!(norm.normalize(dec!(1)).is_err());
}

/// After a reset, feeding new values produces a fresh normalization range.
#[test]
fn test_rolling_window_reset_then_renormalize_fresh_range() {
    let mut norm = MinMaxNormalizer::new(3).unwrap();
    // First range: 0..10
    norm.update(dec!(0));
    norm.update(dec!(5));
    norm.update(dec!(10));
    assert_eq!(norm.normalize(dec!(10)).unwrap(), 1.0);

    norm.reset();

    // New range: 100..200
    norm.update(dec!(100));
    norm.update(dec!(150));
    norm.update(dec!(200));
    // Now 100 should normalize to 0.0 and 200 to 1.0
    assert_eq!(norm.normalize(dec!(100)).unwrap(), 0.0);
    assert_eq!(norm.normalize(dec!(200)).unwrap(), 1.0);
    // 150 should be 0.5
    let mid = norm.normalize(dec!(150)).unwrap();
    assert!((mid - 0.5).abs() < 1e-10, "mid should be 0.5, got {mid}");
}

/// The rolling window evicts old values and the normalization range shifts.
#[test]
fn test_rolling_window_evicts_and_range_shifts() {
    let mut norm = MinMaxNormalizer::new(3).unwrap();
    norm.update(dec!(0)); // will be evicted on 4th update
    norm.update(dec!(50));
    norm.update(dec!(100));

    let (min1, max1) = norm.min_max().unwrap();
    assert_eq!(min1, dec!(0));
    assert_eq!(max1, dec!(100));

    // 4th update evicts 0; new window: [50, 100, 200]
    norm.update(dec!(200));
    let (min2, max2) = norm.min_max().unwrap();
    assert_eq!(min2, dec!(50));
    assert_eq!(max2, dec!(200));
}

// ── All exchange normalizations produce values within expected range ──────────

fn make_raw_tick(exchange: Exchange, payload: serde_json::Value) -> RawTick {
    RawTick {
        exchange,
        symbol: "SYM".into(),
        payload,
        received_at_ms: 1_700_000_000_000,
    }
}

/// All four exchanges produce a NormalizedTick whose price parses to a positive Decimal.
#[test]
fn test_all_exchange_normalized_tick_has_positive_price() {
    let normalizer = TickNormalizer::new();
    let cases = vec![
        make_raw_tick(
            Exchange::Binance,
            json!({ "p": "65000.50", "q": "0.01", "m": false }),
        ),
        make_raw_tick(
            Exchange::Coinbase,
            json!({ "price": "65000.50", "size": "0.01", "side": "buy" }),
        ),
        make_raw_tick(Exchange::Alpaca, json!({ "p": "180.50", "s": "10" })),
        make_raw_tick(Exchange::Polygon, json!({ "p": "180.50", "s": "5" })),
    ];
    for raw in cases {
        let exchange = raw.exchange;
        let tick = normalizer.normalize(raw).unwrap();
        assert!(
            tick.price > Decimal::ZERO,
            "Exchange {exchange}: price should be > 0, got {}",
            tick.price
        );
        assert!(
            tick.quantity > Decimal::ZERO,
            "Exchange {exchange}: quantity should be > 0, got {}",
            tick.quantity
        );
    }
}

/// Normalizing raw Binance ticks through MinMaxNormalizer produces [0,1] values.
#[test]
fn test_normalized_binance_ticks_in_unit_interval_via_min_max() {
    let normalizer = TickNormalizer::new();
    let mut norm = MinMaxNormalizer::new(10).unwrap();

    let prices = [
        "50000", "50100", "49900", "50200", "49800", "50300", "49700", "50400", "49600", "50500",
    ];

    let ticks: Vec<NormalizedTick> = prices
        .iter()
        .map(|&p| {
            let raw = make_raw_tick(Exchange::Binance, json!({ "p": p, "q": "1", "m": false }));
            normalizer.normalize(raw).unwrap()
        })
        .collect();

    // Seed the normalizer with all prices
    for tick in &ticks {
        norm.update(tick.price);
    }

    // Every tick's price should normalize to [0, 1]
    for tick in &ticks {
        let v = norm.normalize(tick.price).unwrap();
        assert!(
            (0.0..=1.0).contains(&v),
            "normalized price {} = {} is out of [0, 1]",
            tick.price,
            v
        );
    }
}

// ── NormalizedTick structural checks ─────────────────────────────────────────

/// Binance 'm' field: maker=false → Buy, maker=true → Sell.
#[test]
fn test_tick_normalization_binance_side_mapping() {
    let normalizer = TickNormalizer::new();

    let buy_raw = make_raw_tick(
        Exchange::Binance,
        json!({ "p": "100", "q": "1", "m": false }),
    );
    let sell_raw = make_raw_tick(
        Exchange::Binance,
        json!({ "p": "100", "q": "1", "m": true }),
    );

    assert_eq!(
        normalizer.normalize(buy_raw).unwrap().side,
        Some(TradeSide::Buy)
    );
    assert_eq!(
        normalizer.normalize(sell_raw).unwrap().side,
        Some(TradeSide::Sell)
    );
}

/// Coinbase 'side' field maps correctly.
#[test]
fn test_tick_normalization_coinbase_side_mapping() {
    let normalizer = TickNormalizer::new();

    let buy_raw = make_raw_tick(
        Exchange::Coinbase,
        json!({ "price": "100", "size": "1", "side": "buy"  }),
    );
    let sell_raw = make_raw_tick(
        Exchange::Coinbase,
        json!({ "price": "100", "size": "1", "side": "sell" }),
    );

    assert_eq!(
        normalizer.normalize(buy_raw).unwrap().side,
        Some(TradeSide::Buy)
    );
    assert_eq!(
        normalizer.normalize(sell_raw).unwrap().side,
        Some(TradeSide::Sell)
    );
}

/// Alpaca and Polygon ticks have no side field (None).
#[test]
fn test_tick_normalization_alpaca_polygon_side_is_none() {
    let normalizer = TickNormalizer::new();

    let alpaca = make_raw_tick(Exchange::Alpaca, json!({ "p": "100", "s": "1" }));
    let polygon = make_raw_tick(Exchange::Polygon, json!({ "p": "100", "s": "1" }));

    assert_eq!(normalizer.normalize(alpaca).unwrap().side, None);
    assert_eq!(normalizer.normalize(polygon).unwrap().side, None);
}

/// Exchange::from_str is case-insensitive and covers all variants.
#[test]
fn test_exchange_from_str_case_insensitive() {
    let cases = [
        ("BINANCE", Exchange::Binance),
        ("binance", Exchange::Binance),
        ("Binance", Exchange::Binance),
        ("COINBASE", Exchange::Coinbase),
        ("alpaca", Exchange::Alpaca),
        ("POLYGON", Exchange::Polygon),
    ];
    for (s, expected) in cases {
        let got = Exchange::from_str(s).unwrap();
        assert_eq!(got, expected, "from_str({s}) failed");
    }
}

/// Numeric price fields (f64 in JSON) are accepted alongside string fields.
#[test]
fn test_tick_normalization_accepts_numeric_price_fields() {
    let normalizer = TickNormalizer::new();
    let raw = make_raw_tick(Exchange::Binance, json!({ "p": 65000.0, "q": 0.5 }));
    let tick = normalizer.normalize(raw).unwrap();
    assert!(tick.price > Decimal::ZERO);
    assert!(tick.quantity > Decimal::ZERO);
}
