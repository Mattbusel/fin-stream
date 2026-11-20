# fin-stream: a price is not a float

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

[![crates.io](https://img.shields.io/crates/v/fin-stream.svg)](https://crates.io/crates/fin-stream)
[![docs.rs](https://docs.rs/fin-stream/badge.svg)](https://docs.rs/fin-stream)
[![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Every exchange sends the same trade in a different shape: Binance and Coinbase send the price as a string, Alpaca and Polygon as a JSON number. Parse them all into `f64` and your bot has already agreed that 0.1 + 0.2 is 0.30000000000000004. fin-stream turns those messages into one exact-decimal tick, so the price your strategy sees is the price the exchange sent.

About 1.2 billion ring buffer push/pops a second on one thread.

**Why it exists.** Every Rust trading bot writes its own parser for each venue's trade message, and most of them land the price in `f64` on the first line. Even barter-data, the best-known Rust market-data crate, carries `PublicTrade { price: f64, amount: f64 }` (barter-rs `barter-data/src/subscription/trade.rs`, Nov 20 2026). fin-stream takes the other side: **a price is a `Decimal` from the byte the exchange sent to the bar your strategy reads.**

**Why this instead of your own parser.** Seven venues (Binance, Coinbase, Kraken, OKX, Bybit, Alpaca, Polygon) into one tick type; a missing or malformed field, or a zero-size print, is a typed `StreamError`, never a panic and never a silent zero; each tick records whether its timestamp is the exchange's or your machine's.

**Why today.** `cargo add fin-stream`, or `default-features = false, features = ["net"]` for just the WebSocket client and the normalizer. market-maker-rs users get it through [market-maker-fin-stream](https://crates.io/crates/market-maker-fin-stream).

**Who it is for:** Rust trading, market-making and recording code that reads live exchange trades.

**Trade-offs, on purpose:** `Decimal` costs more than `f64` per operation; we pay it at the edge so nothing downstream has to guess what the exchange meant.


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
