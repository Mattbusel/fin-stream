# fin-stream benchmarks against other crates

A small standalone crate (`publish = false`) that depends on fin-stream by path and on the
crates.io releases of [`rtrb`](https://crates.io/crates/rtrb) 0.3,
[`ringbuf`](https://crates.io/crates/ringbuf) 0.4 and
[`crossbeam-queue`](https://crates.io/crates/crossbeam-queue) 0.3.

```bash
cargo run --release          # histogram accuracy check
cargo bench --bench vs       # timings
```

Run on 2026-10-03, i7-13700KF, Windows 11, rustc 1.91.0, `opt-level=3`, thin LTO. The
machine was also running other builds, so read the numbers as ratios.

## Single-producer single-consumer queues (capacity 1023, `u64` items)

| benchmark | fin-stream `SpscRing` | rtrb | ringbuf | crossbeam `ArrayQueue` |
|---|---|---|---|---|
| one thread: push then pop, 1M times | 0.81 ms (1.24 G/s) | 1.77 ms | 6.03 ms | 9.06 ms |
| two threads: producer pushes 1M, consumer pops | 8.2 ms (121 M/s) | 6.8 ms (146 M/s) | 7.2 ms (139 M/s) | 12.0 ms (83 M/s) |

Across threads, rtrb and ringbuf are faster. Before 2.12 `SpscRing` kept `head` and `tail`
on one cache line and re-read the other side's index on every operation (94 M/s in the
same test); 2.12 pads them onto separate lines and caches the other index in the producer
and consumer halves, which brought it to 121 M/s. Its ring tests, including two
cross-thread ones, pass under Miri.

## Latency histogram (100,000 latency-shaped samples, then p99)

| | 2.11.3 (hand-rolled, 4 buckets per power of two) | 2.12 (hdrhistogram, 2 significant digits) |
|---|---|---|
| record 100k + p99 | 0.41 ms | 0.37 ms |
| p50 error | +15.9% | +0.36% |
| p90 error | +11.6% | 0.00% |
| p99 error | +2.4% | +0.25% |
| p99.9 error | +2.9% | +0.10% |
| p99.99 error | +5.7% | 0.00% |

Errors are against the exact percentile of the same samples (`cargo run --release`). The
2.11.3 code is copied into `src/lib.rs` so both versions run side by side.
