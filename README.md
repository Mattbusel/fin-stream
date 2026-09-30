# fin-stream

**A Rust library that takes live trade messages from Binance, Coinbase, Alpaca and Polygon, turns them into one clean tick format with exact prices, hands them between threads through a lock-free ring buffer, and rolls them into OHLCV candles.**

For Rust developers building trading bots, market-data collectors, backtesters or crypto and stock analytics who want the exchange plumbing done once.

<p align="center">
  <a href="https://crates.io/crates/fin-stream"><img alt="crates.io version" src="https://img.shields.io/crates/v/fin-stream.svg"></a>
  <a href="https://docs.rs/fin-stream"><img alt="docs.rs" src="https://docs.rs/fin-stream/badge.svg"></a>
  <a href="https://gitlab.com/mattbusel/fin-stream/-/blob/main/LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

<p align="center">
  <img alt="A real terminal session: cargo run --example tape streams 40 BTC-USD trades from Binance, Coinbase, Alpaca and Polygon live over about five seconds, each with side, price, size bar and latency, a summary bar every two seconds, then totals per venue and ring usage" src="assets/demo.gif" width="100%">
</p>

## Install

```bash
cargo add fin-stream serde_json
```

It is a library, so there is nothing to download or install system-wide (Rust 1.75 or newer).
`serde_json` is only there so you can build raw payloads with `json!`. Or in `Cargo.toml`:
`fin-stream = "2.11"`. Want to see it first? `git clone https://gitlab.com/mattbusel/fin-stream && cd fin-stream && cargo run --example tape`.

## How it works

`WsManager` keeps the exchange WebSocket open (reconnecting with backoff) and hands you each
text frame. You wrap the frame in a `RawTick`; `TickNormalizer` turns any of the four venue
formats into one `NormalizedTick` with `Decimal` price and size. A feed thread pushes ticks into
an `SpscRing`, your thread pops them, and from there they go into bars, features or anything else.
Order book depth messages go into an `OrderBook` that rejects crossed books and sequence gaps.

<p align="center"><img alt="Animated diagram of the fin-stream pipeline replaying the first 17 trades of the tape example: WsManager passes WebSocket text frames down an mpsc channel; each frame becomes a RawTick and TickNormalizer turns the Binance, Coinbase, Alpaca or Polygon JSON into a NormalizedTick; the feed thread pushes it into an SpscRing slot and the main thread pops it; OhlcvAggregator returns the 14:30:00 two-second bar (open 64250.19, high 64272.65, low 64250.19, close 64254.78, 16 trades) when trade 17 arrives, a ZScoreNormalizer turns prices into z-scores, and OrderBook::apply returns BookCrossed and SequenceGap errors for bad depth updates" src="docs/img/pipeline.svg" width="100%"></p>

## Examples

Four programs ship in [`examples/`](examples/). No network, no API keys, same trades every run.
In a terminal they stream in real time; piped, they print instantly. `NO_COLOR=1` turns colors off.

| Run this | You get |
|---|---|
| `cargo run --example tape` | four venues' trades normalized on a feed thread, passed through an `SpscRing`, printed as a live time-and-sales tape and rolled into 2-second bars |
| `cargo run --example normalize` | one trade as each venue sends it, the one `NormalizedTick` each becomes, and three typed rejections |
| `cargo run --example feed_health` | `HealthMonitor` watching four feeds: one goes quiet, turns stale, trips its circuit and recovers |
| `cargo run --example replay` | a recorded NDJSON file streamed through the live-feed trait into 30-second bars |

**`tape`** is the recording at the top of this page: trades from four venues, one format, each 2-second bar printed the moment it closes.

**`feed_health`** (real output, run 2026-09-28, `NO_COLOR=1`):

```text
  binance   ││││││││││││││││││││││││││││││││││││││││││││││││  healthy   96 beats, last  0.0s ago
  coinbase  │││││││││····▒▒█████████││││││││││││││││││││││││  healthy   33 beats, last  0.0s ago
  alpaca    ·││·││·││·││·││·││·││·││·││·││·│···▒▒███████████  open      21 beats, last  8.2s ago
  polygon   ·····│·····│·····│·····│·····│·····│·····│·····│  healthy    8 beats, last  0.0s ago
            0s        5s        10s       15s       20s

  t+ 7.0s  coinbase  Feed 'coinbase' is stale: last tick was 2500ms ago (threshold: 2000ms)
  t+ 8.0s  coinbase  circuit OPEN after 3 stale checks
  t+12.5s  coinbase  heartbeat received, circuit closed
```

(`│` heartbeat, `·` quiet, `▒` stale, `█` circuit open. The header and the last lines are cut for length.)

<details>
<summary><b>tape</b> (full run), <b>normalize</b> and <b>replay</b> output</summary>

<br>

<p align="center"><img alt="Output of cargo run --example tape: 40 BTC-USD trades from four venues with time, venue, side, price colored by tick direction, size bar and latency, with a bar summary line every two seconds and a closing summary of ticks per venue, bars, and ring usage" src="assets/term-tape.png" width="840"></p>

<p align="center"><img alt="Output of cargo run --example normalize: four venue payloads for one trade and the normalized price, quantity, side and exchange timestamp each produces, then three malformed payloads and their StreamError messages" src="assets/term-normalize.png" width="840"></p>

<p align="center"><img alt="Output of cargo run --example replay: twenty 30-second bars drawn as horizontal candles on a fixed price axis, with close, change versus the session open and volume, from 600 recorded ticks" src="assets/term-replay.png" width="760"></p>

</details>

## Use it in 3 steps

**1. Make a project and add the crate**

```bash
cargo new tick-demo && cd tick-demo
cargo add fin-stream serde_json
```

**2. Put this in `src/main.rs`**

```rust
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let normalizer = TickNormalizer::new();

    // One BTC trade each, in the JSON shape each exchange really sends.
    let trades = [
        (Exchange::Binance, json!({"p": "64251.30", "q": "0.40", "m": true, "t": 7, "T": 1790260201205u64})),
        (Exchange::Coinbase, json!({"price": "64250.10", "size": "0.012", "side": "buy"})),
        (Exchange::Alpaca, json!({"p": 64252.0, "s": 0.05, "i": 99})),
        (Exchange::Polygon, json!({"p": 64249.75, "s": 0.2, "i": "p-1"})),
    ];

    // Four formats in, one tick type out.
    for (venue, payload) in trades {
        let tick = normalizer.normalize(RawTick::new(venue, "BTC-USD", payload))?;
        let side = tick.side.map_or("n/a".to_string(), |s| s.to_string());
        println!("{:<9} {:<5} {:>6} BTC @ {}", tick.exchange.to_string(), side, tick.quantity, tick.price);
    }

    // Bad input is a typed error, not a panic.
    let broken = RawTick::new(Exchange::Binance, "BTC-USD", json!({"q": "1"}));
    println!("no price  -> {}", normalizer.normalize(broken).unwrap_err());
    Ok(())
}
```

**3. Run it**

```bash
cargo run
```

You will see:

```text
Binance   sell    0.40 BTC @ 64251.30
Coinbase  buy    0.012 BTC @ 64250.10
Alpaca    n/a     0.05 BTC @ 64252.0
Polygon   n/a      0.2 BTC @ 64249.75
no price  -> Tick parse error from Binance: missing field 'p'
```

Four exchanges spell a trade four different ways; you get one `NormalizedTick` with exact
decimal price and size for each. Venues that do not say which side was the aggressor show
`n/a` rather than a guess, and a malformed message is an error you can match on. This exact
program was built against fin-stream 2.11 from crates.io and run again on 2026-09-28 (it is
also compiled and run by this repository's `cargo test --doc`).

## Documentation

| Read this | For |
|---|---|
| [docs.rs/fin-stream](https://docs.rs/fin-stream) | every type and method |
| [docs/EXAMPLES.md](docs/EXAMPLES.md) | short recipes: ring buffer, bars, normalizers, order book, feed health, sessions, Lorentz features |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | what every module does, supported exchanges and their wire fields, design rules, benchmark numbers, adding an exchange |
| [docs/REFERENCE.md](docs/REFERENCE.md) | module guides (multi-feed and NBBO aggregation, circuit breakers, feed quality, anomalies, replay, FIX 4.2, gRPC, OFI, VPIN, microstructure, regimes and more), math, API signatures, every `StreamError` |
| [docs/TESTING.md](docs/TESTING.md) | running the tests and benchmarks, current test status |
| [CHANGELOG.md](CHANGELOG.md) | what changed in each version |
| [Project site](https://fin-stream-rs.vercel.app/) | the same overview as a web page |

The optional gRPC server is behind the `grpc` feature.

> Research and engineering library. It does not place orders, and nothing here is financial advice.

## Contributing

Issues and pull requests are welcome. Public items need `///` docs (`#![deny(missing_docs)]`),
fallible code returns `Result<_, StreamError>`, and new behavior needs a test. Run `cargo fmt`,
`cargo clippy` and `cargo test --doc` before opening a PR. To add an exchange, follow
[Adding a new exchange adapter](docs/ARCHITECTURE.md#adding-a-new-exchange-adapter).

## License and related projects

MIT, see [LICENSE](LICENSE). Built on [fin-primitives](https://gitlab.com/mattbusel/fin-primitives)
(checked price and quantity types, order book, indicators, risk). The `lorentz` module comes from
the Special Relativity Financial Modeling work: [Special-Relativity-in-Financial-Modeling](https://gitlab.com/mattbusel/Special-Relativity-in-Financial-Modeling),
[srfm-python](https://gitlab.com/mattbusel/srfm-python), [srfm-paper-impl](https://gitlab.com/mattbusel/srfm-paper-impl) and [srfm-lab](https://gitlab.com/mattbusel/srfm-lab).
