//! Extended test coverage — error display, tick edge cases, book invariants,
//! ohlcv timeframes, health edge cases, session boundary cases.

use fin_stream::book::{BookDelta, BookSide, OrderBook, PriceLevel};
use fin_stream::error::StreamError;
use fin_stream::health::HealthMonitor;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::session::{MarketSession, SessionAwareness, TradingStatus};
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::str::FromStr;
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

// ── Exchange round-trip serialization ────────────────────────────────────────

#[test]
fn test_exchange_serde_roundtrip_all() {
    for ex in [Exchange::Binance, Exchange::Coinbase, Exchange::Alpaca, Exchange::Polygon] {
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

// ── TickNormalizer edge cases ────────────────────────────────────────────────

#[test]
fn test_normalize_binance_missing_qty_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".into(),
        payload: json!({ "p": "50000" }), // missing q
        received_at_ms: 0,
    };
    assert!(matches!(TickNormalizer::new().normalize(raw), Err(StreamError::ParseError { .. })));
}

#[test]
fn test_normalize_coinbase_missing_size_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Coinbase,
        symbol: "BTC-USD".into(),
        payload: json!({ "price": "50000" }), // missing size
        received_at_ms: 0,
    };
    assert!(matches!(TickNormalizer::new().normalize(raw), Err(StreamError::ParseError { .. })));
}

#[test]
fn test_normalize_alpaca_missing_price_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Alpaca,
        symbol: "AAPL".into(),
        payload: json!({ "s": "10" }), // missing p
        received_at_ms: 0,
    };
    assert!(matches!(TickNormalizer::new().normalize(raw), Err(StreamError::ParseError { .. })));
}

#[test]
fn test_normalize_polygon_missing_size_returns_error() {
    let raw = RawTick {
        exchange: Exchange::Polygon,
        symbol: "AAPL".into(),
        payload: json!({ "p": "180" }), // missing s
        received_at_ms: 0,
    };
    assert!(matches!(TickNormalizer::new().normalize(raw), Err(StreamError::ParseError { .. })));
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
    b.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(5))).unwrap();
    b.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(10))).unwrap();
    // Only one level should exist at that price
    assert_eq!(b.bid_depth(), 1);
    assert_eq!(b.best_bid().unwrap().quantity, dec!(10));
}

#[test]
fn test_order_book_reset_crossed_is_error() {
    let mut b = OrderBook::new("BTC-USD");
    let result = b.reset(
        vec![PriceLevel::new(dec!(50100), dec!(1))],
        vec![PriceLevel::new(dec!(50000), dec!(1))], // bid > ask
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
    // Ticks in same 5-minute window
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 300_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50100), dec!(1), 599_999)).unwrap();
    // Still same bar
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 300_000);
}

#[test]
fn test_ohlcv_1h_timeframe_bar_start_ms() {
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Hours(1)).unwrap();
    agg.feed(&make_tick("ETH-USD", dec!(3000), dec!(1), 3_600_000)).unwrap();
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 3_600_000);
}

#[test]
fn test_ohlcv_open_equals_first_tick_price() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(30)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(65432), dec!(1), 30_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(66000), dec!(1), 30_100)).unwrap();
    assert_eq!(agg.current_bar().unwrap().open, dec!(65432));
}

#[test]
fn test_ohlcv_bar_not_complete_until_new_window() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
    assert!(!agg.current_bar().unwrap().is_complete);
}

#[test]
fn test_ohlcv_timeframe_serde() {
    let tf = Timeframe::Minutes(5);
    let json = serde_json::to_string(&tf).unwrap();
    let back: Timeframe = serde_json::from_str(&json).unwrap();
    assert_eq!(tf, back);
}

// ── HealthMonitor edge cases ─────────────────────────────────────────────────

#[test]
fn test_health_monitor_re_register_overwrites() {
    let m = HealthMonitor::new(5_000);
    m.register("BTC-USD", Some(1_000));
    m.register("BTC-USD", Some(10_000)); // re-register with new threshold
    m.heartbeat("BTC-USD", 1_000_000).unwrap();
    // 2s elapsed vs 10s threshold → still healthy
    let errors = m.check_all(1_002_000);
    assert!(errors.is_empty());
}

#[test]
fn test_health_monitor_default_threshold_applied() {
    let m = HealthMonitor::new(3_000); // 3s default
    m.register("ETH-USD", None);
    m.heartbeat("ETH-USD", 1_000_000).unwrap();
    let errors = m.check_all(1_004_000); // 4s > 3s threshold
    assert!(!errors.is_empty());
}

#[test]
fn test_health_status_serde() {
    use fin_stream::health::HealthStatus;
    for status in [HealthStatus::Healthy, HealthStatus::Stale, HealthStatus::Unknown] {
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
    // Monday 12:00 UTC — definitely open
    let mon_midday: u64 = 1704715200000; // Mon Jan 8 2024 12:00 UTC
    assert_eq!(sa.status(mon_midday).unwrap(), TradingStatus::Open);
}

#[test]
fn test_session_forex_closed_saturday_any_time() {
    let sa = SessionAwareness::new(MarketSession::Forex);
    let sat: u64 = 1705104000000;
    assert_eq!(sa.status(sat).unwrap(), TradingStatus::Closed);
    assert_eq!(sa.status(sat + 12 * 3600 * 1000).unwrap(), TradingStatus::Closed);
}

#[test]
fn test_trading_status_serde() {
    for status in [TradingStatus::Open, TradingStatus::Extended, TradingStatus::Closed] {
        let json = serde_json::to_string(&status).unwrap();
        let back: TradingStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}

#[test]
fn test_market_session_serde() {
    for session in [MarketSession::UsEquity, MarketSession::Crypto, MarketSession::Forex] {
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
    let p = ReconnectPolicy::new(5, Duration::from_millis(200), Duration::from_secs(30), 3.0).unwrap();
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
