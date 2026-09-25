//! One trade, four wire formats, one `NormalizedTick`.
//!
//! Binance, Coinbase, Alpaca and Polygon each describe a trade with different
//! field names, number encodings, side conventions and timestamp formats.
//! `TickNormalizer` maps all four onto the same struct with exact `Decimal`
//! prices, and rejects malformed payloads with a typed `StreamError`.
//!
//! ```text
//! cargo run --example normalize
//! ```

use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use fin_stream::StreamError;
use serde_json::{json, Value};

mod support;
use support::{clock_ms, Paint, SESSION_START_MS};

fn main() -> Result<(), StreamError> {
    let paint = Paint::detect();
    let n = TickNormalizer::new();
    let ts = SESSION_START_MS + 112;

    let payloads: [(Exchange, Value, &str); 4] = [
        (
            Exchange::Binance,
            json!({"e":"trade","t":4120001u64,"p":"64250.10","q":"0.01200","T":ts,"m":false}),
            "strings for numbers; m = buyer is maker",
        ),
        (
            Exchange::Coinbase,
            json!({"trade_id":"91200337","price":"64250.10","size":"0.01200000",
                   "side":"buy","time":"2026-09-24T14:30:00.112Z"}),
            "side spelled out; ISO-8601 time",
        ),
        (
            Exchange::Alpaca,
            json!({"T":"t","i":88410023u64,"p":64250.10,"s":0.012,"t":"2026-09-24T14:30:00.112Z"}),
            "JSON numbers; no side",
        ),
        (
            Exchange::Polygon,
            json!({"ev":"XT","i":"55c1e07a","p":64250.10,"s":0.012,"t":ts * 1_000_000}),
            "nanosecond epoch; no side",
        ),
    ];

    println!();
    println!(
        "  {}  {}",
        paint.bold("TickNormalizer"),
        paint.dim("the same BTC-USD trade as four venues send it")
    );
    for (exchange, payload, note) in payloads {
        println!();
        println!(
            "  {:<9} {}",
            paint.bold(exchange.to_string().to_lowercase()),
            paint.dim(note)
        );
        println!("  {:<9} {}", "", paint.brass(payload.to_string()));
        let tick = n.normalize(RawTick {
            exchange,
            symbol: "BTC-USD".into(),
            payload,
            received_at_ms: ts + 21,
        })?;
        println!(
            "  {:<9} {} {}",
            "",
            paint.dim("->"),
            describe(&paint, &tick)
        );
    }

    println!();
    println!(
        "  {}  {}",
        paint.bold("rejected"),
        paint.dim("typed errors, never a panic")
    );
    let bad: [(Exchange, Value); 3] = [
        (Exchange::Binance, json!({"q":"0.5","T":ts})),
        (
            Exchange::Coinbase,
            json!({"price":"-3.10","size":"1","side":"buy"}),
        ),
        (Exchange::Alpaca, json!({"p":"sixty-four thousand","s":1})),
    ];
    for (exchange, payload) in bad {
        let shown = payload.to_string();
        let err = n
            .normalize(RawTick {
                exchange,
                symbol: "BTC-USD".into(),
                payload,
                received_at_ms: ts,
            })
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        println!();
        println!(
            "  {:<9} {}",
            exchange.to_string().to_lowercase(),
            paint.brass(shown)
        );
        println!("  {:<9} {} {}", "", paint.dim("->"), paint.ask(err));
    }
    println!();
    Ok(())
}

fn describe(paint: &Paint, t: &NormalizedTick) -> String {
    let side = match t.side {
        Some(TradeSide::Buy) => paint.bid("Buy").to_string(),
        Some(TradeSide::Sell) => paint.ask("Sell").to_string(),
        None => paint.dim("None").to_string(),
    };
    format!(
        "price {}  qty {}  side {}  exch_ts {}",
        paint.bold(t.price.to_string()),
        t.quantity,
        side,
        t.exchange_ts_ms.map_or("None".into(), clock_ms),
    )
}
