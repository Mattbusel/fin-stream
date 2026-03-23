//! Tick anomaly detection — price spikes, volume spikes, sequence gaps, and
//! timestamp inversions.
//!
//! ## Responsibility
//! Run each incoming [`NormalizedTick`] through a set of configurable anomaly
//! detectors. When an anomaly is found, emit an [`AnomalyEvent`] to a separate
//! channel alongside the normal tick stream. Normal ticks always flow through
//! even when flagged.
//!
//! ## Detectors
//!
//! | Detector | Trigger |
//! |---|---|
//! | Price spike | `|price - rolling_mean| > N × rolling_std` |
//! | Volume spike | `quantity > base_volume × threshold_multiplier` |
//! | Sequence gap | `tick.trade_id` sequence increments by more than 1 |
//! | Timestamp inversion | `tick.received_at_ms < last_received_at_ms` |
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or emit events.
//! - Thread-safe: detector state managed inside a single-owner async pipeline.

use crate::tick::NormalizedTick;
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// The kind of anomaly that was detected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AnomalyKind {
    /// Price deviated from the rolling mean by more than N standard deviations.
    PriceSpike {
        /// Deviation from the rolling mean in units of standard deviation.
        z_score: String, // stored as string to avoid f64 in the public API
    },
    /// Trade quantity exceeded the rolling mean by more than the configured
    /// multiplier.
    VolumeSpike {
        /// Ratio of current quantity to rolling mean quantity.
        ratio: String,
    },
    /// One or more sequence numbers were skipped (gap in trade IDs).
    SequenceGap {
        /// Last observed sequence number (parsed from trade_id).
        last_seq: u64,
        /// Current (received) sequence number.
        current_seq: u64,
    },
    /// A tick arrived with a timestamp earlier than the previous tick's
    /// timestamp, indicating out-of-order delivery.
    TimestampInversion {
        /// Timestamp of the previous tick (ms since Unix epoch).
        previous_ms: u64,
        /// Timestamp of the current tick (ms since Unix epoch).
        current_ms: u64,
    },
}

/// An anomaly event emitted alongside the normal tick stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnomalyEvent {
    /// The tick that triggered the anomaly.
    pub tick: NormalizedTick,
    /// Classification of the anomaly.
    pub kind: AnomalyKind,
    /// Wall-clock timestamp (ms since Unix epoch) when the anomaly was detected.
    pub detected_at_ms: u64,
}

impl AnomalyEvent {
    /// Returns `true` if this is a price-spike anomaly.
    pub fn is_price_spike(&self) -> bool {
        matches!(self.kind, AnomalyKind::PriceSpike { .. })
    }

    /// Returns `true` if this is a volume-spike anomaly.
    pub fn is_volume_spike(&self) -> bool {
        matches!(self.kind, AnomalyKind::VolumeSpike { .. })
    }

    /// Returns `true` if this is a sequence-gap anomaly.
    pub fn is_sequence_gap(&self) -> bool {
        matches!(self.kind, AnomalyKind::SequenceGap { .. })
    }

    /// Returns `true` if this is a timestamp-inversion anomaly.
    pub fn is_timestamp_inversion(&self) -> bool {
        matches!(self.kind, AnomalyKind::TimestampInversion { .. })
    }
}

/// Configuration for [`TickAnomalyDetector`].
#[derive(Debug, Clone)]
pub struct AnomalyDetectorConfig {
    /// Number of ticks in the rolling window used for mean/std calculations.
    /// Must be >= 2.
    pub window_size: usize,
    /// Z-score threshold for price spikes. A tick is flagged when
    /// `|price - mean| / std > price_spike_z`.
    pub price_spike_z: f64,
    /// Volume spike multiplier. A tick is flagged when
    /// `quantity > mean_quantity * volume_spike_multiplier`.
    pub volume_spike_multiplier: f64,
    /// Whether sequence gap detection is enabled. Requires `trade_id` to be
    /// a parseable integer sequence.
    pub detect_sequence_gaps: bool,
    /// Whether timestamp inversion detection is enabled.
    pub detect_timestamp_inversions: bool,
}

impl AnomalyDetectorConfig {
    /// Create a config with sensible defaults.
    ///
    /// - `window_size`: 50 ticks
    /// - `price_spike_z`: 3.0 standard deviations
    /// - `volume_spike_multiplier`: 5.0×
    /// - sequence gap detection: enabled
    /// - timestamp inversion detection: enabled
    pub fn default_config() -> Self {
        Self {
            window_size: 50,
            price_spike_z: 3.0,
            volume_spike_multiplier: 5.0,
            detect_sequence_gaps: true,
            detect_timestamp_inversions: true,
        }
    }

    /// Override window size.
    ///
    /// Panics if `size < 2` (misuse guard, consistent with other constructors).
    pub fn with_window_size(mut self, size: usize) -> Self {
        assert!(size >= 2, "AnomalyDetectorConfig: window_size must be >= 2");
        self.window_size = size;
        self
    }

    /// Override the price-spike z-score threshold.
    pub fn with_price_spike_z(mut self, z: f64) -> Self {
        self.price_spike_z = z;
        self
    }

    /// Override the volume-spike multiplier.
    pub fn with_volume_spike_multiplier(mut self, m: f64) -> Self {
        self.volume_spike_multiplier = m;
        self
    }
}

impl Default for AnomalyDetectorConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

/// Rolling-window statistics helper (f64 internally, prices converted).
struct RollingWindow {
    values: VecDeque<f64>,
    capacity: usize,
    sum: f64,
    sum_sq: f64,
}

impl RollingWindow {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    fn push(&mut self, v: f64) {
        if self.values.len() == self.capacity {
            if let Some(old) = self.values.pop_front() {
                self.sum -= old;
                self.sum_sq -= old * old;
            }
        }
        self.values.push_back(v);
        self.sum += v;
        self.sum_sq += v * v;
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn mean(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.sum / self.values.len() as f64
    }

    fn std_dev(&self) -> f64 {
        let n = self.values.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let variance = (self.sum_sq / n - (self.sum / n).powi(2)).max(0.0);
        variance.sqrt()
    }
}

/// Streaming tick anomaly detector.
///
/// Pass ticks through [`TickAnomalyDetector::process`]. Normal ticks are always
/// returned. Anomalies are additionally emitted to the `anomaly_tx` channel
/// provided at construction.
///
/// # Example
///
/// ```rust,no_run
/// use fin_stream::anomaly::{AnomalyDetectorConfig, TickAnomalyDetector};
///
/// # async fn example() {
/// let cfg = AnomalyDetectorConfig::default_config();
/// let (detector, mut anomaly_rx) = TickAnomalyDetector::new(cfg, 64);
/// # }
/// ```
pub struct TickAnomalyDetector {
    config: AnomalyDetectorConfig,
    price_window: RollingWindow,
    volume_window: RollingWindow,
    last_seq: Option<u64>,
    last_received_ms: Option<u64>,
    anomaly_tx: mpsc::Sender<AnomalyEvent>,
    ticks_processed: u64,
    anomalies_detected: u64,
}

impl TickAnomalyDetector {
    /// Create a new detector with the given config and anomaly channel capacity.
    ///
    /// Returns `(detector, anomaly_rx)`.
    pub fn new(
        config: AnomalyDetectorConfig,
        anomaly_channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<AnomalyEvent>) {
        let (anomaly_tx, anomaly_rx) = mpsc::channel(anomaly_channel_capacity);
        let price_window = RollingWindow::new(config.window_size);
        let volume_window = RollingWindow::new(config.window_size);
        let detector = Self {
            config,
            price_window,
            volume_window,
            last_seq: None,
            last_received_ms: None,
            anomaly_tx,
            ticks_processed: 0,
            anomalies_detected: 0,
        };
        (detector, anomaly_rx)
    }

    /// Process one tick, running all configured anomaly checks.
    ///
    /// Always returns the tick unchanged. Anomaly events are sent to the
    /// anomaly channel; if the channel is full the event is dropped with a
    /// warning (backpressure — normal ticks are never dropped).
    pub async fn process(&mut self, tick: NormalizedTick) -> NormalizedTick {
        let now_ms = now_ms();
        self.ticks_processed += 1;

        // --- Timestamp inversion ---
        if self.config.detect_timestamp_inversions {
            if let Some(prev_ms) = self.last_received_ms {
                if tick.received_at_ms < prev_ms {
                    let event = AnomalyEvent {
                        tick: tick.clone(),
                        kind: AnomalyKind::TimestampInversion {
                            previous_ms: prev_ms,
                            current_ms: tick.received_at_ms,
                        },
                        detected_at_ms: now_ms,
                    };
                    self.emit(event).await;
                }
            }
        }
        self.last_received_ms = Some(tick.received_at_ms);

        // --- Sequence gap ---
        if self.config.detect_sequence_gaps {
            if let Some(trade_id) = &tick.trade_id {
                if let Ok(seq) = trade_id.parse::<u64>() {
                    if let Some(last) = self.last_seq {
                        if seq > last + 1 {
                            let event = AnomalyEvent {
                                tick: tick.clone(),
                                kind: AnomalyKind::SequenceGap {
                                    last_seq: last,
                                    current_seq: seq,
                                },
                                detected_at_ms: now_ms,
                            };
                            self.emit(event).await;
                        }
                    }
                    self.last_seq = Some(seq);
                }
            }
        }

        let price_f64 = decimal_to_f64(tick.price);
        let qty_f64 = decimal_to_f64(tick.quantity);

        // --- Price spike --- (only when window is seeded)
        if self.price_window.len() >= 2 {
            let mean = self.price_window.mean();
            let std = self.price_window.std_dev();
            if std > 0.0 {
                let z = ((price_f64 - mean) / std).abs();
                if z > self.config.price_spike_z {
                    debug!(
                        symbol = %tick.symbol,
                        price = %tick.price,
                        z_score = z,
                        threshold = self.config.price_spike_z,
                        "price spike detected"
                    );
                    let event = AnomalyEvent {
                        tick: tick.clone(),
                        kind: AnomalyKind::PriceSpike {
                            z_score: format!("{z:.4}"),
                        },
                        detected_at_ms: now_ms,
                    };
                    self.emit(event).await;
                }
            }
        }

        // --- Volume spike --- (only when window is seeded)
        if self.volume_window.len() >= 2 {
            let mean_vol = self.volume_window.mean();
            if mean_vol > 0.0 {
                let ratio = qty_f64 / mean_vol;
                if ratio > self.config.volume_spike_multiplier {
                    debug!(
                        symbol = %tick.symbol,
                        quantity = %tick.quantity,
                        ratio = ratio,
                        threshold = self.config.volume_spike_multiplier,
                        "volume spike detected"
                    );
                    let event = AnomalyEvent {
                        tick: tick.clone(),
                        kind: AnomalyKind::VolumeSpike {
                            ratio: format!("{ratio:.4}"),
                        },
                        detected_at_ms: now_ms,
                    };
                    self.emit(event).await;
                }
            }
        }

        // Update rolling windows AFTER checks so current tick doesn't affect
        // its own detection.
        self.price_window.push(price_f64);
        self.volume_window.push(qty_f64);

        tick
    }

    async fn emit(&mut self, event: AnomalyEvent) {
        self.anomalies_detected += 1;
        if let Err(e) = self.anomaly_tx.try_send(event) {
            warn!("anomaly channel full, dropping event: {e}");
        }
    }

    /// Total ticks processed.
    pub fn ticks_processed(&self) -> u64 {
        self.ticks_processed
    }

    /// Total anomaly events emitted.
    pub fn anomalies_detected(&self) -> u64 {
        self.anomalies_detected
    }

    /// Current rolling price window mean, or `0.0` if no data.
    pub fn rolling_price_mean(&self) -> f64 {
        self.price_window.mean()
    }

    /// Current rolling price standard deviation, or `0.0` if insufficient data.
    pub fn rolling_price_std(&self) -> f64 {
        self.price_window.std_dev()
    }

    /// Current rolling volume mean, or `0.0` if no data.
    pub fn rolling_volume_mean(&self) -> f64 {
        self.volume_window.mean()
    }

    /// Number of ticks currently in the price rolling window.
    pub fn window_fill(&self) -> usize {
        self.price_window.len()
    }
}

fn decimal_to_f64(d: Decimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&d.to_string()).unwrap_or(0.0)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}
