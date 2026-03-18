//! Extended test coverage -- error display, tick edge cases, book invariants,
//! OHLCV timeframes, health edge cases, session boundary cases, ring buffer,
//! normalization, and Lorentz transforms.

use fin_stream::book::{BookDelta, BookSide, OrderBook, PriceLevel};
use fin_stream::error::StreamError;
use fin_stream::health::HealthMonitor;
use fin_stream::lorentz::{LorentzTransform, SpacetimePoint};
use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::session::{MarketSession, SessionAwareness, TradingStatus};
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::time::Duration;

// ── StreamError display ──────────────────────────────────────────────────────

#[test]
fn test_error_io_display() {
    let e = StreamError::Io("connection reset".into());
    assert!(e.to_string().contains("connection reset"));
}

#[test]
fn test_error_websocket_display() {
    let e = StreamError::WebSocket("protocol error".into());
    assert!(e.to_string().contains("protocol error"));
}

#[test]
fn test_error_ring_buffer_full_display() {
    let e = StreamError::RingBufferFull { capacity: 512 };
    assert!(e.to_string().contains("512"));
}

#[test]
fn test_error_ring_buffer_empty_display() {
    let e = StreamError::RingBufferEmpty;
    assert!(e.to_string().contains("empty"));
}

#[test]
fn test_error_aggregation_error_display() {
    let e = StreamError::AggregationError {
        reason: "bad symbol".into(),
    };
    assert!(e.to_string().contains("bad symbol"));
}

#[test]
fn test_error_normalization_error_display() {
    let e = StreamError::NormalizationError {
        reason: "no data".into(),
    };
    assert!(e.to_string().contains("no data"));
}

#[test]
fn test_error_invalid_tick_display() {
    let e = StreamError::InvalidTick {
        reason: "price <= 0".into(),
    };
    assert!(e.to_string().contains("price <= 0"));
}

#[test]
fn test_error_lorentz_config_error_display() {
    let e = StreamError::LorentzConfigError {
        reason: "beta=1.0".into(),
    };
    assert!(e.to_string().contains("beta=1.0"));
}

// ── Exchange round-trip serialization ────────────────────────────────────────

#[test]
fn test_exchange_serde_roundtrip_all() {
    for ex in [
        Exchange::Binance,
        Exchange::Coinbase,
        Exchange::Alpaca,
        Exchange::Polygon,
    ] {
        let json = serde_json::to_string(&ex).unwrap();
        let back: Exchange = serde_json::from_str(&json).unwrap();
        assert_eq!(ex, back);
    }
}

#[test]
fn test_exchange_hash_stable() {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    map.insert(Exchange::Binance, 1u32);
    map.insert(Exchange::Coinbase, 2);
    assert_eq!(map[&Exchange::Binance], 1);
}

// ── TickNormalizer edge cases ─────────────────────────────────────────────────

#[test]
fn test_normalize_binance_missing_qty_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": "50000" }),
        received_at_ms: 0,
    };
    assert!(matches!(
        TickNormalizer::new().normalize(raw),
        Err(StreamError::ParseError { .. })
    ));
}

#[test]
fn test_normalize_coinbase_missing_size_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Coinbase,
        symbol: "BTC-USD".into(),
        payload: json!({ "price": "50000" }),
        received_at_ms: 0,
    };
    assert!(matches!(
        TickNormalizer::new().normalize(raw),
        Err(StreamError::ParseError { .. })
    ));
}

#[test]
fn test_normalize_alpaca_missing_price_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Alpaca,
        symbol: "AAPL".into(),
        payload: json!({ "s": "10" }),
        received_at_ms: 0,
    };
    assert!(matches!(
        TickNormalizer::new().normalize(raw),
        Err(StreamError::ParseError { .. })
    ));
}

#[test]
fn test_normalize_polygon_missing_size_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Polygon,
        symbol: "AAPL".into(),
        payload: json!({ "p": "180" }),
        received_at_ms: 0,
    };
    assert!(matches!(
        TickNormalizer::new().normalize(raw),
        Err(StreamError::ParseError { .. })
    ));
}

#[test]
fn test_normalized_tick_serde_roundtrip() {
    let tick = NormalizedTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        price: dec!(50000),
        quantity: dec!(1),
        side: Some(TradeSide::Buy),
        trade_id: Some("123".into()),
        exchange_ts_ms: Some(1700000000),
        received_at_ms: 1700000001,
    };
    let json = serde_json::to_string(&tick).unwrap();
    let back: NormalizedTick = serde_json::from_str(&json).unwrap();
    assert_eq!(back.price, tick.price);
    assert_eq!(back.exchange, tick.exchange);
    assert_eq!(back.side, tick.side);
}

#[test]
fn test_trade_side_equality() {
    assert_eq!(TradeSide::Buy, TradeSide::Buy);
    assert_ne!(TradeSide::Buy, TradeSide::Sell);
}

// ── OrderBook invariants ─────────────────────────────────────────────────────

#[test]
fn test_order_book_empty_spread_is_none() {
    let b = OrderBook::new("BTC-USD");
    assert!(b.spread().is_none());
}

#[test]
fn test_order_book_update_existing_bid_quantity() {
    let mut b = OrderBook::new("BTC-USD");
    b.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Bid,
        dec!(50000),
        dec!(5),
    ))
    .unwrap();
    b.apply(BookDelta::new(
        "BTC-USD",
        BookSide::Bid,
        dec!(50000),
        dec!(10),
    ))
    .unwrap();
    assert_eq!(b.bid_depth(), 1);
    assert_eq!(b.best_bid().unwrap().quantity, dec!(10));
}

#[test]
fn test_order_book_reset_crossed_is_error() {
    let mut b = OrderBook::new("BTC-USD");
    let result = b.reset(
        vec![PriceLevel::new(dec!(50100), dec!(1))],
        vec![PriceLevel::new(dec!(50000), dec!(1))],
    );
    assert!(matches!(result, Err(StreamError::BookCrossed { .. })));
}

#[test]
fn test_order_book_symbol_accessor() {
    let b = OrderBook::new("ETH-USD");
    assert_eq!(b.symbol(), "ETH-USD");
}

#[test]
fn test_price_level_serde() {
    let lvl = PriceLevel::new(dec!(100), dec!(5));
    let json = serde_json::to_string(&lvl).unwrap();
    let back: PriceLevel = serde_json::from_str(&json).unwrap();
    assert_eq!(back.price, lvl.price);
    assert_eq!(back.quantity, lvl.quantity);
}

#[test]
fn test_book_side_equality() {
    assert_eq!(BookSide::Bid, BookSide::Bid);
    assert_ne!(BookSide::Bid, BookSide::Ask);
}

// ── OHLCV multi-timeframe ────────────────────────────────────────────────────

fn make_tick(symbol: &str, price: Decimal, qty: Decimal, ts_ms: u64) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Coinbase,
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
fn test_ohlcv_5min_timeframe_bar_alignment() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(5)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 300_000))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(1), 599_999))
        .unwrap();
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 300_000);
}

#[test]
fn test_ohlcv_1h_timeframe_bar_start_ms() {
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Hours(1)).unwrap();
    agg.feed(&make_tick("ETH-USD", dec!(3000), dec!(1), 3_600_000))
        .unwrap();
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 3_600_000);
}

#[test]
fn test_ohlcv_open_equals_first_tick_price() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(30)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(65432), dec!(1), 30_000))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(66000), dec!(1), 30_100))
        .unwrap();
    assert_eq!(agg.current_bar().unwrap().open, dec!(65432));
}

#[test]
fn test_ohlcv_bar_not_complete_until_new_window() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
        .unwrap();
    assert!(!agg.current_bar().unwrap().is_complete);
}

#[test]
fn test_ohlcv_timeframe_serde() {
    let tf = Timeframe::Minutes(5);
    let json = serde_json::to_string(&tf).unwrap();
    let back: Timeframe = serde_json::from_str(&json).unwrap();
    assert_eq!(tf, back);
}

// ── OHLCV: period boundary, multiple bars, gap detection, volume ─────────────

/// A tick exactly on a boundary (e.g., ts_ms == 120_000 for 1-min bars)
/// starts a new bar, completing the previous one.
#[test]
fn test_ohlcv_tick_exactly_on_boundary_completes_bar() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
        .unwrap();
    let bars = agg
        .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 120_000))
        .unwrap();
    assert_eq!(bars.len(), 1);
    assert!(bars[0].is_complete);
    assert_eq!(bars[0].bar_start_ms, 60_000);
}

/// Three distinct 1-minute windows produce three separate completed bars
/// when ticked sequentially.
#[test]
fn test_ohlcv_multiple_bars_in_sequence() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    // Tick in window 1
    agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 60_000))
        .unwrap();
    // Tick in window 2 closes window 1
    let b1 = agg
        .feed(&make_tick("BTC-USD", dec!(200), dec!(1), 120_000))
        .unwrap();
    // Tick in window 3 closes window 2
    let b2 = agg
        .feed(&make_tick("BTC-USD", dec!(300), dec!(1), 180_000))
        .unwrap();
    // Flush window 3
    let b3 = agg.flush().unwrap();

    assert_eq!(b1.len(), 1);
    assert_eq!(b2.len(), 1);
    assert_eq!(b1[0].open, dec!(100));
    assert_eq!(b2[0].open, dec!(200));
    assert_eq!(b3.open, dec!(300));
}

/// Gap detection: with `emit_empty_bars` enabled, skipping 3 windows produces
/// the real bar plus 2 synthetic empty bars.
#[test]
fn test_ohlcv_gap_detection_emit_empty_bars() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1))
        .unwrap()
        .with_emit_empty_bars(true);
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000))
        .unwrap();
    // Jump 3 minutes ahead (skipping 120_000 and 180_000 windows)
    let bars = agg
        .feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 240_000))
        .unwrap();
    // 1 real + 2 empty gap bars
    assert_eq!(bars.len(), 3);
    assert!(!bars[0].volume.is_zero());
    assert!(bars[1].volume.is_zero());
    assert!(bars[2].volume.is_zero());
}

/// Volume accumulates correctly across multiple ticks in the same bar.
#[test]
fn test_ohlcv_volume_accumulation_all_ticks() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    let qtys = [dec!(0.5), dec!(1.25), dec!(2.0), dec!(0.75)];
    for (i, &q) in qtys.iter().enumerate() {
        agg.feed(&make_tick(
            "BTC-USD",
            dec!(50000),
            q,
            60_000 + i as u64 * 100,
        ))
        .unwrap();
    }
    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.volume, dec!(4.5));
    assert_eq!(bar.trade_count, 4);
}

/// All OHLCV fields are correctly set on a flushed bar.
#[test]
fn test_ohlcv_all_fields_correct_on_flush() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(30)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(100), dec!(5), 30_000))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(110), dec!(3), 30_100))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(90), dec!(2), 30_200))
        .unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(105), dec!(4), 30_300))
        .unwrap();
    let bar = agg.flush().unwrap();

    assert_eq!(bar.open, dec!(100));
    assert_eq!(bar.high, dec!(110));
    assert_eq!(bar.low, dec!(90));
    assert_eq!(bar.close, dec!(105));
    assert_eq!(bar.volume, dec!(14));
    assert_eq!(bar.trade_count, 4);
    assert!(bar.is_complete);
    assert_eq!(bar.bar_start_ms, 30_000);
    assert_eq!(bar.symbol, "BTC-USD");
}

// ── HealthMonitor edge cases ─────────────────────────────────────────────────

#[test]
fn test_health_monitor_re_register_overwrites() {
    let m = HealthMonitor::new(5_000);
    m.register("BTC-USD", Some(1_000));
    m.register("BTC-USD", Some(10_000));
    m.heartbeat("BTC-USD", 1_000_000).unwrap();
    let errors = m.check_all(1_002_000);
    assert!(errors.is_empty());
}

#[test]
fn test_health_monitor_default_threshold_applied() {
    let m = HealthMonitor::new(3_000);
    m.register("ETH-USD", None);
    m.heartbeat("ETH-USD", 1_000_000).unwrap();
    let errors = m.check_all(1_004_000);
    assert!(!errors.is_empty());
}

#[test]
fn test_health_status_serde() {
    use fin_stream::health::HealthStatus;
    for status in [
        HealthStatus::Healthy,
        HealthStatus::Stale,
        HealthStatus::Unknown,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let back: HealthStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}

#[test]
fn test_feed_health_serde_roundtrip() {
    use fin_stream::health::{FeedHealth, HealthStatus};
    let h = FeedHealth {
        feed_id: "BTC-USD".into(),
        status: HealthStatus::Healthy,
        last_tick_ms: Some(1_000_000),
        stale_threshold_ms: 5_000,
        tick_count: 42,
        consecutive_stale: 0,
    };
    let json = serde_json::to_string(&h).unwrap();
    let back: FeedHealth = serde_json::from_str(&json).unwrap();
    assert_eq!(back.feed_id, h.feed_id);
    assert_eq!(back.tick_count, h.tick_count);
}

// ── Session awareness edge cases ─────────────────────────────────────────────

#[test]
fn test_session_forex_open_monday_midday() {
    let sa = SessionAwareness::new(MarketSession::Forex);
    let mon_midday: u64 = 1704715200000;
    assert_eq!(sa.status(mon_midday).unwrap(), TradingStatus::Open);
}

#[test]
fn test_session_forex_closed_saturday_any_time() {
    let sa = SessionAwareness::new(MarketSession::Forex);
    let sat: u64 = 1705104000000;
    assert_eq!(sa.status(sat).unwrap(), TradingStatus::Closed);
    assert_eq!(
        sa.status(sat + 12 * 3600 * 1000).unwrap(),
        TradingStatus::Closed
    );
}

#[test]
fn test_trading_status_serde() {
    for status in [
        TradingStatus::Open,
        TradingStatus::Extended,
        TradingStatus::Closed,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let back: TradingStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}

#[test]
fn test_market_session_serde() {
    for session in [
        MarketSession::UsEquity,
        MarketSession::Crypto,
        MarketSession::Forex,
    ] {
        let json = serde_json::to_string(&session).unwrap();
        let back: MarketSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }
}

// ── WsManager edge cases ─────────────────────────────────────────────────────

#[test]
fn test_ws_manager_multiple_connects_track_attempts() {
    let config = ConnectionConfig::new("wss://feed.io/ws", 256).unwrap();
    let mut mgr = WsManager::new(config);
    mgr.connect_simulated();
    mgr.disconnect_simulated();
    mgr.connect_simulated();
    assert_eq!(mgr.connect_attempts(), 2);
    assert!(mgr.is_connected());
}

#[test]
fn test_reconnect_policy_backoff_attempt_0() {
    let p =
        ReconnectPolicy::new(5, Duration::from_millis(200), Duration::from_secs(30), 3.0).unwrap();
    assert_eq!(p.backoff_for_attempt(0), Duration::from_millis(200));
}

#[test]
fn test_connection_config_default_ping_interval() {
    let config = ConnectionConfig::new("wss://x.io/ws", 512).unwrap();
    assert!(config.ping_interval.as_secs() > 0);
}

#[test]
fn test_ws_manager_url_accessible_via_config() {
    let config = ConnectionConfig::new("wss://market.data.io/feed", 1024).unwrap();
    let mgr = WsManager::new(config);
    assert_eq!(mgr.config().url, "wss://market.data.io/feed");
}

// ── SPSC ring buffer extended tests ─────────────────────────────────────────

/// Capacity boundary: ring of N=8 holds exactly 7 items.
#[test]
fn test_ring_capacity_boundary_n8() {
    let r: SpscRing<u32, 8> = SpscRing::new();
    assert_eq!(r.capacity(), 7);
    for i in 0..7u32 {
        r.push(i).unwrap();
    }
    assert!(r.is_full());
    assert!(matches!(
        r.push(99).unwrap_err(),
        StreamError::RingBufferFull { capacity: 7 }
    ));
}

/// Capacity boundary: ring of N=4 holds exactly 3 items.
#[test]
fn test_ring_capacity_boundary_n4() {
    let r: SpscRing<u32, 4> = SpscRing::new();
    assert_eq!(r.capacity(), 3);
    for i in 0..3u32 {
        r.push(i).unwrap();
    }
    assert!(r.is_full());
}

/// FIFO ordering is preserved across many push/pop cycles.
#[test]
fn test_ring_fifo_ordering_extended() {
    let r: SpscRing<u64, 32> = SpscRing::new();
    for batch in 0u64..5 {
        for i in 0u64..10 {
            r.push(batch * 100 + i).unwrap();
        }
        for i in 0u64..10 {
            assert_eq!(r.pop().unwrap(), batch * 100 + i);
        }
    }
}

/// Wraparound: fill, drain, fill again; indices must not alias.
#[test]
fn test_ring_wraparound_fill_drain_refill() {
    let r: SpscRing<u32, 4> = SpscRing::new(); // capacity = 3
    r.push(1).unwrap();
    r.push(2).unwrap();
    r.push(3).unwrap();
    r.pop().unwrap();
    r.pop().unwrap();
    r.pop().unwrap();
    // Second fill
    r.push(10).unwrap();
    r.push(20).unwrap();
    r.push(30).unwrap();
    assert_eq!(r.pop().unwrap(), 10);
    assert_eq!(r.pop().unwrap(), 20);
    assert_eq!(r.pop().unwrap(), 30);
}

// ── Coordinate normalization extended tests ──────────────────────────────────

#[test]
fn test_normalization_range_always_0_to_1() {
    let mut n = MinMaxNormalizer::new(10);
    for i in 0..10 {
        n.update(i as f64 * 5.0);
    }
    for i in -5i64..55 {
        let v = n.normalize(i as f64).unwrap();
        assert!(
            (0.0..=1.0).contains(&v),
            "normalize({i}) = {v} is out of [0,1]"
        );
    }
}

#[test]
fn test_normalization_min_max_rolling_window() {
    let mut n = MinMaxNormalizer::new(3);
    n.update(10.0);
    n.update(20.0);
    n.update(30.0);
    let (min1, max1) = n.min_max().unwrap();
    assert_eq!(min1, 10.0);
    assert_eq!(max1, 30.0);
    // Push 40.0, evicting 10.0
    n.update(40.0);
    let (min2, max2) = n.min_max().unwrap();
    assert_eq!(min2, 20.0);
    assert_eq!(max2, 40.0);
}

#[test]
fn test_normalization_reset_then_renormalize() {
    let mut n = MinMaxNormalizer::new(5);
    for i in 0..5 {
        n.update(i as f64);
    }
    n.reset();
    assert!(n.is_empty());
    n.update(100.0);
    n.update(200.0);
    let v = n.normalize(150.0).unwrap();
    assert!((v - 0.5).abs() < 1e-10);
}

// ── Lorentz transform extended tests ─────────────────────────────────────────

#[test]
fn test_lorentz_beta_zero_identity_batch() {
    let lt = LorentzTransform::new(0.0).unwrap();
    let pts = vec![
        SpacetimePoint::new(0.0, 0.0),
        SpacetimePoint::new(1.0, 2.0),
        SpacetimePoint::new(-1.0, 3.0),
    ];
    for &p in &pts {
        let q = lt.transform(p);
        assert!((q.t - p.t).abs() < 1e-12);
        assert!((q.x - p.x).abs() < 1e-12);
    }
}

#[test]
fn test_lorentz_gamma_exceeds_one_for_nonzero_beta() {
    let lt = LorentzTransform::new(0.5).unwrap();
    assert!(lt.gamma() > 1.0);
}

#[test]
fn test_lorentz_gamma_increases_with_beta() {
    let lt1 = LorentzTransform::new(0.3).unwrap();
    let lt2 = LorentzTransform::new(0.7).unwrap();
    let lt3 = LorentzTransform::new(0.9).unwrap();
    assert!(lt1.gamma() < lt2.gamma());
    assert!(lt2.gamma() < lt3.gamma());
}

#[test]
fn test_lorentz_inverse_roundtrip_various_betas() {
    for &beta in &[0.0, 0.1, 0.5, 0.8, 0.99] {
        let lt = LorentzTransform::new(beta).unwrap();
        let p = SpacetimePoint::new(3.0, 1.5);
        let q = lt.transform(p);
        let r = lt.inverse_transform(q);
        assert!(
            (r.t - p.t).abs() < 1e-9,
            "t round-trip failed for beta={beta}"
        );
        assert!(
            (r.x - p.x).abs() < 1e-9,
            "x round-trip failed for beta={beta}"
        );
    }
}

#[test]
fn test_lorentz_spacetime_interval_invariant() {
    // The spacetime interval s^2 = t^2 - x^2 is Lorentz invariant.
    let lt = LorentzTransform::new(0.6).unwrap();
    let p = SpacetimePoint::new(5.0, 3.0);
    let q = lt.transform(p);
    let s2_before = p.t * p.t - p.x * p.x;
    let s2_after = q.t * q.t - q.x * q.x;
    assert!(
        (s2_before - s2_after).abs() < 1e-9,
        "spacetime interval not preserved: {s2_before} vs {s2_after}"
    );
}

#[test]
fn test_lorentz_transform_approaching_unity_beta() {
    // beta = 0.9999 -- gamma should be very large (>= 70)
    let lt = LorentzTransform::new(0.9999).unwrap();
    assert!(lt.gamma() > 70.0);
}
