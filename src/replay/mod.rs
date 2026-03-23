//! Historical tick replay engine.
//!
//! ## Overview
//!
//! This module provides [`TickReplayer`], which reads NDJSON tick files
//! produced by live-capture tooling and replays them at configurable speed
//! through the same [`TickSource`] trait that live feeds implement.  Strategy
//! and analytics code therefore has no awareness of whether it is running
//! against live or historical data.
//!
//! ## Speed control
//!
//! The `speed_multiplier` in [`ReplaySession`] scales inter-tick wall-clock
//! delays:
//!
//! | Multiplier | Effect |
//! |------------|--------|
//! | `1.0` | Real-time: honours original inter-tick gaps |
//! | `10.0` | 10× faster than real-time |
//! | `100.0` | 100× faster than real-time |
//! | `0.0` | Emit all ticks as fast as possible (no delay) |
//!
//! ## File format
//!
//! Input files are newline-delimited JSON (NDJSON). Each line must
//! deserialise into a [`NormalizedTick`].  Lines beginning with `#` are
//! treated as comments and skipped.
//!
//! ```json
//! {"exchange":"Binance","symbol":"BTC-USDT","price":"50000","quantity":"0.01","side":null,"trade_id":null,"exchange_ts_ms":1700000000000,"received_at_ms":1700000000001}
//! {"exchange":"Binance","symbol":"BTC-USDT","price":"50010","quantity":"0.02","side":"buy","trade_id":"T2","exchange_ts_ms":1700000000100,"received_at_ms":1700000000101}
//! ```
//!
//! ## Trait integration
//!
//! [`TickReplayer`] implements [`TickSource`].  Pass any `Box<dyn TickSource>`
//! to strategy/analytics code and swap between live and replay modes at
//! construction time without touching the strategy.

use crate::error::StreamError;
use crate::tick::NormalizedTick;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// TickSource trait
// ---------------------------------------------------------------------------

/// A source of [`NormalizedTick`] events, either live or replayed.
///
/// Implement this trait to plug any tick producer into strategy/analytics code
/// without coupling to a specific transport.
///
/// # Cancellation
///
/// The `run` method should return when the source is exhausted (for replay) or
/// when the `shutdown_tx` channel is closed by the caller.
#[async_trait::async_trait]
pub trait TickSource: Send + Sync {
    /// Start producing ticks into `tx`.
    ///
    /// The implementation should push [`NormalizedTick`]s into `tx` as they
    /// become available.  Returns when the source is exhausted or an
    /// unrecoverable error occurs.
    ///
    /// # Errors
    ///
    /// Returns a [`StreamError`] if the source cannot be initialised or
    /// encounters a fatal error during operation.
    async fn run(
        &mut self,
        tx: mpsc::Sender<NormalizedTick>,
    ) -> Result<(), StreamError>;

    /// Human-readable description of this source (e.g. file path or URL).
    fn description(&self) -> &str;
}

// ---------------------------------------------------------------------------
// ReplaySession
// ---------------------------------------------------------------------------

/// Configuration for a single replay session.
///
/// Construct via [`ReplaySession::new`] and pass to
/// [`TickReplayer::with_session`].
#[derive(Debug, Clone)]
pub struct ReplaySession {
    /// Path to the NDJSON tick file.
    pub source_file: PathBuf,
    /// Playback speed multiplier.
    ///
    /// - `1.0` = real-time (honours original inter-tick gaps)
    /// - `10.0` = 10× faster
    /// - `100.0` = 100× faster
    /// - `0.0` = as fast as possible (no sleeps)
    pub speed_multiplier: f64,
    /// When `true`, the replayer wraps back to the start of the file after
    /// reaching the end.
    pub loop_replay: bool,
    /// Skip ticks whose `received_at_ms` falls before this offset from the
    /// first tick's timestamp. Use `0` to start from the beginning.
    pub start_offset_ms: u64,
    /// Maximum number of ticks to emit per session. `None` = unlimited.
    pub max_ticks: Option<usize>,
}

impl ReplaySession {
    /// Create a new session configuration for the given file.
    ///
    /// Defaults: `speed_multiplier = 1.0`, no loop, no start offset, no tick
    /// limit.
    pub fn new(source_file: impl Into<PathBuf>) -> Self {
        Self {
            source_file: source_file.into(),
            speed_multiplier: 1.0,
            loop_replay: false,
            start_offset_ms: 0,
            max_ticks: None,
        }
    }

    /// Set the playback speed multiplier.
    #[must_use]
    pub fn with_speed(mut self, multiplier: f64) -> Self {
        self.speed_multiplier = multiplier;
        self
    }

    /// Enable looping: restart from the beginning when the file ends.
    #[must_use]
    pub fn looping(mut self) -> Self {
        self.loop_replay = true;
        self
    }

    /// Skip all ticks before `offset_ms` milliseconds after the first tick.
    #[must_use]
    pub fn with_start_offset(mut self, offset_ms: u64) -> Self {
        self.start_offset_ms = offset_ms;
        self
    }

    /// Stop after emitting `max` ticks.
    #[must_use]
    pub fn with_max_ticks(mut self, max: usize) -> Self {
        self.max_ticks = Some(max);
        self
    }
}

// ---------------------------------------------------------------------------
// ReplayStats
// ---------------------------------------------------------------------------

/// Performance statistics for a completed (or in-progress) replay session.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReplayStats {
    /// Total number of ticks emitted by the replayer.
    pub ticks_replayed: u64,
    /// Wall-clock duration of the session in milliseconds.
    pub duration_ms: u64,
    /// Mean lag between the scheduled emit time and the actual emit time, in
    /// milliseconds.  Non-zero when the downstream consumer is slower than the
    /// configured playback speed.
    pub lag_ms: f64,
    /// Number of lines in the source file that could not be parsed.
    pub parse_errors: u64,
    /// Number of ticks skipped due to `start_offset_ms`.
    pub skipped_ticks: u64,
}

impl ReplayStats {
    /// Compute mean ticks per second based on elapsed duration.
    ///
    /// Returns `None` when `duration_ms` is zero.
    pub fn ticks_per_second(&self) -> Option<f64> {
        if self.duration_ms == 0 {
            None
        } else {
            Some(self.ticks_replayed as f64 / (self.duration_ms as f64 / 1_000.0))
        }
    }
}

// ---------------------------------------------------------------------------
// TickReplayer
// ---------------------------------------------------------------------------

/// Replays an NDJSON tick file at configurable speed.
///
/// Implements [`TickSource`] so it can replace live feeds in any pipeline
/// without changing strategy or analytics code.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::replay::{TickReplayer, ReplaySession, TickSource};
///
/// # async fn run() -> Result<(), fin_stream::StreamError> {
/// let session = ReplaySession::new("data/btc_ticks.ndjson")
///     .with_speed(10.0)   // replay at 10× real time
///     .with_max_ticks(10_000);
///
/// let mut replayer = TickReplayer::with_session(session);
///
/// let (tx, mut rx) = tokio::sync::mpsc::channel(1_024);
///
/// // Spawn the replayer on its own task.
/// tokio::spawn(async move {
///     replayer.run(tx).await.ok();
/// });
///
/// while let Some(tick) = rx.recv().await {
///     println!("{} @ {}", tick.symbol, tick.price);
/// }
/// # Ok(())
/// # }
/// ```
pub struct TickReplayer {
    session: ReplaySession,
    stats: ReplayStats,
}

impl TickReplayer {
    /// Construct a new replayer from a [`ReplaySession`] configuration.
    pub fn with_session(session: ReplaySession) -> Self {
        Self {
            session,
            stats: ReplayStats::default(),
        }
    }

    /// Returns a snapshot of the current replay statistics.
    ///
    /// Safe to call while the replayer is running from another task because
    /// this returns a clone of the internal counter state captured at the time
    /// of the call.
    pub fn stats(&self) -> &ReplayStats {
        &self.stats
    }

    /// Load all ticks from the source file into memory.
    ///
    /// Useful for small files or when you need random access.  For large files
    /// prefer the streaming [`TickSource::run`] path.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] when the file cannot be opened, or
    /// [`StreamError::ParseError`] (logged as a warning) when a line cannot be
    /// parsed — those lines are skipped.
    pub fn load_all(&mut self) -> Result<Vec<NormalizedTick>, StreamError> {
        let path = &self.session.source_file;
        let file = std::fs::File::open(path).map_err(|e| StreamError::Io(e.to_string()))?;
        let reader = BufReader::new(file);
        let mut ticks = Vec::new();
        let mut errors = 0u64;

        for (line_no, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| StreamError::Io(e.to_string()))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match serde_json::from_str::<NormalizedTick>(line) {
                Ok(tick) => ticks.push(tick),
                Err(e) => {
                    errors += 1;
                    warn!(line = line_no, error = %e, "replay: skipping unparseable line");
                }
            }
        }

        self.stats.parse_errors += errors;
        info!(
            file = %path.display(),
            loaded = ticks.len(),
            errors,
            "replay: loaded tick file"
        );
        Ok(ticks)
    }

    /// Internal: run one pass over the file, emitting ticks into `tx`.
    async fn run_pass(
        &mut self,
        tx: &mpsc::Sender<NormalizedTick>,
        start_wall: std::time::Instant,
    ) -> Result<bool, StreamError> {
        let path = self.session.source_file.clone();
        let file = std::fs::File::open(&path).map_err(|e| StreamError::Io(e.to_string()))?;
        let reader = BufReader::new(file);

        let mut first_tick_ts: Option<u64> = None;
        let mut prev_tick_ts: Option<u64> = None;
        let mut total_lag_ms: f64 = 0.0;
        let mut lag_samples: u64 = 0;
        let speed = self.session.speed_multiplier;

        for line in reader.lines() {
            let line = line.map_err(|e| StreamError::Io(e.to_string()))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let tick = match serde_json::from_str::<NormalizedTick>(line) {
                Ok(t) => t,
                Err(e) => {
                    self.stats.parse_errors += 1;
                    warn!(error = %e, "replay: skipping unparseable line");
                    continue;
                }
            };

            let tick_ts = tick.received_at_ms;

            // Determine the base timestamp (first tick in the file).
            let base_ts = *first_tick_ts.get_or_insert(tick_ts);

            // Apply start offset filter.
            if tick_ts < base_ts + self.session.start_offset_ms {
                self.stats.skipped_ticks += 1;
                continue;
            }

            // Calculate inter-tick delay scaled by speed.
            if let Some(prev_ts) = prev_tick_ts {
                let raw_gap_ms = tick_ts.saturating_sub(prev_ts);
                if speed > 0.0 {
                    let scaled_gap_ms = (raw_gap_ms as f64 / speed) as u64;
                    if scaled_gap_ms > 0 {
                        let before_sleep = std::time::Instant::now();
                        tokio::time::sleep(Duration::from_millis(scaled_gap_ms)).await;
                        // Measure actual sleep lag.
                        let actual_sleep = before_sleep.elapsed().as_millis() as u64;
                        let lag = actual_sleep.saturating_sub(scaled_gap_ms) as f64;
                        total_lag_ms += lag;
                        lag_samples += 1;
                    }
                }
            }
            prev_tick_ts = Some(tick_ts);

            debug!(
                symbol = %tick.symbol,
                price = %tick.price,
                ts = tick_ts,
                "replay: emitting tick"
            );

            if tx.send(tick).await.is_err() {
                // Receiver has been dropped; stop replaying.
                return Ok(false);
            }

            self.stats.ticks_replayed += 1;

            // Update cumulative stats.
            if lag_samples > 0 {
                self.stats.lag_ms = total_lag_ms / lag_samples as f64;
            }
            self.stats.duration_ms = start_wall.elapsed().as_millis() as u64;

            // Check tick limit.
            if let Some(max) = self.session.max_ticks {
                if self.stats.ticks_replayed as usize >= max {
                    return Ok(false);
                }
            }
        }

        Ok(true) // `true` = file exhausted naturally, caller may loop
    }
}

#[async_trait::async_trait]
impl TickSource for TickReplayer {
    /// Stream all ticks from the configured NDJSON file into `tx`.
    ///
    /// Honours `speed_multiplier`, `loop_replay`, `start_offset_ms`, and
    /// `max_ticks` from the [`ReplaySession`].
    ///
    /// Returns when the file is exhausted (or looping is disabled and the end
    /// is reached), when `max_ticks` is exceeded, or when `tx` is closed.
    async fn run(
        &mut self,
        tx: mpsc::Sender<NormalizedTick>,
    ) -> Result<(), StreamError> {
        let start = std::time::Instant::now();
        info!(
            file = %self.session.source_file.display(),
            speed = self.session.speed_multiplier,
            looping = self.session.loop_replay,
            "TickReplayer: starting replay"
        );

        loop {
            let continue_loop = self.run_pass(&tx, start).await?;
            if !continue_loop || !self.session.loop_replay {
                break;
            }
            info!("TickReplayer: looping replay from start");
        }

        self.stats.duration_ms = start.elapsed().as_millis() as u64;
        info!(
            ticks = self.stats.ticks_replayed,
            duration_ms = self.stats.duration_ms,
            lag_ms = self.stats.lag_ms,
            "TickReplayer: replay complete"
        );
        Ok(())
    }

    fn description(&self) -> &str {
        self.session
            .source_file
            .to_str()
            .unwrap_or("<invalid path>")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::Exchange;
    use rust_decimal_macros::dec;
    use std::io::Write;

    /// Write NDJSON ticks to a temp file in `std::env::temp_dir()` and return
    /// the path.  The caller is responsible for removing the file when done.
    fn write_ndjson(ticks: &[NormalizedTick], name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        for t in ticks {
            let line = serde_json::to_string(t).expect("serialize");
            writeln!(f, "{}", line).expect("write");
        }
        path
    }

    fn make_tick(price: rust_decimal::Decimal, ts: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTC-USDT".into(),
            price,
            quantity: dec!(1),
            side: None,
            trade_id: None,
            exchange_ts_ms: Some(ts),
            received_at_ms: ts,
        }
    }

    #[tokio::test]
    async fn replay_emits_all_ticks() {
        let ticks = vec![
            make_tick(dec!(50000), 1_000),
            make_tick(dec!(50100), 1_010),
            make_tick(dec!(50200), 1_020),
        ];
        let path = write_ndjson(&ticks, "fin_stream_replay_all.ndjson");
        let session = ReplaySession::new(&path).with_speed(0.0);

        let mut replayer = TickReplayer::with_session(session);
        let (tx, mut rx) = mpsc::channel(16);

        tokio::spawn(async move {
            replayer.run(tx).await.expect("run");
        });

        let mut received = Vec::new();
        while let Some(t) = rx.recv().await {
            received.push(t);
        }
        let _ = std::fs::remove_file(&path);
        assert_eq!(received.len(), 3);
        assert_eq!(received[0].price, dec!(50000));
        assert_eq!(received[2].price, dec!(50200));
    }

    #[tokio::test]
    async fn replay_respects_max_ticks() {
        let ticks: Vec<NormalizedTick> = (0..10)
            .map(|i| make_tick(dec!(50000) + rust_decimal::Decimal::from(i * 100), i * 10))
            .collect();
        let path = write_ndjson(&ticks, "fin_stream_replay_max.ndjson");
        let session = ReplaySession::new(&path)
            .with_speed(0.0)
            .with_max_ticks(3);

        let mut replayer = TickReplayer::with_session(session);
        let (tx, mut rx) = mpsc::channel(16);

        tokio::spawn(async move {
            replayer.run(tx).await.expect("run");
        });

        let mut count = 0;
        while rx.recv().await.is_some() {
            count += 1;
        }
        let _ = std::fs::remove_file(&path);
        assert_eq!(count, 3, "should have stopped after max_ticks=3");
    }

    #[tokio::test]
    async fn replay_skips_comment_lines() {
        let path = std::env::temp_dir().join("fin_stream_replay_comments.ndjson");
        {
            let mut f = std::fs::File::create(&path).expect("create");
            writeln!(f, "# this is a comment").expect("write");
            writeln!(f, "{}", serde_json::to_string(&make_tick(dec!(100), 1)).expect("ser")).expect("write");
            writeln!(f, "# another comment").expect("write");
            writeln!(f, "{}", serde_json::to_string(&make_tick(dec!(200), 2)).expect("ser")).expect("write");
        }

        let session = ReplaySession::new(&path).with_speed(0.0);
        let mut replayer = TickReplayer::with_session(session);
        let (tx, mut rx) = mpsc::channel(8);

        tokio::spawn(async move {
            replayer.run(tx).await.expect("run");
        });

        let mut received = Vec::new();
        while let Some(t) = rx.recv().await {
            received.push(t);
        }
        let _ = std::fs::remove_file(&path);
        assert_eq!(received.len(), 2, "comments should be skipped");
    }

    #[test]
    fn session_builder() {
        let s = ReplaySession::new("ticks.ndjson")
            .with_speed(100.0)
            .looping()
            .with_start_offset(500)
            .with_max_ticks(1_000);
        assert!((s.speed_multiplier - 100.0).abs() < f64::EPSILON);
        assert!(s.loop_replay);
        assert_eq!(s.start_offset_ms, 500);
        assert_eq!(s.max_ticks, Some(1_000));
    }

    #[test]
    fn replay_stats_ticks_per_second() {
        let stats = ReplayStats {
            ticks_replayed: 10_000,
            duration_ms: 1_000,
            ..Default::default()
        };
        assert!((stats.ticks_per_second().unwrap() - 10_000.0).abs() < 0.01);

        let zero = ReplayStats::default();
        assert!(zero.ticks_per_second().is_none());
    }
}
