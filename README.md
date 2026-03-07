# fin-stream

Real-time market data streaming primitives — 100K+ ticks/second ingestion pipeline built on Tokio.

## What's inside

- **SPSC ring buffer** — lock-free single-producer/single-consumer queue for zero-allocation tick ingestion
- **OHLCV aggregation pipeline** — streaming bar construction at any timeframe from raw tick data
- **Coordinate normalization** — rolling-window min-max normalization for ML feature pipelines
- **Spacetime transforms** — special-relativistic Lorentz transforms applied to financial time series
- **Stream engine** — composable pipeline stages: ingest → normalize → transform → emit

## Performance

Designed for sub-microsecond hot-path latency. The SPSC ring buffer sustains 100K+ ticks/second with no heap allocation on the fast path.

## Quick start

```rust
use fin_stream::{SPSCRing, StreamEngine, OHLCVTick};

let mut ring: SPSCRing<OHLCVTick, 1024> = SPSCRing::new();
ring.push_copy(tick)?;
let out = ring.pop().unwrap();
```

## Add to your project

```toml
[dependencies]
fin-stream = { git = "https://github.com/Mattbusel/fin-stream" }
```

## Test coverage

```bash
cargo test
cargo bench
```
