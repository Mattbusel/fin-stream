use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fin_stream::book::{BookDelta, BookSide, OrderBook};
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;

fn make_binance_raw() -> RawTick {
    RawTick {
        exchange: Exchange::Binance,
        symbol: "BTCUSDT".to_string(),
        payload: json!({ "p": "50000.12", "q": "0.001", "m": false, "t": 12345u64, "T": 1700000000000u64 }),
        received_at_ms: 1700000000001,
    }
}

fn make_normalized_tick(ts_ms: u64) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Binance,
        symbol: "BTC-USD".to_string(),
        price: dec!(50000),
        quantity: dec!(1),
        side: None,
        trade_id: None,
        exchange_ts_ms: Some(ts_ms),
        received_at_ms: ts_ms,
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
        b.iter(|| black_box("binance".parse::<fin_stream::tick::Exchange>().unwrap()))
    });
}

/// Benchmark the SPSC ring buffer push+pop round-trip.
fn bench_ring_push_pop(c: &mut Criterion) {
    c.bench_function("ring_push_pop_u64", |b| {
        let ring: SpscRing<u64, 128> = SpscRing::new();
        let mut i: u64 = 0;
        b.iter(|| {
            ring.push(black_box(i)).unwrap();
            let v = ring.pop().unwrap();
            i = i.wrapping_add(1);
            black_box(v)
        })
    });
}

/// Benchmark ring buffer with NormalizedTick (realistic payload size).
fn bench_ring_push_pop_tick(c: &mut Criterion) {
    c.bench_function("ring_push_pop_normalized_tick", |b| {
        let ring: SpscRing<NormalizedTick, 256> = SpscRing::new();
        b.iter(|| {
            let tick = make_normalized_tick(black_box(1700000000000));
            ring.push(tick).unwrap();
            black_box(ring.pop().unwrap())
        })
    });
}

/// Benchmark OHLCV feed within a single bar window (no bar completion).
fn bench_ohlcv_feed_same_window(c: &mut Criterion) {
    c.bench_function("ohlcv_feed_same_window", |b| {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();
        let mut ts: u64 = 60_000;
        b.iter(|| {
            let tick = make_normalized_tick(black_box(ts));
            ts += 100;
            black_box(agg.feed(&tick).unwrap())
        })
    });
}

/// Benchmark OHLCV feed across bar boundaries (triggers bar completion).
fn bench_ohlcv_feed_with_completion(c: &mut Criterion) {
    c.bench_function("ohlcv_feed_bar_completion", |b| {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(1)).unwrap();
        let mut ts: u64 = 1_000;
        b.iter(|| {
            let tick = make_normalized_tick(black_box(ts));
            ts += 1_000; // each tick advances a full second → bar completes every call
            black_box(agg.feed(&tick).unwrap())
        })
    });
}

/// Benchmark order book apply (single-level update, no completion).
fn bench_order_book_apply(c: &mut Criterion) {
    c.bench_function("order_book_apply_delta", |b| {
        let mut book = OrderBook::new("BTC-USD");
        // Seed the book so ask side exists and crossed check is meaningful.
        book.apply(BookDelta::new(
            "BTC-USD",
            BookSide::Ask,
            dec!(50100),
            dec!(1),
        ))
        .unwrap();
        let mut qty = dec!(1);
        b.iter(|| {
            let delta = BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), qty);
            qty += dec!(0.001);
            black_box(book.apply(delta).unwrap())
        })
    });
}

/// Benchmark order book best_bid + best_ask lookup (hot read path).
fn bench_order_book_best_levels(c: &mut Criterion) {
    c.bench_function("order_book_best_levels", |b| {
        let mut book = OrderBook::new("BTC-USD");
        for i in 0u32..10 {
            let bid = dec!(49900) + Decimal::from(i * 10);
            let ask = dec!(50100) + Decimal::from(i * 10);
            book.apply(BookDelta::new("BTC-USD", BookSide::Bid, bid, dec!(1)))
                .unwrap();
            book.apply(BookDelta::new("BTC-USD", BookSide::Ask, ask, dec!(1)))
                .unwrap();
        }
        b.iter(|| {
            let bid = book.best_bid();
            let ask = book.best_ask();
            black_box((bid, ask))
        })
    });
}

criterion_group!(
    benches,
    bench_tick_normalize_binance,
    bench_tick_normalize_coinbase,
    bench_exchange_parse,
    bench_ring_push_pop,
    bench_ring_push_pop_tick,
    bench_ohlcv_feed_same_window,
    bench_ohlcv_feed_with_completion,
    bench_order_book_apply,
    bench_order_book_best_levels,
);
criterion_main!(benches);
