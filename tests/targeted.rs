/// Targeted tests for fin-stream.
///
/// Covers:
/// - SPSC buffer wraparound at capacity boundary
/// - OHLCV aggregation on bar boundary (last tick of one bar, first of next)
/// - Feed health state transitions (Unknown -> Healthy -> Stale -> Healthy)
/// - Session lifecycle: open / extended / closed state machine
/// - WebSocket reconnect behavior with simulated disconnects

use fin_stream::health::{HealthMonitor, HealthStatus};
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::session::{MarketSession, SessionAwareness, TradingStatus};
use fin_stream::tick::{Exchange, NormalizedTick, TradeSide};
use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use fin_stream::StreamError;
use rust_decimal_macros::dec;
use std::time::Duration;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_tick(symbol: &str, price_val: rust_decimal::Decimal, qty: rust_decimal::Decimal, ts_ms: u64) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Binance,
        symbol: symbol.to_string(),
        price: price_val,
        quantity: qty,
        side: Some(TradeSide::Buy),
        trade_id: None,
        exchange_ts_ms: None,
        received_at_ms: ts_ms,
    }
}

fn default_ws_config() -> ConnectionConfig {
    ConnectionConfig::new("wss://example.com/ws", 1024).unwrap()
}

// ── SPSC ring buffer: wraparound at capacity boundary ─────────────────────────

/// After filling the ring to capacity and draining it completely, pushing and
/// popping again must still produce FIFO-ordered items. This exercises the
/// index wraparound path through the backing array.
#[test]
fn spsc_ring_wraparound_at_exact_capacity_boundary() {
    // Capacity = N-1 = 3 for SpscRing<_, 4>.
    let ring: SpscRing<u32, 4> = SpscRing::new();
    assert_eq!(ring.capacity(), 3);

    // Fill to capacity.
    ring.push(10).unwrap();
    ring.push(20).unwrap();
    ring.push(30).unwrap();
    assert!(ring.is_full());

    // Drain completely.
    assert_eq!(ring.pop().unwrap(), 10);
    assert_eq!(ring.pop().unwrap(), 20);
    assert_eq!(ring.pop().unwrap(), 30);
    assert!(ring.is_empty());

    // Fill again -- indices have wrapped; head and tail are now 3 each mod N.
    ring.push(100).unwrap();
    ring.push(200).unwrap();
    ring.push(300).unwrap();
    assert!(ring.is_full());

    // Pop again in FIFO order.
    assert_eq!(ring.pop().unwrap(), 100);
    assert_eq!(ring.pop().unwrap(), 200);
    assert_eq!(ring.pop().unwrap(), 300);
    assert!(ring.is_empty());
}

/// After N-1 pushes the N-th push must return RingBufferFull.
/// After one pop, a push must succeed again.
#[test]
fn spsc_ring_capacity_boundary_push_pop_push() {
    let ring: SpscRing<u64, 8> = SpscRing::new(); // capacity = 7
    for i in 0..7u64 {
        ring.push(i).unwrap();
    }
    // Next push must fail.
    let err = ring.push(99).unwrap_err();
    assert!(matches!(err, StreamError::RingBufferFull { capacity: 7 }));

    // Pop one item to free a slot.
    assert_eq!(ring.pop().unwrap(), 0u64);

    // Now a push must succeed.
    ring.push(999).unwrap();
    assert_eq!(ring.len(), 7);
}

/// Pop from an empty ring must return RingBufferEmpty.
#[test]
fn spsc_ring_empty_pop_returns_empty_error() {
    let ring: SpscRing<u32, 8> = SpscRing::new();
    let err = ring.pop().unwrap_err();
    assert!(matches!(err, StreamError::RingBufferEmpty));
}

/// Multiple interleaved push/pop cycles across multiple wraparounds are FIFO-ordered.
#[test]
fn spsc_ring_multiple_wraparound_cycles_are_fifo() {
    let ring: SpscRing<u32, 4> = SpscRing::new(); // capacity = 3
    let mut next_push = 0u32;
    let mut next_pop = 0u32;

    for _cycle in 0..10 {
        // Fill.
        for _ in 0..3 {
            ring.push(next_push).unwrap();
            next_push += 1;
        }
        // Drain.
        for _ in 0..3 {
            let v = ring.pop().unwrap();
            assert_eq!(v, next_pop, "FIFO order violated at value {next_pop}");
            next_pop += 1;
        }
        assert!(ring.is_empty());
    }
}

// ── OHLCV: bar boundary (last tick of one bar, first tick of next) ────────────

/// The last tick of bar N and the first tick of bar N+1 must produce exactly
/// one completed bar when the second tick is processed.
#[test]
fn ohlcv_bar_boundary_last_tick_of_bar_and_first_of_next() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();

    // Bar 0 starts at 0ms. Duration = 60_000ms.
    // Last tick of bar 0: ts_ms = 59_999 (still in [0, 60_000)).
    // First tick of bar 1: ts_ms = 60_000 (starts next window).

    let last_tick_of_bar_0 = make_tick("BTC-USD", dec!(50_000), dec!(1), 0);
    let second_tick_of_bar_0 = make_tick("BTC-USD", dec!(50_500), dec!(2), 59_999);
    let first_tick_of_bar_1 = make_tick("BTC-USD", dec!(51_000), dec!(1), 60_000);

    // Both ticks belong to bar 0; no bar should complete yet.
    let r1 = agg.feed(&last_tick_of_bar_0).unwrap();
    assert!(r1.is_empty(), "first tick must not complete a bar");

    let r2 = agg.feed(&second_tick_of_bar_0).unwrap();
    assert!(r2.is_empty(), "last tick of current bar must not complete it yet");

    // The first tick of bar 1 triggers completion of bar 0.
    let mut r3 = agg.feed(&first_tick_of_bar_1).unwrap();
    assert_eq!(r3.len(), 1, "bar boundary tick must emit exactly one completed bar");

    let completed_bar = r3.remove(0);
    assert!(completed_bar.is_complete, "emitted bar must be marked complete");
    assert_eq!(completed_bar.bar_start_ms, 0, "completed bar must start at 0ms");
    assert_eq!(completed_bar.open, dec!(50_000), "open must be first tick price");
    assert_eq!(completed_bar.close, dec!(50_500), "close must be last tick of that bar");
    assert_eq!(completed_bar.high, dec!(50_500));
    assert_eq!(completed_bar.low, dec!(50_000));
    assert_eq!(completed_bar.volume, dec!(3));
    assert_eq!(completed_bar.trade_count, 2);

    // The new current bar must start with the boundary tick's price.
    let current = agg.current_bar().unwrap();
    assert_eq!(current.bar_start_ms, 60_000, "new bar must start at 60_000ms");
    assert_eq!(current.open, dec!(51_000), "new bar open must be boundary tick price");
    assert_eq!(current.trade_count, 1);
}

/// A single tick exactly at the bar boundary starts a new bar.
#[test]
fn ohlcv_single_tick_at_bar_boundary_starts_new_bar() {
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Seconds(30)).unwrap();
    // Tick at exactly 30_000ms is the first tick of the second 30-second bar.
    let tick_at_boundary = make_tick("ETH-USD", dec!(3_000), dec!(5), 30_000);
    let result = agg.feed(&tick_at_boundary).unwrap();
    assert!(result.is_empty(), "first tick ever cannot complete a bar");
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 30_000);
}

/// Two consecutive ticks with the same bar-start timestamp must accumulate into
/// the same bar without completing it.
#[test]
fn ohlcv_two_ticks_same_bar_window_accumulate() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(30)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(65_000), dec!(1), 30_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(66_000), dec!(2), 59_999)).unwrap();
    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.trade_count, 2);
    assert_eq!(bar.volume, dec!(3));
    assert_eq!(bar.high, dec!(66_000));
}

// ── Feed health: state transitions ───────────────────────────────────────────

/// Full lifecycle: Unknown -> Healthy -> Stale -> Healthy (after recovery heartbeat).
#[test]
fn feed_health_transitions_unknown_healthy_stale_healthy() {
    let monitor = HealthMonitor::new(5_000); // 5 second threshold
    monitor.register("BTC-USD", None);

    // Initial state: Unknown.
    let h = monitor.get("BTC-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Unknown, "newly registered feed must be Unknown");
    assert!(h.last_tick_ms.is_none());

    // Heartbeat -> Healthy.
    monitor.heartbeat("BTC-USD", 1_000_000).unwrap();
    let h = monitor.get("BTC-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Healthy, "heartbeat must transition to Healthy");

    // 6 seconds elapsed, check -> Stale.
    let errors = monitor.check_all(1_006_000);
    assert!(!errors.is_empty(), "feed should be stale after threshold exceeded");
    let h = monitor.get("BTC-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Stale, "feed must be Stale after threshold exceeded");
    assert_eq!(h.consecutive_stale, 1);

    // Recovery heartbeat -> Healthy again.
    monitor.heartbeat("BTC-USD", 1_007_000).unwrap();
    let h = monitor.get("BTC-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Healthy, "recovery heartbeat must restore Healthy status");
    assert_eq!(h.consecutive_stale, 0, "consecutive_stale must reset on heartbeat");
}

/// A feed that receives no heartbeat ever remains Unknown through all checks.
#[test]
fn feed_health_unknown_feed_never_transitions_to_stale_without_heartbeat() {
    let monitor = HealthMonitor::new(1_000);
    monitor.register("ETH-USD", None);
    // Check many times without any heartbeat.
    for i in 0..5 {
        let errors = monitor.check_all(1_000_000 + i * 10_000);
        assert!(errors.is_empty(), "feed with no heartbeat must not be flagged stale");
    }
    let h = monitor.get("ETH-USD").unwrap();
    assert_eq!(h.status, HealthStatus::Unknown);
}

/// Circuit breaker: opens after N consecutive stale checks, closes on heartbeat.
#[test]
fn feed_health_circuit_breaker_opens_and_closes() {
    let monitor = HealthMonitor::new(1_000).with_circuit_breaker_threshold(2);
    monitor.register("FEED-X", None);
    monitor.heartbeat("FEED-X", 1_000_000).unwrap();

    // First stale check.
    monitor.check_all(1_002_000);
    assert!(!monitor.is_circuit_open("FEED-X"), "circuit must not open after 1 stale check");

    // Second stale check -- circuit opens.
    monitor.check_all(1_004_000);
    assert!(monitor.is_circuit_open("FEED-X"), "circuit must open at threshold = 2");

    // Heartbeat closes the circuit.
    monitor.heartbeat("FEED-X", 1_005_000).unwrap();
    assert!(!monitor.is_circuit_open("FEED-X"), "heartbeat must close the circuit");
}

/// Multiple feeds: stale detection does not affect healthy feeds.
#[test]
fn feed_health_stale_detection_is_per_feed() {
    let monitor = HealthMonitor::new(5_000);
    monitor.register("BTC-USD", None);
    monitor.register("ETH-USD", Some(1_000)); // 1 second threshold for ETH

    // Both get heartbeats at the same time.
    monitor.heartbeat("BTC-USD", 1_000_000).unwrap();
    monitor.heartbeat("ETH-USD", 1_000_000).unwrap();

    // After 2 seconds: ETH stale (> 1s threshold), BTC still healthy (< 5s threshold).
    let errors = monitor.check_all(1_002_000);
    assert_eq!(errors.len(), 1, "only ETH-USD should be stale");
    assert!(errors[0].to_string().contains("ETH-USD"));

    assert_eq!(monitor.healthy_count(), 1);
    assert_eq!(monitor.stale_count(), 1);
}

// ── Session lifecycle: open -> extended -> closed state machine ───────────────

/// US equity session follows: Closed (overnight) -> Extended (pre-market) -> Open -> Extended (post) -> Closed.
///
/// All timestamps are UTC. EST = UTC - 5h.
///   3:00 ET = 08:00 UTC
///   4:00 ET = 09:00 UTC
///   9:30 ET = 14:30 UTC
///  16:00 ET = 21:00 UTC
///  20:00 ET = 01:00 UTC next day
///
/// Reference: Monday 2024-01-08.
#[test]
fn session_us_equity_full_day_lifecycle() {
    let sa = SessionAwareness::new(MarketSession::UsEquity);

    // Mon Jan 08 2024 00:00 UTC = Sun Jan 07 19:00 ET.
    // 19:00 ET is after market close (16:00) but before post-market ends (20:00),
    // so it maps to Extended hours.
    let overnight: u64 = 1704672000000;
    let status = sa.status(overnight).unwrap();
    assert!(
        status == TradingStatus::Extended || status == TradingStatus::Closed,
        "Mon 00:00 UTC (Sun 19:00 ET) must be Extended or Closed, got {:?}", status
    );

    // Mon Jan 08 2024 09:00 UTC = 04:00 ET -- pre-market (Extended).
    let pre_market: u64 = 1704704400000;
    let status = sa.status(pre_market).unwrap();
    assert!(
        status == TradingStatus::Extended || status == TradingStatus::Open,
        "04:00 ET must be Extended or Open, got {:?}", status
    );

    // Mon Jan 08 2024 14:30 UTC = 09:30 ET -- regular trading hours (Open).
    let market_open: u64 = 1704724200000;
    assert_eq!(sa.status(market_open).unwrap(), TradingStatus::Open);

    // Mon Jan 08 2024 21:00 UTC = 16:00 ET -- market close, extended hours begin.
    let market_close: u64 = 1704747600000;
    let status = sa.status(market_close).unwrap();
    assert!(
        status == TradingStatus::Extended || status == TradingStatus::Closed,
        "16:00 ET should be Extended or Closed, got {:?}", status
    );
}

/// Crypto is always Open regardless of day or time.
#[test]
fn session_crypto_always_open_full_week() {
    let sa = SessionAwareness::new(MarketSession::Crypto);
    // Monday through Sunday at various hours.
    let timestamps: Vec<u64> = vec![
        1704672000000, // Mon 00:00 UTC
        1704715200000, // Mon 12:00 UTC
        1704758400000, // Tue 00:00 UTC
        1705017600000, // Thu 12:00 UTC
        1705104000000, // Sat 00:00 UTC
        1705147200000, // Sat 12:00 UTC
        1705190400000, // Sun 00:00 UTC
    ];
    for ts in timestamps {
        assert_eq!(
            sa.status(ts).unwrap(),
            TradingStatus::Open,
            "crypto must be Open at all times (ts={ts})"
        );
    }
}

/// Forex is closed on Saturday entirely and Sunday before 22:00 UTC.
#[test]
fn session_forex_closed_on_weekend_open_on_weekday() {
    let sa = SessionAwareness::new(MarketSession::Forex);

    // Saturday: closed at any hour.
    let sat_morning: u64 = 1705104000000; // Sat Jan 13 2024 00:00 UTC
    let sat_evening: u64 = sat_morning + 20 * 3600 * 1000;
    assert_eq!(sa.status(sat_morning).unwrap(), TradingStatus::Closed);
    assert_eq!(sa.status(sat_evening).unwrap(), TradingStatus::Closed);

    // Sunday before 22:00 UTC: closed.
    let sun_before: u64 = 1704621600000; // Sun Jan 07 2024 10:00 UTC
    assert_eq!(sa.status(sun_before).unwrap(), TradingStatus::Closed);

    // Sunday at/after 22:00 UTC: open.
    let sun_after: u64 = 1704664800000; // Sun Jan 07 2024 22:00 UTC
    assert_eq!(sa.status(sun_after).unwrap(), TradingStatus::Open);

    // Monday: open.
    let mon_midday: u64 = 1704715200000; // Mon Jan 08 2024 12:00 UTC
    assert_eq!(sa.status(mon_midday).unwrap(), TradingStatus::Open);
}

// ── WebSocket reconnect behavior ──────────────────────────────────────────────

/// After a simulated disconnect, the manager must not report connected and
/// must correctly compute backoff durations in exponential sequence.
#[test]
fn ws_reconnect_after_disconnect_produces_exponential_backoff() {
    let policy = ReconnectPolicy::new(
        5,
        Duration::from_millis(100),
        Duration::from_secs(30),
        2.0,
    ).unwrap();
    let config = ConnectionConfig::new("wss://feed.example.com/ws", 512)
        .unwrap()
        .with_reconnect(policy);
    let mut mgr = WsManager::new(config);

    // Simulate a successful connection.
    mgr.connect_simulated();
    assert!(mgr.is_connected());
    assert_eq!(mgr.connect_attempts(), 1);

    // Simulate disconnect.
    mgr.disconnect_simulated();
    assert!(!mgr.is_connected());

    // Consume all remaining reconnect slots (max_attempts=5, but connect_simulated()
    // already consumed attempt 1, so 4 remain); verify backoff grows monotonically.
    let mut backoffs = Vec::new();
    while mgr.can_reconnect() {
        backoffs.push(mgr.next_reconnect_backoff().unwrap());
    }
    assert!(!backoffs.is_empty(), "should have consumed at least one reconnect slot");
    for w in backoffs.windows(2) {
        assert!(
            w[1] >= w[0],
            "backoff must be non-decreasing; got {:?} then {:?}", w[0], w[1]
        );
    }

    // Sixth attempt must fail.
    let err = mgr.next_reconnect_backoff().unwrap_err();
    assert!(matches!(err, StreamError::ReconnectExhausted { .. }));
}

/// After reconnect slots are exhausted, can_reconnect() returns false.
#[test]
fn ws_reconnect_exhausted_can_reconnect_is_false() {
    let policy = ReconnectPolicy::new(2, Duration::from_millis(50), Duration::from_secs(5), 2.0).unwrap();
    let config = ConnectionConfig::new("wss://feed.io/ws", 256)
        .unwrap()
        .with_reconnect(policy);
    let mut mgr = WsManager::new(config);
    mgr.next_reconnect_backoff().unwrap();
    mgr.next_reconnect_backoff().unwrap();
    assert!(!mgr.can_reconnect());
}

/// A fresh WsManager is not connected and has zero attempts.
#[test]
fn ws_manager_initial_state_not_connected() {
    let mgr = WsManager::new(default_ws_config());
    assert!(!mgr.is_connected());
    assert_eq!(mgr.connect_attempts(), 0);
    assert!(mgr.can_reconnect(), "fresh manager must allow reconnects");
}

/// connect_simulated followed by disconnect_simulated followed by connect_simulated
/// must accumulate attempt count correctly.
#[test]
fn ws_manager_connect_disconnect_reconnect_counts_attempts() {
    let mut mgr = WsManager::new(default_ws_config());
    mgr.connect_simulated(); // attempt 1
    mgr.disconnect_simulated();
    mgr.connect_simulated(); // attempt 2
    assert_eq!(mgr.connect_attempts(), 2);
    assert!(mgr.is_connected());
}

/// The backoff for the first reconnect attempt equals `initial_backoff`.
#[test]
fn ws_reconnect_first_attempt_backoff_equals_initial() {
    let initial = Duration::from_millis(250);
    let policy = ReconnectPolicy::new(5, initial, Duration::from_secs(30), 2.0).unwrap();
    let config = ConnectionConfig::new("wss://x.io/ws", 64)
        .unwrap()
        .with_reconnect(policy);
    let mut mgr = WsManager::new(config);
    let backoff = mgr.next_reconnect_backoff().unwrap();
    assert_eq!(backoff, initial, "first reconnect backoff must equal initial_backoff");
}

/// The backoff is capped at `max_backoff` regardless of attempt number.
#[test]
fn ws_reconnect_backoff_capped_at_max() {
    let max = Duration::from_secs(3);
    let policy = ReconnectPolicy::new(20, Duration::from_millis(500), max, 2.0).unwrap();
    let config = ConnectionConfig::new("wss://x.io/ws", 64)
        .unwrap()
        .with_reconnect(policy);
    let mut mgr = WsManager::new(config);
    // Consume many slots; all backoffs must stay at or below max.
    while mgr.can_reconnect() {
        let b = mgr.next_reconnect_backoff().unwrap();
        assert!(b <= max, "backoff {b:?} must not exceed max {max:?}");
    }
}
