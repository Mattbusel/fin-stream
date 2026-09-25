//! Replay a recorded tick file through the same code path a live feed uses.
//!
//! `TickReplayer` implements `TickSource`, the trait live feeds implement, so
//! it streams `NormalizedTick`s into a Tokio channel with the original
//! inter-tick gaps scaled by a speed multiplier. The consumer here rolls them
//! into 30-second bars and draws each bar as it closes, on a fixed price axis.
//!
//! ```text
//! cargo run --example replay            # 10 minutes of ticks at 120x, a few seconds
//! cargo run --example replay -- 0       # as fast as possible
//! cargo run --example replay -- --record  # regenerate the data file
//! ```
//!
//! The data file `examples/data/btc-usd-10m.ndjson` holds 600 one-second ticks
//! produced by the crate's own jump-diffusion generator (seed 11), so it is
//! synthetic but real NDJSON in the exact format a recorder would write.

use fin_stream::ohlcv::{OhlcvAggregator, OhlcvBar, Timeframe};
use fin_stream::replay::{ReplaySession, TickReplayer, TickSource};
use fin_stream::synthetic::{JumpDiffusion, SyntheticMarketGenerator};
use fin_stream::tick::{Exchange, NormalizedTick};
use fin_stream::StreamError;
use rust_decimal::Decimal;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

mod support;
use support::{clock, f, fixed, live, money, Paint, SESSION_START_MS};

const WIDTH: usize = 46;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), StreamError> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/data/btc-usd-10m.ndjson");
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("--record") {
        return record(&path);
    }
    let speed: f64 = match arg {
        Some(s) => {
            match s.parse::<f64>() {
                Ok(v) if v >= 0.0 => v,
                _ => {
                    eprintln!("usage: cargo run --example replay -- [SPEED | --record]");
                    eprintln!("  SPEED  playback multiplier, e.g. 120 (default in a terminal), 0 = no delay");
                    eprintln!("got '{s}'");
                    std::process::exit(2);
                }
            }
        }
        None if live() => 120.0,
        None => 0.0,
    };
    let paint = Paint::detect();

    let mut replayer = TickReplayer::with_session(ReplaySession::new(&path).with_speed(speed));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<NormalizedTick>(1_024);
    let source = tokio::spawn(async move {
        let result = replayer.run(tx).await;
        (replayer, result)
    });

    println!();
    println!(
        "  {}  {}",
        paint.bold("TickReplayer"),
        paint.dim(format!(
            "examples/data/btc-usd-10m.ndjson at {}, 30s bars",
            if speed > 0.0 {
                format!("{speed}x")
            } else {
                "full speed".into()
            }
        ))
    );
    println!();

    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(30))?;
    let mut axis: Option<(f64, f64)> = None;
    let mut bars = 0usize;
    let mut first: Option<Decimal> = None;
    let started = Instant::now();

    while let Some(tick) = rx.recv().await {
        // Fix the axis around the first print: +/- 0.4%.
        let (lo, hi) = *axis.get_or_insert_with(|| {
            let p = f(tick.price);
            (p * 0.996, p * 1.004)
        });
        if first.is_none() {
            first = Some(tick.price);
            print_axis(&paint, lo, hi);
        }
        for b in agg.feed(&tick)? {
            print_bar(&paint, &b, lo, hi, first.unwrap_or(b.open));
            bars += 1;
            std::io::stdout().flush().ok();
        }
    }
    if let (Some(b), Some((lo, hi))) = (agg.flush(), axis) {
        print_bar(&paint, &b, lo, hi, first.unwrap_or(b.open));
        bars += 1;
    }

    let (replayer, result) = source.await.map_err(|e| StreamError::Io(e.to_string()))?;
    result?;
    let stats = replayer.stats();
    println!();
    println!(
        "  {}  {}",
        paint.bold(format!("{} ticks", stats.ticks_replayed)),
        paint.dim(format!(
            "{} bars, {} parse errors, {} ms wall clock for 600 s of market time",
            bars,
            stats.parse_errors,
            started.elapsed().as_millis()
        ))
    );
    println!();
    Ok(())
}

fn col(p: f64, lo: f64, hi: f64) -> usize {
    (((p - lo) / (hi - lo)) * (WIDTH - 1) as f64)
        .round()
        .clamp(0.0, (WIDTH - 1) as f64) as usize
}

fn print_axis(paint: &Paint, lo: f64, hi: f64) {
    let l = money(Decimal::from_f64_retain(lo).unwrap_or_default(), 0);
    let m = money(
        Decimal::from_f64_retain((lo + hi) / 2.0).unwrap_or_default(),
        0,
    );
    let h = money(Decimal::from_f64_retain(hi).unwrap_or_default(), 0);
    let gap = WIDTH.saturating_sub(l.len() + m.len() + h.len());
    let left = gap / 2;
    println!(
        "  {:<8}  {}{}{}{}{}   {}",
        paint.dim("bar"),
        paint.dim(&l),
        " ".repeat(left),
        paint.dim(&m),
        " ".repeat(gap - left),
        paint.dim(&h),
        paint.dim("     close   vs open  volume")
    );
    println!(
        "  {:<8}  {}",
        "",
        paint.dim("┬".to_string() + &"─".repeat(WIDTH - 2) + "┬")
    );
}

fn print_bar(paint: &Paint, b: &OhlcvBar, lo: f64, hi: f64, session_open: Decimal) {
    let (l, h) = (col(f(b.low), lo, hi), col(f(b.high), lo, hi));
    let (o, c) = (col(f(b.open), lo, hi), col(f(b.close), lo, hi));
    let (b0, b1) = (o.min(c), o.max(c));
    let up = b.close >= b.open;
    let mut row = String::new();
    for i in 0..WIDTH {
        let cell = if i >= b0 && i <= b1 {
            let s = if up {
                paint.bid("█")
            } else {
                paint.ask("█")
            };
            s.to_string()
        } else if i >= l && i <= h {
            paint.dim("─").to_string()
        } else {
            " ".to_string()
        };
        row.push_str(&cell);
    }
    let pct = (b.close - session_open) / session_open * Decimal::from(100);
    let pct_s = format!(
        "{}{}%",
        if pct >= Decimal::ZERO { "+" } else { "" },
        fixed(pct, 2)
    );
    let pct_s = if pct >= Decimal::ZERO {
        paint.bid(pct_s)
    } else {
        paint.ask(pct_s)
    };
    println!(
        "  {:<8}  {}   {:>10}  {:>7}  {:>6}",
        paint.dim(clock(b.bar_start_ms)),
        row,
        money(b.close, 2),
        pct_s,
        fixed(b.volume, 2)
    );
}

/// Write the data file: 600 ticks, one per second, from the crate's own
/// jump-diffusion generator. Prices rounded to cents as an exchange would.
fn record(path: &PathBuf) -> Result<(), StreamError> {
    let mut model = JumpDiffusion::new(0.0, 1.4, 64_250.0, 100_000.0, -0.0015, 0.001);
    let mut gen = SyntheticMarketGenerator::new(11)
        .with_symbol("BTC-USD")
        .with_exchange(Exchange::Coinbase);
    gen.start_ts_ms = SESSION_START_MS;
    gen.tick_interval_ms = 1_000;
    let mut out = String::from("# 600 synthetic BTC-USD ticks, JumpDiffusion seed 11. Regenerate: cargo run --example replay -- --record\n");
    for mut t in gen.generate_ticks(600, &mut model) {
        t.price = t.price.round_dp(2);
        t.quantity = (t.quantity / Decimal::from(10)).round_dp(4);
        t.exchange_ts_ms = Some(t.received_at_ms);
        t.received_at_ms += 14;
        let line = serde_json::to_string(&t).map_err(|e| StreamError::Io(e.to_string()))?;
        out.push_str(&line);
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| StreamError::Io(e.to_string()))?;
    }
    std::fs::write(path, out).map_err(|e| StreamError::Io(e.to_string()))?;
    println!("wrote {}", path.display());
    Ok(())
}
