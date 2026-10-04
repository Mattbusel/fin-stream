//! End-to-end tests for `WsManager` against a real WebSocket server on 127.0.0.1.
//!
//! The server side is tokio-tungstenite's own `accept_async`, so these tests run
//! the full handshake, framing, close and reconnect paths with no network access.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fin_stream::ws::{ConnectionConfig, ReconnectPolicy, WsManager};
use fin_stream::StreamError;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, tungstenite::Message};

async fn listener() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", l.local_addr().unwrap());
    (l, url)
}

fn fast_policy(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        max_attempts,
        Duration::from_millis(10),
        Duration::from_millis(50),
        2.0,
    )
    .unwrap()
}

#[tokio::test]
async fn forwards_text_and_utf8_binary_frames() {
    let (l, url) = listener().await;
    tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        ws.send(Message::Text("one".into())).await.unwrap();
        ws.send(Message::Binary(b"two".to_vec().into())).await.unwrap();
        ws.send(Message::Binary(vec![0xff, 0xfe].into())).await.unwrap(); // not UTF-8
        ws.send(Message::Text("three".into())).await.unwrap();
        // Keep the socket open until the client goes away.
        while ws.next().await.is_some() {}
    });

    let (tx, mut rx) = mpsc::channel(16);
    let cfg = ConnectionConfig::new(url, 16).unwrap().with_reconnect(fast_policy(3));
    let mut mgr = WsManager::new(cfg);
    let task = tokio::spawn(async move {
        let r = mgr.run(tx, None).await;
        (r, mgr.stats().total_messages_received)
    });

    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap());
    }
    assert_eq!(got, ["one", "two", "three"]);
    drop(rx);
    let (r, received) = timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(received, 4, "the non-UTF-8 frame is counted but not forwarded");
}

#[tokio::test]
async fn outbound_messages_reach_the_server_and_a_dropped_sender_is_harmless() {
    let (l, url) = listener().await;
    let (seen_tx, mut seen_rx) = mpsc::channel::<String>(4);
    tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        // Wait for the subscription, then answer after the client dropped its sender.
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                seen_tx.send(t.to_string()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                ws.send(Message::Text("ack".into())).await.unwrap();
            }
        }
    });

    let (tx, mut rx) = mpsc::channel(16);
    let (out_tx, out_rx) = mpsc::channel(4);
    let cfg = ConnectionConfig::new(url, 16).unwrap().with_reconnect(fast_policy(3));
    let mut mgr = WsManager::new(cfg);
    let task = tokio::spawn(async move { mgr.run(tx, Some(out_rx)).await });

    out_tx.send(r#"{"op":"subscribe"}"#.to_string()).await.unwrap();
    drop(out_tx);
    let seen = timeout(Duration::from_secs(5), seen_rx.recv()).await.unwrap().unwrap();
    assert_eq!(seen, r#"{"op":"subscribe"}"#);
    let ack = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(ack, "ack");
    drop(rx);
    assert!(timeout(Duration::from_secs(5), task).await.unwrap().unwrap().is_ok());
}

/// A feed that connects fine but is closed by the server every so often must keep
/// reconnecting. Before 2.12 every successful connection used up reconnect slots, so
/// with `max_attempts = 1` the second server-side close ended the stream for good.
#[tokio::test]
async fn established_connections_reset_the_reconnect_budget() {
    let (l, url) = listener().await;
    tokio::spawn(async move {
        for i in 0..4 {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();
            ws.send(Message::Text(format!("session {i}").into())).await.unwrap();
            ws.close(None).await.unwrap();
            while ws.next().await.is_some() {}
        }
    });

    let (tx, mut rx) = mpsc::channel(16);
    let cfg = ConnectionConfig::new(url, 16).unwrap().with_reconnect(fast_policy(1));
    let mut mgr = WsManager::new(cfg);
    let task = tokio::spawn(async move { mgr.run(tx, None).await });

    for i in 0..4 {
        let msg = timeout(Duration::from_secs(5), rx.recv()).await.unwrap();
        assert_eq!(msg.as_deref(), Some(format!("session {i}").as_str()));
    }
    drop(rx);
    let _ = timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
}

/// A server that completes the handshake and then goes silent (no data, no pong)
/// must be detected and replaced instead of hanging the reader forever.
#[tokio::test]
async fn silent_connection_is_detected_and_replaced() {
    let (l, url) = listener().await;
    tokio::spawn(async move {
        // First connection: handshake, then never read or write again.
        let (tcp, _) = l.accept().await.unwrap();
        let _silent = accept_async(tcp).await.unwrap();
        // Second connection: behaves.
        let (tcp, _) = l.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        ws.send(Message::Text("fresh".into())).await.unwrap();
        while ws.next().await.is_some() {}
        drop(_silent);
    });

    let (tx, mut rx) = mpsc::channel(16);
    let cfg = ConnectionConfig::new(url, 16)
        .unwrap()
        .with_reconnect(fast_policy(3))
        .with_ping_interval(Duration::from_millis(100));
    let mut mgr = WsManager::new(cfg);
    let task = tokio::spawn(async move { mgr.run(tx, None).await });

    let msg = timeout(Duration::from_secs(5), rx.recv()).await.unwrap();
    assert_eq!(msg.as_deref(), Some("fresh"));
    drop(rx);
    let _ = timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
}

#[tokio::test]
async fn gives_up_after_max_consecutive_failures() {
    // Bind and drop to get a port with nothing listening.
    let (l, url) = listener().await;
    drop(l);
    let (tx, _rx) = mpsc::channel(16);
    let cfg = ConnectionConfig::new(url, 16).unwrap().with_reconnect(fast_policy(2));
    let mut mgr = WsManager::new(cfg);
    let r = timeout(Duration::from_secs(10), mgr.run(tx, None)).await.unwrap();
    match r {
        Err(StreamError::ReconnectExhausted { attempts, .. }) => assert_eq!(attempts, 2),
        other => panic!("expected ReconnectExhausted, got {other:?}"),
    }
    // The first attempt plus two reconnects.
    assert_eq!(mgr.connect_attempts(), 3);
}
