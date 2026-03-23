//! Detect market events from streaming ticks.
//!
//! Provides [`EventDetector`] which maintains per-symbol state and emits
//! [`MarketEvent`]s when breakouts, volume spikes, reversals, or gap-opens
//! are detected.

use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A discrete market event detected from the tick stream.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketEvent {
    /// Price has broken above resistance or below support over the lookback window.
    Breakout {
        /// Asset identifier.
        symbol: String,
        /// +1 for upward breakout, -1 for downward breakout.
        direction: i8,
        /// Current price at time of breakout.
        price: f64,
        /// The resistance (upward) or support (downward) level breached.
        resistance_level: f64,
        /// Tick timestamp in milliseconds since Unix epoch.
        timestamp_ms: u64,
    },
    /// Current volume is a multiple of the rolling average above the configured threshold.
    VolumeSpike {
        /// Asset identifier.
        symbol: String,
        /// Current tick volume.
        volume: f64,
        /// Rolling average volume over the lookback window.
        avg_volume: f64,
        /// `volume / avg_volume`.
        ratio: f64,
        /// Tick timestamp in milliseconds since Unix epoch.
        timestamp_ms: u64,
    },
    /// Price direction has flipped after N consecutive bars in one direction.
    Reversal {
        /// Asset identifier.
        symbol: String,
        /// Current price at the reversal.
        price: f64,
        /// The prior trend direction: +1 = prior uptrend, -1 = prior downtrend.
        prior_trend: i8,
        /// Tick timestamp in milliseconds since Unix epoch.
        timestamp_ms: u64,
    },
    /// The opening price has gapped relative to the prior close by at least `min_gap_pct`.
    GapOpen {
        /// Asset identifier.
        symbol: String,
        /// Previous session's closing price.
        prev_close: f64,
        /// Current opening price.
        open_price: f64,
        /// Gap as a fraction: `(open_price - prev_close) / prev_close`.
        gap_pct: f64,
    },
}

/// Configuration for [`EventDetector`].
#[derive(Debug, Clone)]
pub struct EventDetectorConfig {
    /// Number of prior ticks used to compute the rolling max/min for breakout detection.
    pub breakout_lookback: usize,
    /// Volume spike threshold: spike is triggered when `volume / avg_volume >= threshold`.
    pub volume_spike_threshold: f64,
    /// Number of consecutive bars required in one direction before a reversal can be detected.
    pub reversal_window: usize,
    /// Minimum absolute gap percentage (0..1) to emit a [`MarketEvent::GapOpen`].
    pub min_gap_pct: f64,
}

impl Default for EventDetectorConfig {
    fn default() -> Self {
        Self {
            breakout_lookback: 20,
            volume_spike_threshold: 2.0,
            reversal_window: 3,
            min_gap_pct: 0.005, // 0.5 %
        }
    }
}

// ---------------------------------------------------------------------------
// Internal per-symbol state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SymbolState {
    /// Rolling price window for breakout detection.
    price_window: VecDeque<f64>,
    /// Rolling volume window for average volume computation.
    volume_window: VecDeque<f64>,
    /// Last N price directions: +1 up, -1 down, 0 flat.
    direction_history: VecDeque<i8>,
    /// Previous price (to derive direction).
    prev_price: Option<f64>,
    /// Previous close price for gap-open detection.
    prev_close: Option<f64>,
    /// Whether the current tick is the first tick of a new session (gap check).
    /// We track this by comparing large timestamp jumps (> 1 hour).
    last_timestamp_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// EventDetector
// ---------------------------------------------------------------------------

/// Stateful detector that processes a stream of ticks and emits [`MarketEvent`]s.
pub struct EventDetector {
    config: EventDetectorConfig,
    symbols: HashMap<String, SymbolState>,
}

impl EventDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: EventDetectorConfig) -> Self {
        Self { config, symbols: HashMap::new() }
    }

    /// Create a new detector with default configuration.
    pub fn default() -> Self {
        Self::new(EventDetectorConfig::default())
    }

    /// Process one tick update for `symbol` and return any events detected.
    ///
    /// `price`  — last trade price.
    /// `volume` — trade volume for this tick.
    /// `timestamp_ms` — Unix timestamp of the tick in milliseconds.
    pub fn update(
        &mut self,
        symbol: &str,
        price: f64,
        volume: f64,
        timestamp_ms: u64,
    ) -> Vec<MarketEvent> {
        let state = self.symbols.entry(symbol.to_string()).or_default();
        let mut events = Vec::new();

        // ---- Gap-open detection ----
        // Heuristic: if > 1 hour has passed since last tick, treat as new session open.
        if let (Some(lts), Some(pc)) = (state.last_timestamp_ms, state.prev_close) {
            if timestamp_ms > lts + 3_600_000 {
                let gap_pct = (price - pc) / pc;
                if gap_pct.abs() >= self.config.min_gap_pct {
                    events.push(MarketEvent::GapOpen {
                        symbol: symbol.to_string(),
                        prev_close: pc,
                        open_price: price,
                        gap_pct,
                    });
                }
            }
        }

        // ---- Update rolling price window ----
        state.price_window.push_back(price);
        if state.price_window.len() > self.config.breakout_lookback {
            state.price_window.pop_front();
        }

        // ---- Breakout detection ----
        // Only fire when the window is full to avoid spurious early signals.
        if state.price_window.len() == self.config.breakout_lookback {
            // Compute max/min over all but the last element (prior window)
            let prior: &VecDeque<f64> = &state.price_window;
            let prior_len = prior.len().saturating_sub(1);
            if prior_len > 0 {
                let mut max_prior = f64::NEG_INFINITY;
                let mut min_prior = f64::INFINITY;
                for (i, &p) in prior.iter().enumerate() {
                    if i < prior_len {
                        if p > max_prior { max_prior = p; }
                        if p < min_prior { min_prior = p; }
                    }
                }
                if price > max_prior {
                    events.push(MarketEvent::Breakout {
                        symbol: symbol.to_string(),
                        direction: 1,
                        price,
                        resistance_level: max_prior,
                        timestamp_ms,
                    });
                } else if price < min_prior {
                    events.push(MarketEvent::Breakout {
                        symbol: symbol.to_string(),
                        direction: -1,
                        price,
                        resistance_level: min_prior,
                        timestamp_ms,
                    });
                }
            }
        }

        // ---- Volume spike detection ----
        state.volume_window.push_back(volume);
        if state.volume_window.len() > self.config.breakout_lookback {
            state.volume_window.pop_front();
        }
        if state.volume_window.len() >= 2 {
            // avg excluding current bar
            let prior_vols: f64 = state.volume_window.iter().rev().skip(1).sum();
            let count = (state.volume_window.len() - 1) as f64;
            let avg_volume = prior_vols / count;
            if avg_volume > 0.0 {
                let ratio = volume / avg_volume;
                if ratio >= self.config.volume_spike_threshold {
                    events.push(MarketEvent::VolumeSpike {
                        symbol: symbol.to_string(),
                        volume,
                        avg_volume,
                        ratio,
                        timestamp_ms,
                    });
                }
            }
        }

        // ---- Reversal detection ----
        let direction: i8 = match state.prev_price {
            Some(pp) if price > pp => 1,
            Some(pp) if price < pp => -1,
            _ => 0,
        };
        state.prev_price = Some(price);

        if direction != 0 {
            state.direction_history.push_back(direction);
            if state.direction_history.len() > self.config.reversal_window + 1 {
                state.direction_history.pop_front();
            }

            if state.direction_history.len() > self.config.reversal_window {
                // Check if the last reversal_window directions (excluding current) were all the opposite
                let history_len = state.direction_history.len();
                let prior_slice: Vec<i8> = state
                    .direction_history
                    .iter()
                    .copied()
                    .take(history_len - 1)
                    .collect();
                let recent_count = self.config.reversal_window.min(prior_slice.len());
                let prior_trend_dir = prior_slice[prior_slice.len() - recent_count];
                let all_same = prior_slice[prior_slice.len() - recent_count..]
                    .iter()
                    .all(|&d| d == prior_trend_dir && d != 0);
                if all_same && direction != prior_trend_dir {
                    events.push(MarketEvent::Reversal {
                        symbol: symbol.to_string(),
                        price,
                        prior_trend: prior_trend_dir,
                        timestamp_ms,
                    });
                }
            }
        }

        // Save state for next tick
        state.prev_close = Some(price);
        state.last_timestamp_ms = Some(timestamp_ms);

        events
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_detector() -> EventDetector {
        EventDetector::new(EventDetectorConfig {
            breakout_lookback: 5,
            volume_spike_threshold: 2.0,
            reversal_window: 3,
            min_gap_pct: 0.01,
        })
    }

    // ---- Breakout ----

    #[test]
    fn test_breakout_upward() {
        let mut det = make_detector();
        // Feed prices 100, 101, 102, 103, 104 → window full, next tick at 110 is breakout
        for (i, p) in [100.0, 101.0, 102.0, 103.0, 104.0].iter().enumerate() {
            det.update("SYM", *p, 1.0, i as u64 * 1000);
        }
        let evts = det.update("SYM", 110.0, 1.0, 6000);
        let breakouts: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Breakout { direction: 1, .. })).collect();
        assert!(!breakouts.is_empty(), "Expected upward breakout");
    }

    #[test]
    fn test_breakout_downward() {
        let mut det = make_detector();
        for (i, p) in [100.0, 99.0, 98.0, 97.0, 96.0].iter().enumerate() {
            det.update("SYM", *p, 1.0, i as u64 * 1000);
        }
        let evts = det.update("SYM", 90.0, 1.0, 6000);
        let breakouts: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Breakout { direction: -1, .. })).collect();
        assert!(!breakouts.is_empty(), "Expected downward breakout");
    }

    #[test]
    fn test_no_breakout_within_range() {
        let mut det = make_detector();
        for (i, p) in [100.0, 101.0, 102.0, 103.0, 104.0].iter().enumerate() {
            det.update("SYM", *p, 1.0, i as u64 * 1000);
        }
        // 104 is not above 104, no breakout
        let evts = det.update("SYM", 104.0, 1.0, 6000);
        let breakouts: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Breakout { .. })).collect();
        assert!(breakouts.is_empty());
    }

    // ---- Volume spike ----

    #[test]
    fn test_volume_spike_detected() {
        let mut det = make_detector();
        // Baseline volume of 100 for several ticks
        for i in 0..4u64 {
            det.update("VOL", 100.0, 100.0, i * 1000);
        }
        // Spike at 500 → ratio 5x → above threshold of 2
        let evts = det.update("VOL", 100.0, 500.0, 5000);
        let spikes: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::VolumeSpike { .. })).collect();
        assert!(!spikes.is_empty(), "Expected volume spike");
        if let MarketEvent::VolumeSpike { ratio, .. } = spikes[0] {
            assert!(*ratio >= 2.0);
        }
    }

    #[test]
    fn test_no_volume_spike_normal_volume() {
        let mut det = make_detector();
        for i in 0..4u64 {
            det.update("VOL", 100.0, 100.0, i * 1000);
        }
        // Only 1.5x, below threshold
        let evts = det.update("VOL", 100.0, 150.0, 5000);
        let spikes: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::VolumeSpike { .. })).collect();
        assert!(spikes.is_empty());
    }

    // ---- Reversal ----

    #[test]
    fn test_reversal_after_uptrend() {
        let mut det = make_detector();
        // 3 consecutive up ticks, then a down tick → reversal
        det.update("REV", 100.0, 1.0, 0);
        det.update("REV", 101.0, 1.0, 1000);
        det.update("REV", 102.0, 1.0, 2000);
        det.update("REV", 103.0, 1.0, 3000);
        // Now go down
        let evts = det.update("REV", 102.0, 1.0, 4000);
        let reversals: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Reversal { prior_trend: 1, .. })).collect();
        assert!(!reversals.is_empty(), "Expected reversal from uptrend");
    }

    #[test]
    fn test_reversal_after_downtrend() {
        let mut det = make_detector();
        det.update("REV", 103.0, 1.0, 0);
        det.update("REV", 102.0, 1.0, 1000);
        det.update("REV", 101.0, 1.0, 2000);
        det.update("REV", 100.0, 1.0, 3000);
        // Now go up
        let evts = det.update("REV", 101.0, 1.0, 4000);
        let reversals: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Reversal { prior_trend: -1, .. })).collect();
        assert!(!reversals.is_empty(), "Expected reversal from downtrend");
    }

    // ---- Gap open ----

    #[test]
    fn test_gap_open_detected() {
        let mut det = make_detector();
        // First tick establishes prev_close
        det.update("GAP", 100.0, 1.0, 1_000_000);
        // > 1 hour later (3_600_001 ms gap), price jumps 5 % → gap open
        let evts = det.update("GAP", 105.0, 1.0, 1_000_000 + 3_600_001);
        let gaps: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::GapOpen { .. })).collect();
        assert!(!gaps.is_empty(), "Expected gap open event");
        if let MarketEvent::GapOpen { gap_pct, .. } = gaps[0] {
            assert!((*gap_pct - 0.05).abs() < 1e-6);
        }
    }

    #[test]
    fn test_no_gap_open_small_move() {
        let mut det = make_detector();
        det.update("GAP", 100.0, 1.0, 1_000_000);
        // Only 0.5 % move (min_gap_pct = 1 %) → no gap
        let evts = det.update("GAP", 100.5, 1.0, 1_000_000 + 3_600_001);
        let gaps: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::GapOpen { .. })).collect();
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_no_gap_open_continuous_feed() {
        let mut det = make_detector();
        det.update("GAP", 100.0, 1.0, 1_000_000);
        // Only 10 seconds later — not a session gap
        let evts = det.update("GAP", 110.0, 1.0, 1_010_000);
        let gaps: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::GapOpen { .. })).collect();
        assert!(gaps.is_empty());
    }

    // ---- Multi-symbol isolation ----

    #[test]
    fn test_symbols_are_independent() {
        let mut det = make_detector();
        // Build up window for AAPL
        for i in 0..5u64 {
            det.update("AAPL", 100.0 + i as f64, 1.0, i * 1000);
        }
        // GOOG has no history; first tick should NOT produce a breakout
        let evts = det.update("GOOG", 200.0, 1.0, 0);
        let breakouts: Vec<_> = evts.iter().filter(|e| matches!(e, MarketEvent::Breakout { .. })).collect();
        assert!(breakouts.is_empty(), "New symbol should not immediately break out");
    }

    // ---- Config default ----

    #[test]
    fn test_default_config() {
        let cfg = EventDetectorConfig::default();
        assert_eq!(cfg.breakout_lookback, 20);
        assert_eq!(cfg.reversal_window, 3);
        assert!(cfg.volume_spike_threshold > 1.0);
    }
}
