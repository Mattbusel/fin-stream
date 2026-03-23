//! Snapshot-and-replay — binary tick recording and N-speed replay.
//!
//! ## Responsibility
//! [`TickRecorder`] serialises [`NormalizedTick`] values to a compact binary
//! file. [`TickReplayer`] reads that file back and emits ticks at the original
//! timing or at a configurable speed multiplier — enabling backtesting with
//! real captured tick data.
//!
//! ## Wire format
//!
//! Each record is length-prefixed:
//!
//! ```text
//! [4 bytes LE u32 — payload length] [payload bytes — serde_json encoded tick]
//! ```
//!
//! JSON is used for portability and human-inspectability. For maximum
//! throughput the recorder buffers writes via [`BufWriter`].
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result<_, StreamError>`.
//! - Flushed on drop: [`TickRecorder::flush`] must be called explicitly before
//!   drop; the `Drop` impl attempts a best-effort flush and logs warnings.

use crate::error::StreamError;
use crate::tick::NormalizedTick;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Records ticks to a binary file in length-prefixed JSON format.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::snapshot::TickRecorder;
///
/// # async fn example() -> Result<(), fin_stream::StreamError> {
/// let mut recorder = TickRecorder::open("/tmp/ticks.bin")?;
/// // recorder.record(&tick)?;
/// recorder.flush()?;
/// # Ok(())
/// # }
/// ```
pub struct TickRecorder {
    writer: BufWriter<std::fs::File>,
    path: String,
    ticks_written: u64,
    bytes_written: u64,
}

impl TickRecorder {
    /// Open (or create) the file at `path` for writing.
    ///
    /// Any existing content is overwritten (truncated).
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] if the file cannot be created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StreamError> {
        let path_str = path.as_ref().to_string_lossy().into_owned();
        let file = std::fs::File::create(path.as_ref()).map_err(|e| StreamError::Io(e.to_string()))?;
        info!(path = %path_str, "tick recorder opened");
        Ok(Self {
            writer: BufWriter::new(file),
            path: path_str,
            ticks_written: 0,
            bytes_written: 0,
        })
    }

    /// Append (rather than truncate) to an existing file.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] if the file cannot be opened.
    pub fn open_append(path: impl AsRef<Path>) -> Result<Self, StreamError> {
        let path_str = path.as_ref().to_string_lossy().into_owned();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())
            .map_err(|e| StreamError::Io(e.to_string()))?;
        info!(path = %path_str, "tick recorder opened (append)");
        Ok(Self {
            writer: BufWriter::new(file),
            path: path_str,
            ticks_written: 0,
            bytes_written: 0,
        })
    }

    /// Write one tick to the file.
    ///
    /// ## Wire format
    ///
    /// ```text
    /// [u32 LE length][json bytes]
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] on any I/O failure.
    pub fn record(&mut self, tick: &NormalizedTick) -> Result<(), StreamError> {
        let payload =
            serde_json::to_vec(tick).map_err(|e| StreamError::Io(e.to_string()))?;
        let len = payload.len() as u32;
        self.writer
            .write_all(&len.to_le_bytes())
            .map_err(|e| StreamError::Io(e.to_string()))?;
        self.writer
            .write_all(&payload)
            .map_err(|e| StreamError::Io(e.to_string()))?;
        self.ticks_written += 1;
        self.bytes_written += 4 + payload.len() as u64;
        debug!(
            path = %self.path,
            ticks_written = self.ticks_written,
            "tick recorded"
        );
        Ok(())
    }

    /// Flush the internal buffer to disk.
    ///
    /// Call this before dropping the recorder to ensure all ticks are
    /// persisted.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] on flush failure.
    pub fn flush(&mut self) -> Result<(), StreamError> {
        self.writer.flush().map_err(|e| StreamError::Io(e.to_string()))
    }

    /// Total number of ticks written in this session.
    pub fn ticks_written(&self) -> u64 {
        self.ticks_written
    }

    /// Total bytes written (header + payload) in this session.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// File path this recorder is writing to.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for TickRecorder {
    fn drop(&mut self) {
        if let Err(e) = self.writer.flush() {
            warn!(path = %self.path, error = %e, "TickRecorder flush failed on drop");
        }
    }
}

/// Reads a binary tick file written by [`TickRecorder`] and replays ticks.
///
/// Ticks can be replayed:
/// - At original timing (`speed_multiplier = 1.0`)
/// - Faster than real-time (`speed_multiplier > 1.0`, e.g. `10.0` for 10× speed)
/// - Slower than real-time (`speed_multiplier < 1.0`)
/// - As fast as possible (`speed_multiplier = 0.0` — no inter-tick delay)
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::snapshot::TickReplayer;
///
/// # async fn example() -> Result<(), fin_stream::StreamError> {
/// let replayer = TickReplayer::open("/tmp/ticks.bin", 1.0)?;
/// let (handle, mut tick_rx) = replayer.start(64);
/// while let Some(tick) = tick_rx.recv().await {
///     // process tick
///     let _ = tick;
/// }
/// # Ok(())
/// # }
/// ```
pub struct TickReplayer {
    ticks: Vec<NormalizedTick>,
    speed_multiplier: f64,
}

impl TickReplayer {
    /// Load the tick file at `path` entirely into memory.
    ///
    /// `speed_multiplier`:
    /// - `1.0` → real-time replay
    /// - `10.0` → 10× faster than real-time
    /// - `0.0` → no delay (as fast as possible)
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Io`] if the file cannot be read, or
    /// [`StreamError::ParseError`] if any record is malformed.
    pub fn open(path: impl AsRef<Path>, speed_multiplier: f64) -> Result<Self, StreamError> {
        let path_ref = path.as_ref();
        let file =
            std::fs::File::open(path_ref).map_err(|e| StreamError::Io(e.to_string()))?;
        let mut reader = BufReader::new(file);
        let mut ticks = Vec::new();
        let mut len_buf = [0u8; 4];

        loop {
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(StreamError::Io(e.to_string())),
            }
            let payload_len = u32::from_le_bytes(len_buf) as usize;
            let mut payload = vec![0u8; payload_len];
            reader
                .read_exact(&mut payload)
                .map_err(|e| StreamError::Io(e.to_string()))?;
            let tick: NormalizedTick = serde_json::from_slice(&payload).map_err(|e| {
                StreamError::ParseError {
                    exchange: "TickReplayer".into(),
                    reason: e.to_string(),
                }
            })?;
            ticks.push(tick);
        }

        info!(
            path = %path_ref.to_string_lossy(),
            ticks = ticks.len(),
            speed = speed_multiplier,
            "tick file loaded for replay"
        );

        Ok(Self {
            ticks,
            speed_multiplier,
        })
    }

    /// Total number of ticks loaded from the file.
    pub fn tick_count(&self) -> usize {
        self.ticks.len()
    }

    /// Configured speed multiplier.
    pub fn speed_multiplier(&self) -> f64 {
        self.speed_multiplier
    }

    /// Iterate ticks without timing (synchronous, zero-delay access).
    pub fn iter(&self) -> impl Iterator<Item = &NormalizedTick> {
        self.ticks.iter()
    }

    /// Start the async replay, emitting ticks on the returned channel.
    ///
    /// Returns `(join_handle, tick_rx)`. The task drives inter-tick delays
    /// according to `speed_multiplier`. When all ticks are exhausted the sender
    /// is dropped and `tick_rx` returns `None`.
    ///
    /// `channel_capacity` is the mpsc buffer depth.
    pub fn start(self, channel_capacity: usize) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<NormalizedTick>) {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let handle = tokio::spawn(async move {
            replay_task(self.ticks, self.speed_multiplier, tx).await;
        });
        (handle, rx)
    }
}

async fn replay_task(
    ticks: Vec<NormalizedTick>,
    speed_multiplier: f64,
    tx: mpsc::Sender<NormalizedTick>,
) {
    if ticks.is_empty() {
        return;
    }

    let mut prev_ts: Option<u64> = None;

    for tick in ticks {
        // Compute inter-tick delay relative to previous tick's timestamp.
        if speed_multiplier > 0.0 {
            if let Some(prev) = prev_ts {
                let delta_ms = tick.received_at_ms.saturating_sub(prev);
                if delta_ms > 0 {
                    let scaled_ms = (delta_ms as f64 / speed_multiplier) as u64;
                    if scaled_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(scaled_ms)).await;
                    }
                }
            }
        }

        prev_ts = Some(tick.received_at_ms);

        if tx.send(tick).await.is_err() {
            // Receiver dropped — stop replaying.
            debug!("replay receiver dropped — stopping");
            return;
        }
    }

    info!("tick replay complete");
}
