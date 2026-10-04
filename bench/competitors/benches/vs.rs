//! fin-stream vs other SPSC queues on the same work, and the new LatencyHistogram
//! vs the 2.11.3 one. Run: `cargo bench --bench vs` in this folder.

use competitors::OldLatencyHistogram;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use fin_stream::latency::LatencyHistogram;
use fin_stream::ring::SpscRing;
use ringbuf::traits::{Consumer, Producer, Split};
use std::time::{Duration, Instant};

const N: u64 = 1_000_000;
const CAP: usize = 1024;

/// Single thread: push one, pop one, N times (cost of the operations themselves).
fn same_thread(c: &mut Criterion) {
    let mut g = c.benchmark_group("spsc_same_thread_push_pop");
    g.throughput(Throughput::Elements(N));
    g.bench_function("fin-stream SpscRing", |b| {
        let ring: SpscRing<u64, CAP> = SpscRing::new();
        b.iter(|| {
            for i in 0..N {
                ring.push(i).unwrap();
                black_box(ring.pop().unwrap());
            }
        })
    });
    g.bench_function("rtrb", |b| {
        let (mut p, mut q) = rtrb::RingBuffer::<u64>::new(CAP - 1);
        b.iter(|| {
            for i in 0..N {
                p.push(i).unwrap();
                black_box(q.pop().unwrap());
            }
        })
    });
    g.bench_function("ringbuf", |b| {
        let (mut p, mut q) = ringbuf::HeapRb::<u64>::new(CAP - 1).split();
        b.iter(|| {
            for i in 0..N {
                p.try_push(i).unwrap();
                black_box(q.try_pop().unwrap());
            }
        })
    });
    g.bench_function("crossbeam ArrayQueue", |b| {
        let q = crossbeam_queue::ArrayQueue::<u64>::new(CAP - 1);
        b.iter(|| {
            for i in 0..N {
                q.push(i).unwrap();
                black_box(q.pop().unwrap());
            }
        })
    });
    g.finish();
}

/// Spin until the closure succeeds.
#[inline]
fn spin<F: FnMut() -> bool>(mut f: F) {
    while !f() {
        std::hint::spin_loop();
    }
}

/// Two threads: a producer pushes N items, the consumer pops them all (spinning on
/// full or empty), the way a feed thread hands ticks to a strategy thread.
fn cross_thread(c: &mut Criterion) {
    let mut g = c.benchmark_group("spsc_cross_thread_1m");
    g.throughput(Throughput::Elements(N));
    g.sample_size(20);
    g.bench_function("fin-stream SpscRing", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let (p, q) = SpscRing::<u64, CAP>::new().split();
                let start = Instant::now();
                let t = std::thread::spawn(move || {
                    for i in 0..N {
                        spin(|| p.push(i).is_ok());
                    }
                });
                for _ in 0..N {
                    spin(|| q.pop().map(|v| black_box(v)).is_ok());
                }
                t.join().unwrap();
                total += start.elapsed();
            }
            total
        })
    });
    g.bench_function("rtrb", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let (mut p, mut q) = rtrb::RingBuffer::<u64>::new(CAP - 1);
                let start = Instant::now();
                let t = std::thread::spawn(move || {
                    for i in 0..N {
                        spin(|| p.push(i).is_ok());
                    }
                });
                for _ in 0..N {
                    spin(|| q.pop().map(|v| black_box(v)).is_ok());
                }
                t.join().unwrap();
                total += start.elapsed();
            }
            total
        })
    });
    g.bench_function("ringbuf", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let (mut p, mut q) = ringbuf::HeapRb::<u64>::new(CAP - 1).split();
                let start = Instant::now();
                let t = std::thread::spawn(move || {
                    for i in 0..N {
                        spin(|| p.try_push(i).is_ok());
                    }
                });
                for _ in 0..N {
                    spin(|| q.try_pop().map(|v| black_box(v)).is_some());
                }
                t.join().unwrap();
                total += start.elapsed();
            }
            total
        })
    });
    g.bench_function("crossbeam ArrayQueue", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let q = std::sync::Arc::new(crossbeam_queue::ArrayQueue::<u64>::new(CAP - 1));
                let p = q.clone();
                let start = Instant::now();
                let t = std::thread::spawn(move || {
                    for i in 0..N {
                        spin(|| p.push(i).is_ok());
                    }
                });
                for _ in 0..N {
                    spin(|| q.pop().map(|v| black_box(v)).is_some());
                }
                t.join().unwrap();
                total += start.elapsed();
            }
            total
        })
    });
    g.finish();
}

fn histogram(c: &mut Criterion) {
    let samples = competitors::samples();
    let mut g = c.benchmark_group("latency_record_100k_then_p99");
    g.throughput(Throughput::Elements(samples.len() as u64));
    g.bench_function("2.11.3 bucketed", |b| {
        b.iter(|| {
            let mut h = OldLatencyHistogram::new();
            for &s in &samples {
                h.record(s);
            }
            black_box(h.percentile(99.0))
        })
    });
    g.bench_function("2.12 hdrhistogram", |b| {
        b.iter(|| {
            let mut h = LatencyHistogram::new();
            for &s in &samples {
                h.record(s);
            }
            black_box(h.percentile(99.0))
        })
    });
    g.finish();
}

criterion_group!(benches, same_thread, cross_thread, histogram);
criterion_main!(benches);
