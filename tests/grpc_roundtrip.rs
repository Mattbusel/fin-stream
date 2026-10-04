//! Real gRPC round trip: a tonic server on 127.0.0.1 and the generated client.
//! Run with `cargo test --features grpc --test grpc_roundtrip`.
#![cfg(feature = "grpc")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fin_stream::grpc::proto::tick_stream_service_client::TickStreamServiceClient;
use fin_stream::grpc::proto::{SubscribeTicksRequest, TickFilter};
use fin_stream::grpc::TickStreamServer;
use fin_stream::tick::{Exchange, NormalizedTick, TradeSide};
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_stream::wrappers::TcpListenerStream;

fn tick(exchange: Exchange, symbol: &str, ts: u64) -> NormalizedTick {
    NormalizedTick {
        exchange,
        symbol: symbol.to_string(),
        price: dec!(64250.50),
        quantity: dec!(0.25),
        side: Some(TradeSide::Buy),
        trade_id: Some(format!("t{ts}")),
        exchange_ts_ms: Some(ts),
        received_at_ms: ts + 3,
    }
}

async fn start() -> (TickStreamServer, String) {
    let server = TickStreamServer::new(64);
    let svc = server.clone_service();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (server, format!("http://{addr}"))
}

async fn wait_for_subscriber(server: &TickStreamServer) {
    for _ in 0..500 {
        if server.subscriber_count() > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("client never subscribed");
}

#[tokio::test]
async fn filtered_ticks_arrive_with_exact_decimal_strings() {
    let (server, url) = start().await;
    let mut client = TickStreamServiceClient::connect(url).await.unwrap();
    let req = SubscribeTicksRequest {
        filter: Some(TickFilter { symbol: "BTC-USD".into(), exchange: "coinbase".into() }),
    };
    let mut stream = client.subscribe_ticks(req).await.unwrap().into_inner();
    wait_for_subscriber(&server).await;

    server.publish(tick(Exchange::Binance, "BTC-USD", 1)); // wrong exchange
    server.publish(tick(Exchange::Coinbase, "ETH-USD", 2)); // wrong symbol
    server.publish(tick(Exchange::Coinbase, "BTC-USD", 3)); // match

    let got = timeout(Duration::from_secs(5), stream.message()).await.unwrap().unwrap().unwrap();
    assert_eq!(got.exchange, "Coinbase");
    assert_eq!(got.symbol, "BTC-USD");
    assert_eq!(got.price, "64250.50");
    assert_eq!(got.quantity, "0.25");
    assert_eq!(got.trade_id, "t3");
    assert_eq!(got.exchange_ts_ms, 3);
    assert_eq!(got.received_at_ms, 6);
}

#[tokio::test]
async fn unknown_exchange_filter_is_rejected() {
    let (_server, url) = start().await;
    let mut client = TickStreamServiceClient::connect(url).await.unwrap();
    let req = SubscribeTicksRequest {
        filter: Some(TickFilter { symbol: String::new(), exchange: "kraken".into() }),
    };
    let err = client.subscribe_ticks(req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
