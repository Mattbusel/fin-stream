# fin-stream

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

[![crates.io](https://img.shields.io/crates/v/fin-stream.svg)](https://crates.io/crates/fin-stream)
[![docs.rs](https://docs.rs/fin-stream/badge.svg)](https://docs.rs/fin-stream)
[![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Live trades from four exchanges in one format, for Rust.

About 1.2 billion ring buffer push/pops a second on one thread.

- Reads Binance, Coinbase, Alpaca and Polygon.
- Gives exact decimal prices.
- Reconnects on its own.
- Rolls trades into OHLCV candles.
- Returns an error on bad input, never a panic.

## Install

```bash
cargo add fin-stream serde_json
```

## Use

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

[Docs](https://docs.rs/fin-stream) · [crates.io](https://crates.io/crates/fin-stream) · [Site](https://fin-stream-rs.vercel.app/)

More: [docs/DETAILS.md](docs/DETAILS.md)

MIT license. Does not place orders. Not financial advice.
