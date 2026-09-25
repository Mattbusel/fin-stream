//! Watch four feeds with `HealthMonitor` and see a stall trip the circuit.
//!
//! Each feed sends heartbeats on its own schedule. Every 500 ms of simulated
//! time the monitor runs `check_all`; a feed that has been quiet longer than
//! its threshold is marked stale, and after three consecutive stale checks
//! its circuit opens. One heartbeat closes it again.
//!
//! ```text
//! cargo run --example feed_health
//! ```
//!
//! The clock is simulated, so the run is deterministic and needs no network.
//! In a terminal the strip chart redraws live; piped, it prints the final frame.

use fin_stream::health::{HealthMonitor, HealthStatus};
use fin_stream::StreamError;
use std::io::Write;
use std::time::Duration;

mod support;
use support::{live, Paint, SESSION_START_MS};

const STEP_MS: u64 = 500;
const STEPS: u64 = 48; // 24 seconds

/// (feed id, heartbeat period in ms, custom stale threshold, outage windows in ms)
const FEEDS: [(&str, u64, Option<u64>, &[(u64, u64)]); 4] = [
    ("binance", 250, None, &[]),
    ("coinbase", 500, None, &[(5_000, 12_500)]),
    ("alpaca", 750, None, &[(16_000, u64::MAX)]),
    // A slow feed with a longer threshold of its own: never stale.
    ("polygon", 3_000, Some(5_000), &[]),
];

#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Beat,
    Quiet,
    Stale,
    Open,
}

fn main() -> Result<(), StreamError> {
    let paint = Paint::detect();
    let animate = live() && paint.enabled();

    let monitor = HealthMonitor::new(2_000).with_circuit_breaker_threshold(3);
    for (id, _, threshold, _) in FEEDS {
        monitor.register(id, threshold);
    }

    let mut cells: Vec<Vec<Cell>> = vec![Vec::new(); FEEDS.len()];
    let mut events: Vec<(u64, String, String)> = Vec::new();
    let mut was_open = [false; 4];

    println!();
    println!(
        "  {}  {}",
        paint.bold("HealthMonitor"),
        paint.dim("stale after 2 s quiet (polygon: 5 s), circuit opens after 3 stale checks")
    );
    println!();

    for step in 1..=STEPS {
        let t0 = (step - 1) * STEP_MS;
        let t1 = step * STEP_MS;
        let now = SESSION_START_MS + t1;

        // Deliver every heartbeat due in (t0, t1].
        let mut beat = [false; 4];
        for (i, (id, period, _, outages)) in FEEDS.iter().enumerate() {
            let mut t = (t0 / period + 1) * period;
            while t <= t1 {
                let down = outages.iter().any(|&(a, b)| t >= a && t < b);
                if !down {
                    monitor.heartbeat(id, SESSION_START_MS + t)?;
                    beat[i] = true;
                }
                t += period;
            }
        }

        // The periodic health sweep.
        for (feed, err) in monitor.check_all(now) {
            let i = FEEDS.iter().position(|f| f.0 == feed).unwrap_or(0);
            let consecutive = monitor.get(&feed).map_or(0, |h| h.consecutive_stale);
            if consecutive == 1 {
                events.push((t1, feed.clone(), err.to_string()));
            }
            if monitor.is_circuit_open(&feed) && !was_open[i] {
                was_open[i] = true;
                events.push((
                    t1,
                    feed.clone(),
                    format!("circuit OPEN after {consecutive} stale checks"),
                ));
            }
        }

        for (i, (id, ..)) in FEEDS.iter().enumerate() {
            let h = monitor.get(id);
            let open = monitor.is_circuit_open(id);
            if was_open[i] && !open {
                was_open[i] = false;
                events.push((
                    t1,
                    id.to_string(),
                    "heartbeat received, circuit closed".into(),
                ));
            }
            let cell = if open {
                Cell::Open
            } else if h
                .as_ref()
                .map_or(false, |h| h.status == HealthStatus::Stale)
            {
                Cell::Stale
            } else if beat[i] {
                Cell::Beat
            } else {
                Cell::Quiet
            };
            cells[i].push(cell);
        }

        if animate {
            if step > 1 {
                print!("\x1b[{}A", FEEDS.len() + 1);
            }
            draw(&paint, &monitor, &cells, now);
            std::io::stdout().flush().ok();
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    if !animate {
        draw(&paint, &monitor, &cells, SESSION_START_MS + STEPS * STEP_MS);
    }

    println!();
    println!(
        "  {} heartbeat   {} quiet   {} stale   {} circuit open",
        paint.bid("│"),
        paint.dim("·"),
        paint.brass("▒"),
        paint.ask("█")
    );
    println!();
    for (t, feed, msg) in &events {
        let msg = if msg.starts_with("circuit OPEN") {
            paint.ask(msg.as_str())
        } else if msg.starts_with("heartbeat") {
            paint.bid(msg.as_str())
        } else {
            paint.brass(msg.as_str())
        };
        println!(
            "  {}  {:<9} {}",
            paint.dim(format!("t+{:>4.1}s", *t as f64 / 1000.0)),
            feed,
            msg
        );
    }
    let (healthy, stale, unknown) = monitor.status_summary();
    println!();
    println!(
        "  {}  {}",
        paint.bold("end state"),
        paint.dim(format!(
            "{healthy} healthy, {stale} stale, {unknown} unknown; ratio_healthy {:.2}",
            monitor.ratio_healthy()
        ))
    );
    println!();
    Ok(())
}

fn draw(paint: &Paint, monitor: &HealthMonitor, cells: &[Vec<Cell>], now: u64) {
    for (i, (id, ..)) in FEEDS.iter().enumerate() {
        let mut strip = String::new();
        for c in &cells[i] {
            let s = match c {
                Cell::Beat => paint.bid("│"),
                Cell::Quiet => paint.dim("·"),
                Cell::Stale => paint.brass("▒"),
                Cell::Open => paint.ask("█"),
            };
            strip.push_str(&s.to_string());
        }
        strip.push_str(&" ".repeat(STEPS as usize - cells[i].len()));
        let h = monitor.get(id);
        let age = h
            .as_ref()
            .and_then(|h| h.elapsed_ms(now))
            .map_or("-".to_string(), |a| format!("{:.1}s", a as f64 / 1000.0));
        let status = match (monitor.is_circuit_open(id), h.as_ref().map(|h| h.status)) {
            (true, _) => paint.ask("open"),
            (_, Some(HealthStatus::Stale)) => paint.brass("stale"),
            (_, Some(HealthStatus::Healthy)) => paint.bid("healthy"),
            _ => paint.dim("unknown"),
        };
        let ticks = h.as_ref().map_or(0, |h| h.tick_count);
        println!(
            "  {:<9} {}  {:<7}  {}{}",
            id,
            strip,
            status,
            paint.dim(format!("{ticks:>3} beats, last {age:>5} ago")),
            if paint.enabled() { "[K" } else { "" }
        );
    }
    let mut labels = String::new();
    let mut s = 0;
    while s < STEPS as usize {
        let l = format!("{}s", s as u64 * STEP_MS / 1000);
        labels.push_str(&format!("{l:<10}"));
        s += 10;
    }
    println!("  {:<9} {}", "", paint.dim(labels.trim_end()));
}
