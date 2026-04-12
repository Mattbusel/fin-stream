use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fin_stream_core::book::{BookDelta, BookSide, OrderBook};
use fin_stream_core::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream_core::ring::SpscRing;
use fin_stream_core::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
use fin_stream_analytics::norm::{MinMaxNormalizer, ZScoreNormalizer};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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

fn bench_ohlcv_feed_with_completion(c: &mut Criterion) {
    c.bench_function("ohlcv_feed_bar_completion", |b| {
        let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(1)).unwrap();
        let mut ts: u64 = 1_000;
        b.iter(|| {
            let tick = make_normalized_tick(black_box(ts));
            ts += 1_000;
            black_box(agg.feed(&tick).unwrap())
        })
    });
}

fn bench_min_max_normalization_analytics(c: &mut Criterion) {
    let mut norm = MinMaxNormalizer::new(100).unwrap();
    for i in 0..100 {
        norm.update(Decimal::from(i));
    }
    c.bench_function("min_max_80_analytics_hot_path", |b| {
        b.iter(|| {
            norm.update(black_box(dec!(50)));
            // Simulate the high-volume analytical path
            let _ = black_box(norm.mean());
            let _ = black_box(norm.variance());
            let _ = black_box(norm.std_dev());
            let _ = black_box(norm.skewness());
            let _ = black_box(norm.kurtosis());
            let _ = black_box(norm.median());
            let _ = black_box(norm.percentile_value(0.99));
            black_box(norm.normalize(dec!(55)).unwrap())
        })
    });
}

fn bench_z_score_normalization_analytics(c: &mut Criterion) {
    let mut norm = ZScoreNormalizer::new(100).unwrap();
    for i in 0..100 {
        norm.update(Decimal::from(i));
    }
    c.bench_function("z_score_80_analytics_hot_path", |b| {
        b.iter(|| {
            norm.update(black_box(dec!(50)));
            let _ = black_box(norm.mean());
            let _ = black_box(norm.variance());
            let _ = black_box(norm.std_dev());
            black_box(norm.normalize(dec!(55)).unwrap())
        })
    });
}

criterion_group!(
    benches,
    bench_tick_normalize_binance,
    bench_ring_push_pop_tick,
    bench_ohlcv_feed_with_completion,
    bench_min_max_normalization_analytics,
    bench_z_score_normalization_analytics,
);
criterion_main!(benches);
