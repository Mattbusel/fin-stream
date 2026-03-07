use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use serde_json::json;

fn make_binance_raw() -> RawTick {
    RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".to_string(),
        payload: json!({ "p": "50000.12", "q": "0.001", "m": false, "t": 12345u64, "T": 1700000000000u64 }),
        received_at_ms: 1700000000001,
    }
}

fn bench_tick_normalize_binance(c: &mut Criterion) {
    let normalizer = TickNormalizer::new();
    let raw = make_binance_raw();
    c.bench_function("tick_normalize_binance", |b| {
        b.iter(|| {
            let r = RawTick {
                exchange: raw.exchange,
                symbol: raw.symbol.clone(),
                payload: raw.payload.clone(),
                received_at_ms: raw.received_at_ms,
            };
            black_box(normalizer.normalize(r).unwrap())
        })
    });
}

fn bench_tick_normalize_coinbase(c: &mut Criterion) {
    let normalizer = TickNormalizer::new();
    c.bench_function("tick_normalize_coinbase", |b| {
        b.iter(|| {
            let r = RawTick {
                exchange: Exchange::Coinbase,
                symbol: "BTC-USD".to_string(),
                payload: json!({ "price": "50001.00", "size": "0.5", "side": "buy", "trade_id": "abc123" }),
                received_at_ms: 0,
            };
            black_box(normalizer.normalize(r).unwrap())
        })
    });
}

fn bench_exchange_parse(c: &mut Criterion) {
    c.bench_function("exchange_from_str", |b| {
        b.iter(|| {
            black_box("binance".parse::<fin_stream::tick::Exchange>().unwrap())
        })
    });
}

criterion_group!(benches, bench_tick_normalize_binance, bench_tick_normalize_coinbase, bench_exchange_parse);
criterion_main!(benches);
