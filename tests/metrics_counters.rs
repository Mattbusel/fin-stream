//! The `metrics` feature: counters reach whatever recorder the application installs.
//! Run with `cargo test --features metrics --test metrics_counters`.
#![cfg(feature = "metrics")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::tick::{Exchange, NormalizedTick, TradeSide};
use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use futures_util::{SinkExt, StreamExt};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, tungstenite::Message};

type Snap = Vec<(
    metrics_util::CompositeKey,
    Option<metrics::Unit>,
    Option<metrics::SharedString>,
    DebugValue,
)>;

/// Take one snapshot (reading it resets the debugging recorder's counters).
fn take(snap: &Snapshotter) -> Snap {
    snap.snapshot().into_vec()
}

fn counter(snap: &Snap, name: &str, label: (&str, &str)) -> u64 {
    snap.iter()
        .find(|(key, _, _, _)| {
            key.key().name() == name
                && key.key().labels().any(|l| l.key() == label.0 && l.value() == label.1)
        })
        .map(|(_, _, _, v)| match v {
            DebugValue::Counter(c) => *c,
            other => panic!("{name} is not a counter: {other:?}"),
        })
        .unwrap_or(0)
}

fn tick(ts: u64, qty: rust_decimal::Decimal) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Binance,
        symbol: "BTC-USD".into(),
        price: dec!(100),
        quantity: qty,
        side: Some(TradeSide::Buy),
        trade_id: None,
        exchange_ts_ms: Some(ts),
        received_at_ms: ts,
    }
}

// One test function: the global recorder can only be installed once per process.
#[tokio::test]
async fn ws_and_bar_counters_are_emitted() {
    let recorder = DebuggingRecorder::new();
    let snap = recorder.snapshotter();
    recorder.install().unwrap();

    // Bars: two windows, one late tick.
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
    let _ = agg.feed(&tick(1_000, dec!(1))).unwrap();
    let _ = agg.feed(&tick(61_000, dec!(1))).unwrap(); // closes minute 0
    let _ = agg.feed(&tick(2_000, dec!(1))).unwrap(); // late
    let _ = agg.flush(); // closes minute 1
    let s1 = take(&snap);
    assert_eq!(counter(&s1, "fin_stream_ohlcv_bars_total", ("symbol", "BTC-USD")), 2);
    assert_eq!(counter(&s1, "fin_stream_ohlcv_late_ticks_total", ("symbol", "BTC-USD")), 1);

    // WebSocket: two sessions of two messages each, closed by the server.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        for _ in 0..2 {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            ws.send(Message::Text("12345".into())).await.unwrap();
            ws.send(Message::Text("abc".into())).await.unwrap();
            ws.close(None).await.unwrap();
            while ws.next().await.is_some() {}
        }
    });
    let policy = ReconnectPolicy::new(3, Duration::from_millis(5), Duration::from_millis(5), 1.0).unwrap();
    let cfg = ConnectionConfig::new(url.clone(), 16).unwrap().with_reconnect(policy);
    let (tx, mut rx) = mpsc::channel(16);
    let mut mgr = WsManager::new(cfg);
    let task = tokio::spawn(async move { mgr.run(tx, None).await });
    for _ in 0..4 {
        timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    }
    // Let the second close be processed and the third connect fail.
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(rx);
    let _ = timeout(Duration::from_secs(10), task).await;

    let l = ("url", url.as_str());
    let snap = take(&snap);
    assert_eq!(counter(&snap, "fin_stream_ws_messages_total", l), 4);
    assert_eq!(counter(&snap, "fin_stream_ws_bytes_total", l), 16);
    assert_eq!(counter(&snap, "fin_stream_ws_connects_total", l), 2);
    assert_eq!(counter(&snap, "fin_stream_ws_disconnects_total", l), 2);
    assert!(counter(&snap, "fin_stream_ws_connect_failures_total", l) >= 1);
}
