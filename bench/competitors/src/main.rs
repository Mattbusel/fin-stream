//! Accuracy check: the 2.11.3 histogram and the hdrhistogram-backed one against the
//! exact percentiles of the same latency-shaped sample.

use competitors::OldLatencyHistogram;
use fin_stream::latency::LatencyHistogram;

fn main() {
    let mut samples = competitors::samples();
    let (mut old, mut new) = (OldLatencyHistogram::new(), LatencyHistogram::new());
    for &s in &samples {
        old.record(s);
        new.record(s);
    }
    samples.sort_unstable();
    println!("{:>6} {:>10} {:>10} {:>9} {:>10} {:>9}", "pct", "exact us", "2.11.3", "error", "2.12", "error");
    for p in [50.0, 90.0, 99.0, 99.9, 99.99] {
        let idx = ((p / 100.0) * samples.len() as f64).ceil() as usize - 1;
        let exact = samples[idx] as f64;
        let (o, n) = (old.percentile(p) as f64, new.percentile(p) as f64);
        println!(
            "{p:>6} {exact:>10} {o:>10} {:>8.2}% {n:>10} {:>8.2}%",
            (o - exact) / exact * 100.0,
            (n - exact) / exact * 100.0
        );
    }
}
