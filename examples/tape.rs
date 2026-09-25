//! A live time-and-sales tape for one instrument traded on four venues.
//!
//! A feed thread builds each trade in the venue's own wire format (Binance,
//! Coinbase, Alpaca and Polygon all spell a trade differently), turns it into
//! a `NormalizedTick` with `TickNormalizer`, and pushes it into a lock-free
//! `SpscRing`. The main thread pops ticks off the ring, prints the tape, and
//! rolls the same ticks into 2-second OHLCV bars with `OhlcvAggregator`.
//!
//! ```text
//! cargo run --example tape
//! ```
//!
//! Prices come from a seeded random walk, so there is no network, no API key,
//! and the trades are the same on every run (only the ring and wall-clock stats
//! on the last line depend on thread timing). In a terminal the tape streams in
//! real time (about 5 seconds); piped or redirected it prints instantly.
//! `NO_COLOR=1` turns colors off, `FORCE_COLOR=1` keeps them when piping.

use fin_stream::ohlcv::{OhlcvAggregator, OhlcvBar, Timeframe};
use fin_stream::ring::SpscRing;
use fin_stream::synthetic::{GeometricBrownianMotion, PriceModel, Xorshift64};
use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer, TradeSide};
use fin_stream::StreamError;
use rust_decimal::Decimal;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod support;
use support::{bar, clock, clock_ms, f, fixed, live, money, Paint, SESSION_START_MS};

const SYMBOL: &str = "BTC-USD";
const TICKS: usize = 40;
const RING: usize = 64;

fn main() -> Result<(), StreamError> {
    let paint = Paint::detect();
    let pace = live();

    println!();
    println!(
        "  {}  {}",
        paint.bold(SYMBOL),
        paint.dim("time and sales, 4 venues, 2s bars")
    );
    println!(
        "  {}",
        paint.dim("venue JSON -> TickNormalizer -> SpscRing<NormalizedTick, 64> -> tape + OhlcvAggregator")
    );
    println!();
    println!(
        "  {}",
        paint.dim("time (exch)    venue      side        price      size           latency")
    );

    let ring: SpscRing<NormalizedTick, RING> = SpscRing::new();
    let (tx, rx) = ring.split();
    let done = Arc::new(AtomicBool::new(false));
    let feed_done = Arc::clone(&done);

    // Feed thread: the "network" side. Builds venue payloads, normalizes them,
    // and pushes into the ring. A full ring means the consumer is behind; we
    // spin briefly instead of dropping ticks.
    let feed = std::thread::spawn(move || -> Result<u64, StreamError> {
        let normalizer = TickNormalizer::new();
        let mut gen = VenueFeed::new(7);
        let started = Instant::now();
        let mut full_spins = 0u64;
        for _ in 0..TICKS {
            let raw = gen.next_raw();
            if pace {
                let due = Duration::from_millis(raw.received_at_ms - SESSION_START_MS);
                if let Some(wait) = due.checked_sub(started.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            let tick = normalizer.normalize(raw)?;
            // Only this thread pushes, so once the ring has room the push
            // cannot fail. Wait for room instead of dropping the tick.
            while tx.is_full() {
                full_spins += 1;
                std::thread::yield_now();
            }
            tx.push(tick)?;
        }
        feed_done.store(true, Ordering::Release);
        Ok(full_spins)
    });

    // Consumer: the "strategy" side.
    let mut agg = OhlcvAggregator::new(SYMBOL, Timeframe::Seconds(2))?;
    let mut bars: Vec<OhlcvBar> = Vec::new();
    let mut per_venue: BTreeMap<String, usize> = BTreeMap::new();
    let mut last_px: Option<Decimal> = None;
    let mut peak_fill = 0usize;
    let mut notional = Decimal::ZERO;
    let mut volume = Decimal::ZERO;
    let mut lo = Decimal::MAX;
    let mut hi = Decimal::MIN;
    let started = Instant::now();
    let mut seen = 0usize;

    loop {
        peak_fill = peak_fill.max(rx.len());
        let tick = match rx.pop() {
            Ok(t) => t,
            Err(_) if done.load(Ordering::Acquire) && rx.is_empty() => break,
            Err(_) => {
                std::thread::sleep(Duration::from_micros(200));
                continue;
            }
        };
        seen += 1;

        for b in agg.feed(&tick)? {
            print_bar(&paint, &b);
            bars.push(b);
        }
        print_tick(&paint, &tick, last_px);

        last_px = Some(tick.price);
        *per_venue.entry(tick.exchange.to_string()).or_default() += 1;
        notional += tick.price * tick.quantity;
        volume += tick.quantity;
        lo = lo.min(tick.price);
        hi = hi.max(tick.price);
    }
    // The last window is still open when the tape ends; flush it as a partial bar.
    if let Some(b) = agg.flush() {
        print_bar(&paint, &b);
        bars.push(b);
    }

    let full_spins = match feed.join() {
        Ok(r) => r?,
        Err(_) => 0,
    };
    let elapsed = started.elapsed();

    println!();
    let venues: Vec<String> = per_venue
        .iter()
        .map(|(v, n)| format!("{} {n}", v.to_lowercase()))
        .collect();
    println!(
        "  {}  {}",
        paint.bold(format!("{seen} ticks")),
        paint.dim(venues.join("  "))
    );
    println!(
        "  {}  {}",
        paint.bold(format!("{} bars", bars.len())),
        paint.dim(format!(
            "range {} .. {}   vwap {}   volume {} BTC",
            money(lo, 2),
            money(hi, 2),
            money(notional / volume, 2),
            fixed(volume, 4)
        ))
    );
    println!(
        "  {}  {}",
        paint.bold("ring"),
        paint.dim(format!(
            "peak {peak_fill}/{RING} slots in use, {full_spins} full-ring retries, {} ms wall clock{}",
            elapsed.as_millis(),
            if pace { "" } else { " (instant mode)" }
        ))
    );
    println!();
    Ok(())
}

fn print_tick(paint: &Paint, t: &NormalizedTick, prev: Option<Decimal>) {
    let ts = t.exchange_ts_ms.unwrap_or(t.received_at_ms);
    let latency = t.received_at_ms.saturating_sub(ts);
    let side = match t.side {
        Some(TradeSide::Buy) => paint.bid("buy"),
        Some(TradeSide::Sell) => paint.ask("sell"),
        None => paint.dim("n/a"),
    };
    // Tick test: color the price by its direction versus the previous print.
    let px = money(t.price, 2);
    let (arrow, px) = match prev {
        Some(p) if t.price > p => (paint.bid("▲"), paint.bid(px)),
        Some(p) if t.price < p => (paint.ask("▼"), paint.ask(px)),
        _ => (paint.dim("·"), paint.plain(px)),
    };
    let size_bar = bar(f(t.quantity), 1.0, 9);
    let size_bar = match t.side {
        Some(TradeSide::Buy) => paint.bid(size_bar),
        Some(TradeSide::Sell) => paint.ask(size_bar),
        None => paint.dim(size_bar),
    };
    println!(
        "  {}   {:<9}  {:<4}  {} {:>10}  {:>8}  {:<9}  {:>5}",
        paint.dim(clock_ms(ts)),
        t.exchange.to_string().to_lowercase(),
        side,
        arrow,
        px,
        fixed(t.quantity, 4),
        size_bar,
        paint.dim(format!("{latency}ms"))
    );
}

fn print_bar(paint: &Paint, b: &OhlcvBar) {
    let chg = b.close - b.open;
    let body = format!(
        "{} bar  o {}  h {}  l {}  c {}  {} BTC",
        clock(b.bar_start_ms),
        money(b.open, 2),
        money(b.high, 2),
        money(b.low, 2),
        money(b.close, 2),
        fixed(b.volume, 2),
    );
    let mark = if chg > Decimal::ZERO {
        paint.bid(format!("+{}", fixed(chg, 2)))
    } else if chg < Decimal::ZERO {
        paint.ask(fixed(chg, 2))
    } else {
        paint.dim("0.00")
    };
    println!("  {}  {}", paint.brass(body), mark);
}

/// Generates trades for one instrument and serializes each one the way its
/// venue does on the wire. Deterministic for a given seed.
struct VenueFeed {
    rng: Xorshift64,
    px: GeometricBrownianMotion,
    clock_ms: u64,
    last: f64,
    seq: u64,
}

impl VenueFeed {
    fn new(seed: u64) -> Self {
        Self {
            rng: Xorshift64::new(seed),
            // Per-trade volatility of ~0.7 bps; dt = 1 step.
            px: GeometricBrownianMotion::new(0.0, 0.00007, 64_250.0),
            clock_ms: SESSION_START_MS,
            last: 64_250.0,
            seq: 4_120_000,
        }
    }

    fn next_raw(&mut self) -> RawTick {
        // Exponential inter-arrival, mean ~125 ms.
        let gap = (-(1.0 - self.rng.next_f64()).ln() * 125.0) as u64 + 5;
        self.clock_ms += gap;
        let price = (self.px.step(&mut self.rng, 1.0) * 100.0).round() / 100.0;
        let up = price >= self.last;
        self.last = price;
        // Aggressor side leans with the move, as it does in real flow.
        let buy = self.rng.next_f64() < if up { 0.8 } else { 0.2 };
        let qty = 0.0005 + -(1.0 - self.rng.next_f64()).ln() * 0.18;
        let venue = match self.rng.next_u64() % 10 {
            0..=3 => Exchange::Binance,
            4..=5 => Exchange::Coinbase,
            6..=7 => Exchange::Alpaca,
            _ => Exchange::Polygon,
        };
        let latency = match venue {
            Exchange::Binance => 38,
            Exchange::Coinbase => 11,
            Exchange::Alpaca => 19,
            Exchange::Polygon => 24,
        } + self.rng.next_u64() % 9;
        self.seq += 1 + self.rng.next_u64() % 3;
        let ts = self.clock_ms;
        let iso = rfc3339(ts);

        // Four wire formats for the same trade.
        let payload = match venue {
            // Strings for numbers, `m` = buyer is maker (so the seller aggressed).
            Exchange::Binance => json!({
                "e": "trade", "s": "BTCUSDT", "t": self.seq,
                "p": format!("{price:.2}"), "q": format!("{qty:.5}"),
                "T": ts, "m": !buy
            }),
            // Taker side spelled out, ISO-8601 time.
            Exchange::Coinbase => json!({
                "type": "match", "product_id": "BTC-USD", "trade_id": self.seq.to_string(),
                "price": format!("{price:.2}"), "size": format!("{qty:.8}"),
                "side": if buy { "buy" } else { "sell" }, "time": iso
            }),
            // JSON numbers, no side field.
            Exchange::Alpaca => json!({
                "T": "t", "S": "BTC/USD", "i": self.seq,
                "p": price, "s": (qty * 10_000.0).round() / 10_000.0, "t": iso
            }),
            // Nanosecond epoch timestamp, string trade id, no side.
            _ => json!({
                "ev": "XT", "pair": "BTC-USD", "i": self.seq.to_string(),
                "p": price, "s": (qty * 10_000.0).round() / 10_000.0, "t": ts * 1_000_000
            }),
        };
        RawTick {
            exchange: venue,
            symbol: SYMBOL.to_string(),
            payload,
            received_at_ms: ts + latency,
        }
    }
}

fn rfc3339(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}
