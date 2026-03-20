//! Integration tests: cross-module pipelines and end-to-end scenarios.

use fin_stream::book::{BookDelta, BookSide, OrderBook};
use fin_stream::health::HealthMonitor;
use fin_stream::lorentz::{LorentzTransform, SpacetimePoint};
use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::session::{MarketSession, SessionAwareness, TradingStatus};
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use fin_stream::StreamError;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

// ── Tick normalizer end-to-end ───────────────────────────────────────────────

#[test]
fn test_full_binance_pipeline() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": "65000.50", "q": "0.002", "m": false, "t": 9999u64 }),
        received_at_ms: 1700000000000,
    };
    let tick = TickNormalizer::new().normalize(raw).unwrap();
    assert_eq!(tick.exchange, Exchange::Binance);
    assert_eq!(tick.price, Decimal::from_str("65000.50").unwrap());
    assert_eq!(tick.side, Some(TradeSide::Buy));
}

#[test]
fn test_all_exchanges_normalize_successfully() {
    let normalizer = TickNormalizer::new();
    let cases = vec![
        (
            Exchange::Binance,
            json!({ "p": "100", "q": "1", "m": true }),
        ),
        (
            Exchange::Coinbase,
            json!({ "price": "100", "size": "1", "side": "buy" }),
        ),
        (Exchange::Alpaca, json!({ "p": "100", "s": "1" })),
        (Exchange::Polygon, json!({ "p": "100", "s": "1" })),
    ];
    for (exchange, payload) in cases {
        let raw = RawTick {
            exchange,
            symbol: "SYM".into(),
            payload,
            received_at_ms: 0,
        };
        let result = normalizer.normalize(raw);
        assert!(
            result.is_ok(),
            "exchange {:?} failed: {:?}",
            exchange,
            result
        );
    }
}

// ── Order book pipeline ──────────────────────────────────────────────────────

#[test]
fn test_order_book_full_lifecycle() {
    let mut book = OrderBook::new("BTC-USD");
    book.reset(
        vec![
            fin_stream::book::PriceLevel::new(dec!(50000), dec!(5)),
            fin_stream::book::PriceLevel::new(dec!(49900), dec!(3)),
        ],
        vec![
            fin_stream::book::PriceLevel::new(dec!(50100), dec!(2)),
            fin_stream::book::PriceLevel::new(dec!(50200), dec!(1)),
        ],
    )
    .unwrap();
    assert_eq!(book.bid_depth(), 2);
    assert_eq!(book.ask_depth(), 2);
    book.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Bid,
        dec!(50000),
        dec!(10),
    ))
    .unwrap();
    assert_eq!(book.best_bid().unwrap().quantity, dec!(10));
    book.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Ask,
        dec!(50200),
        dec!(0),
    ))
    .unwrap();
    assert_eq!(book.ask_depth(), 1);
    assert_eq!(book.spread().unwrap(), dec!(100));
}

#[test]
fn test_order_book_top_levels() {
    let mut book = OrderBook::new("BTC-USD");
    for i in 0u32..5 {
        let price = dec!(50000) - Decimal::from(i * 100);
        book.apply(BookDelta::new("BTC-USD", BookSide::Bid, price, dec!(1)))
            .unwrap();
        let ask_price = dec!(50100) + Decimal::from(i * 100);
        book.apply(BookDelta::new("BTC-USD", BookSide::Ask, ask_price, dec!(1)))
            .unwrap();
    }
    let top3_bids = book.top_bids(3);
    assert_eq!(top3_bids.len(), 3);
    assert!(top3_bids[0].price > top3_bids[1].price);
}

// ── OHLCV aggregation ────────────────────────────────────────────────────────

fn make_tick(symbol: &str, price: Decimal, qty: Decimal, ts_ms: u64) -> NormalizedTick {
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

#[test]
fn test_ohlcv_multi_bar_sequence() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50500), dec!(2), 60_500))
        .unwrap();
    let mut bars = agg
        .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 120_000))
        .unwrap();
    assert_eq!(bars.len(), 1);
    let completed = bars.remove(0);
    assert!(completed.is_complete);
    assert_eq!(completed.open, dec!(50000));
    assert_eq!(completed.close, dec!(50500));
    assert_eq!(completed.high, dec!(50500));
    assert_eq!(completed.volume, dec!(3));
    let current = agg.current_bar().unwrap();
    assert_eq!(current.open, dec!(51000));
}

#[test]
fn test_ohlcv_flush_returns_partial_bar() {
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Hours(1)).unwrap();
    agg.feed(&make_tick("ETH-USD", dec!(3000), dec!(5), 3_600_000))
        .unwrap();
    agg.feed(&make_tick("ETH-USD", dec!(3100), dec!(3), 3_601_000))
        .unwrap();
    let flushed = agg.flush().unwrap();
    assert!(flushed.is_complete);
    assert_eq!(flushed.trade_count, 2);
    assert_eq!(flushed.volume, dec!(8));
}

// ── Health monitor ───────────────────────────────────────────────────────────

#[test]
fn test_health_monitor_multi_feed_scenario() {
    let monitor = HealthMonitor::new(5_000);
    monitor.register("BTC-USD", None);
    monitor.register("ETH-USD", Some(2_000));

    monitor.heartbeat("BTC-USD", 1_000_000).unwrap();
    monitor.heartbeat("ETH-USD", 1_000_000).unwrap();

    let errors = monitor.check_all(1_003_000);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.to_string().contains("ETH-USD"));

    assert_eq!(monitor.healthy_count(), 1);
    assert_eq!(monitor.stale_count(), 1);
}

// ── Session awareness ────────────────────────────────────────────────────────

const SAT_UTC_MS: u64 = 1705147200000; // 2024-01-13 12:00 UTC = 07:00 EST (Saturday in ET)
const MON_OPEN_UTC_MS: u64 = 1704724200000; // Mon Jan 08 2024 14:30 UTC = 09:30 ET

#[test]
fn test_session_crypto_always_open_any_time() {
    let sa = SessionAwareness::new(MarketSession::Crypto);
    for ts in [0u64, 1_000_000, 1_700_000_000_000, SAT_UTC_MS] {
        assert_eq!(sa.status(ts).unwrap(), TradingStatus::Open);
    }
}

#[test]
fn test_session_us_equity_weekend_closed() {
    let sa = SessionAwareness::new(MarketSession::UsEquity);
    assert_eq!(sa.status(SAT_UTC_MS).unwrap(), TradingStatus::Closed);
}

#[test]
fn test_session_us_equity_open_during_market_hours() {
    let sa = SessionAwareness::new(MarketSession::UsEquity);
    assert_eq!(sa.status(MON_OPEN_UTC_MS).unwrap(), TradingStatus::Open);
}

// ── WsManager / reconnect ────────────────────────────────────────────────────

#[test]
fn test_ws_manager_reconnect_policy_integration() {
    let policy =
        ReconnectPolicy::new(4, Duration::from_millis(50), Duration::from_secs(5), 2.0).unwrap();
    let config = ConnectionConfig::new("wss://feed.example.com/ws", 512)
        .unwrap()
        .with_reconnect(policy);
    let mut mgr = WsManager::new(config);

    mgr.connect_simulated();
    assert!(mgr.is_connected());

    mgr.disconnect_simulated();
    assert!(!mgr.is_connected());

    let b0 = mgr.next_reconnect_backoff().unwrap();
    let b1 = mgr.next_reconnect_backoff().unwrap();
    let b2 = mgr.next_reconnect_backoff().unwrap();
    assert!(b1 >= b0);
    assert!(b2 >= b1);

    let result = mgr.next_reconnect_backoff();
    assert!(matches!(
        result,
        Err(StreamError::ReconnectExhausted { .. })
    ));
}

// ── Cross-module: tick -> OHLCV pipeline ────────────────────────────────────

#[test]
fn test_tick_to_ohlcv_end_to_end() {
    let normalizer = TickNormalizer::new();
    let mut agg = OhlcvAggregator::new("BTCUSDT", Timeframe::Seconds(30)).unwrap();

    let base_ts = 30_000u64;
    let payloads = vec![
        (dec!(50000), dec!(1)),
        (dec!(50100), dec!(2)),
        (dec!(49900), dec!(0.5)),
    ];

    for (i, (price, qty)) in payloads.iter().enumerate() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": price.to_string(), "q": qty.to_string(), "m": false }),
            received_at_ms: base_ts + i as u64 * 100,
        };
        let tick = normalizer.normalize(raw).unwrap();
        agg.feed(&tick).unwrap();
    }

    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.open, dec!(50000));
    assert_eq!(bar.high, dec!(50100));
    assert_eq!(bar.low, dec!(49900));
    assert_eq!(bar.close, dec!(49900));
    assert_eq!(bar.trade_count, 3);
}

// ── SPSC ring buffer pipeline tests ─────────────────────────────────────────

/// Feed ticks through an SPSC ring buffer and verify no items are lost.
#[test]
fn test_ring_buffer_tick_pipeline() {
    let ring: SpscRing<NormalizedTick, 64> = SpscRing::new();
    let normalizer = TickNormalizer::new();

    for i in 0..10u64 {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "50000", "q": "1", "m": false }),
            received_at_ms: i * 100,
        };
        let tick = normalizer.normalize(raw).unwrap();
        ring.push(tick).unwrap();
    }

    let mut count = 0;
    while let Ok(_tick) = ring.pop() {
        count += 1;
    }
    assert_eq!(count, 10);
}

/// Ring buffer full condition: pushing beyond capacity returns RingBufferFull.
#[test]
fn test_ring_buffer_full_error_propagation() {
    let ring: SpscRing<u32, 4> = SpscRing::new(); // capacity = 3
    ring.push(1).unwrap();
    ring.push(2).unwrap();
    ring.push(3).unwrap();
    let err = ring.push(4).unwrap_err();
    assert!(matches!(err, StreamError::RingBufferFull { .. }));
}

/// Ring buffer empty condition: popping from empty returns RingBufferEmpty.
#[test]
fn test_ring_buffer_empty_error_propagation() {
    let ring: SpscRing<u32, 8> = SpscRing::new();
    let err = ring.pop().unwrap_err();
    assert!(matches!(err, StreamError::RingBufferEmpty));
}

// ── End-to-end: tick -> OHLCV -> normalized pipeline ─────────────────────────

/// Full pipeline: ingest ticks via the ring buffer, aggregate to OHLCV, then
/// normalize the close price with a rolling window.
#[test]
fn test_tick_to_ohlcv_to_normalized_pipeline() {
    let ring: SpscRing<NormalizedTick, 32> = SpscRing::new();
    let (prod, cons) = ring.split();

    // Produce 5 ticks in the same 1-minute window.
    let prices: &[f64] = &[100.0, 102.0, 98.0, 103.0, 101.0];
    for (i, &p) in prices.iter().enumerate() {
        let tick = NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTC-USD".into(),
            price: Decimal::try_from(p).unwrap(),
            quantity: dec!(1),
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 60_000 + i as u64 * 100,
        };
        prod.push(tick).unwrap();
    }

    // Consume and aggregate.
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    while let Ok(tick) = cons.pop() {
        agg.feed(&tick).unwrap();
    }

    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.trade_count, 5);

    // Normalize the close price.
    let mut norm = MinMaxNormalizer::new(10).unwrap();
    for &p in prices {
        norm.update(Decimal::try_from(p).unwrap());
    }
    let normalized = norm.normalize(bar.close).unwrap();
    assert!((0.0..=1.0).contains(&normalized));
}

// ── Lorentz transform pipeline integration ───────────────────────────────────

/// Verify that Lorentz-transformed OHLCV timestamps round-trip correctly.
#[test]
fn test_lorentz_transform_on_ohlcv_timestamps() {
    let lt = LorentzTransform::new(0.5).unwrap();
    let bar_starts: &[f64] = &[60_000.0, 120_000.0, 180_000.0];

    for &t in bar_starts {
        let p = SpacetimePoint::new(t, 50_000.0);
        let transformed = lt.transform(p);
        let recovered = lt.inverse_transform(transformed);
        assert!(
            (recovered.t - t).abs() < 1e-6,
            "round-trip failed for t={t}"
        );
    }
}

/// Pipeline: normalize prices, then apply Lorentz transform to (time, price).
#[test]
fn test_normalize_then_lorentz_pipeline() {
    let mut norm = MinMaxNormalizer::new(5).unwrap();
    let prices: &[f64] = &[100.0, 105.0, 95.0, 110.0, 90.0];
    for &p in prices {
        norm.update(Decimal::try_from(p).unwrap());
    }

    let lt = LorentzTransform::new(0.3).unwrap();

    for (i, &p) in prices.iter().enumerate() {
        let x = norm.normalize(Decimal::try_from(p).unwrap()).unwrap(); // in [0, 1]
        let t = i as f64;
        let pt = SpacetimePoint::new(t, x);
        let transformed = lt.transform(pt);
        // Transformed point should be finite and well-defined.
        assert!(transformed.t.is_finite());
        assert!(transformed.x.is_finite());
    }
}

// ── Concurrent SPSC ring buffer ───────────────────────────────────────────────

/// Concurrent producer/consumer with 50 000 ticks -- integration smoke test.
#[test]
fn test_concurrent_tick_ring_buffer_integration() {
    const N: usize = 50_000;
    let ring: SpscRing<u64, 512> = SpscRing::new();
    let (prod, cons) = ring.split();

    let producer = thread::spawn(move || {
        let mut sent = 0;
        while sent < N {
            if prod.push(sent as u64).is_ok() {
                sent += 1;
            }
        }
    });

    let consumer = thread::spawn(move || {
        let mut received = 0usize;
        while received < N {
            if cons.pop().is_ok() {
                received += 1;
            }
        }
        received
    });

    producer.join().unwrap();
    let count = consumer.join().unwrap();
    assert_eq!(count, N);
}
