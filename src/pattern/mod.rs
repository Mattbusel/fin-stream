//! Real-time candlestick pattern detection on streaming bars.
//!
//! Feed bars into [`PatternDetector`] one at a time; call [`PatternDetector::detect`]
//! after each push to receive any newly identified patterns.  For concurrent,
//! multi-symbol use, wrap detectors in a [`StreamingPatternMonitor`].

use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};

// ─────────────────────────────────────────────────────────────────────────────
// Core types
// ─────────────────────────────────────────────────────────────────────────────

/// A single OHLCV bar from a streaming feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// Opening price.
    pub open: f64,
    /// High price.
    pub high: f64,
    /// Low price.
    pub low: f64,
    /// Closing price.
    pub close: f64,
    /// Traded volume.
    pub volume: f64,
    /// Bar open time in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
}

/// Named candlestick / price-action patterns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatternType {
    /// Open ≈ close; indecision candle.
    Doji,
    /// Long lower shadow, small body near the top.
    Hammer,
    /// Bullish bar that fully engulfs the prior bearish bar.
    BullishEngulfing,
    /// Bearish bar that fully engulfs the prior bullish bar.
    BearishEngulfing,
    /// Three-bar bottom reversal pattern.
    MorningStar,
    /// Three-bar top reversal pattern.
    EveningStar,
    /// Long tail (wick) ≥ 2.5× the body, small opposite wick.
    PinBar,
    /// Current bar's high/low is entirely within the prior bar's range.
    InsideBar,
}

/// Directional bias of a detected pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Direction {
    /// Pattern implies upward price pressure.
    Bullish,
    /// Pattern implies downward price pressure.
    Bearish,
    /// No directional bias (e.g. Doji).
    Neutral,
}

/// A single detected pattern signal.
#[derive(Debug, Clone)]
pub struct PatternSignal {
    /// Which pattern was detected.
    pub pattern: PatternType,
    /// Index into the detector's internal window (0 = oldest in the window).
    pub bar_index: usize,
    /// Normalised signal strength in \[0.0, 1.0\].
    pub strength: f64,
    /// Implied direction.
    pub direction: Direction,
}

// ─────────────────────────────────────────────────────────────────────────────
// PatternDetector
// ─────────────────────────────────────────────────────────────────────────────

const WINDOW: usize = 5;

/// Stateful rolling-window pattern detector for a single instrument.
///
/// Maintains a sliding window of the last 5 bars.
pub struct PatternDetector {
    window: VecDeque<Bar>,
}

impl PatternDetector {
    /// Create a new, empty detector.
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW + 1),
        }
    }

    /// Push a new bar into the sliding window, evicting the oldest if needed.
    pub fn push(&mut self, bar: Bar) {
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(bar);
    }

    /// Scan the current window for all recognised patterns.
    pub fn detect(&self) -> Vec<PatternSignal> {
        let bars: Vec<&Bar> = self.window.iter().collect();
        let n = bars.len();
        let mut signals = Vec::new();

        if n == 0 {
            return signals;
        }

        // Work from newest bar backward.
        let last_idx = n - 1;
        let cur = bars[last_idx];

        let body = (cur.close - cur.open).abs();
        let range = cur.high - cur.low;

        // ── Doji ─────────────────────────────────────────────────────────────
        if range > 0.0 && body / range < 0.1 {
            signals.push(PatternSignal {
                pattern: PatternType::Doji,
                bar_index: last_idx,
                strength: 1.0 - body / range,
                direction: Direction::Neutral,
            });
        }

        // ── Hammer ───────────────────────────────────────────────────────────
        if body > 0.0 && range > 0.0 {
            let lower_wick = cur.close.min(cur.open) - cur.low;
            let upper_wick = cur.high - cur.close.max(cur.open);
            if lower_wick >= 2.0 * body && upper_wick <= 0.5 * body {
                signals.push(PatternSignal {
                    pattern: PatternType::Hammer,
                    bar_index: last_idx,
                    strength: (lower_wick / body / 2.0).min(1.0),
                    direction: Direction::Bullish,
                });
            }

            // ── PinBar ───────────────────────────────────────────────────────
            // Tail (longest wick) ≥ 2.5× body, small opposite wick.
            let long_wick = lower_wick.max(upper_wick);
            let short_wick = lower_wick.min(upper_wick);
            if body > 0.0 && long_wick >= 2.5 * body && short_wick <= 0.5 * body {
                let dir = if lower_wick > upper_wick {
                    Direction::Bullish
                } else {
                    Direction::Bearish
                };
                signals.push(PatternSignal {
                    pattern: PatternType::PinBar,
                    bar_index: last_idx,
                    strength: (long_wick / body / 2.5).min(1.0),
                    direction: dir,
                });
            }
        }

        // ── Two-bar patterns ─────────────────────────────────────────────────
        if n >= 2 {
            let prev = bars[last_idx - 1];
            let prev_bearish = prev.close < prev.open;
            let prev_bullish = prev.close > prev.open;
            let cur_bullish = cur.close > cur.open;
            let cur_bearish = cur.close < cur.open;

            // InsideBar
            if cur.high < prev.high && cur.low > prev.low {
                signals.push(PatternSignal {
                    pattern: PatternType::InsideBar,
                    bar_index: last_idx,
                    strength: 0.7,
                    direction: Direction::Neutral,
                });
            }

            // BullishEngulfing
            if prev_bearish
                && cur_bullish
                && cur.open <= prev.close
                && cur.close >= prev.open
            {
                let prev_body = (prev.close - prev.open).abs();
                let cur_body_val = (cur.close - cur.open).abs();
                let strength = if prev_body > 0.0 {
                    (cur_body_val / prev_body).min(1.0)
                } else {
                    0.5
                };
                signals.push(PatternSignal {
                    pattern: PatternType::BullishEngulfing,
                    bar_index: last_idx,
                    strength,
                    direction: Direction::Bullish,
                });
            }

            // BearishEngulfing
            if prev_bullish
                && cur_bearish
                && cur.open >= prev.close
                && cur.close <= prev.open
            {
                let prev_body = (prev.close - prev.open).abs();
                let cur_body_val = (cur.close - cur.open).abs();
                let strength = if prev_body > 0.0 {
                    (cur_body_val / prev_body).min(1.0)
                } else {
                    0.5
                };
                signals.push(PatternSignal {
                    pattern: PatternType::BearishEngulfing,
                    bar_index: last_idx,
                    strength,
                    direction: Direction::Bearish,
                });
            }
        }

        // ── Three-bar patterns ───────────────────────────────────────────────
        if n >= 3 {
            let b0 = bars[last_idx - 2];
            let b1 = bars[last_idx - 1];

            let b0_bearish = b0.close < b0.open;
            let b0_bullish = b0.close > b0.open;
            let b1_range = b1.high - b1.low;
            let b1_body = (b1.close - b1.open).abs();
            let b1_small = b1_range > 0.0 && b1_body / b1_range < 0.3;
            let cur_bullish = cur.close > cur.open;
            let cur_bearish = cur.close < cur.open;

            // MorningStar
            if b0_bearish
                && b1_small
                && cur_bullish
                && cur.close > (b0.open + b0.close) / 2.0
            {
                signals.push(PatternSignal {
                    pattern: PatternType::MorningStar,
                    bar_index: last_idx,
                    strength: 0.85,
                    direction: Direction::Bullish,
                });
            }

            // EveningStar
            if b0_bullish
                && b1_small
                && cur_bearish
                && cur.close < (b0.open + b0.close) / 2.0
            {
                signals.push(PatternSignal {
                    pattern: PatternType::EveningStar,
                    bar_index: last_idx,
                    strength: 0.85,
                    direction: Direction::Bearish,
                });
            }
        }

        signals
    }
}

impl Default for PatternDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PatternStats
// ─────────────────────────────────────────────────────────────────────────────

/// Rolling statistics for a single symbol's pattern detector.
#[derive(Debug, Default)]
pub struct PatternStats {
    /// Cumulative count of each pattern type detected.
    pub pattern_counts: HashMap<PatternType, u32>,
    /// Most recently detected signal (if any).
    pub last_signal: Option<PatternSignal>,
}

impl PatternStats {
    /// Update stats from a batch of newly detected signals.
    pub fn update(&mut self, signals: &[PatternSignal]) {
        for sig in signals {
            *self.pattern_counts.entry(sig.pattern.clone()).or_insert(0) += 1;
            self.last_signal = Some(sig.clone());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// StreamingPatternMonitor
// ─────────────────────────────────────────────────────────────────────────────

/// Concurrent, multi-symbol streaming pattern monitor.
///
/// Internally stores one [`PatternDetector`] per symbol in a [`DashMap`],
/// so `update` can be called from multiple threads simultaneously on different
/// symbols without contention.
pub struct StreamingPatternMonitor {
    detectors: DashMap<String, PatternDetector>,
}

impl StreamingPatternMonitor {
    /// Create a new, empty monitor.
    pub fn new() -> Self {
        Self {
            detectors: DashMap::new(),
        }
    }

    /// Push a new bar for `symbol` and return any detected patterns.
    pub fn update(&self, symbol: &str, bar: Bar) -> Vec<PatternSignal> {
        let mut entry = self
            .detectors
            .entry(symbol.to_string())
            .or_insert_with(PatternDetector::new);
        entry.push(bar);
        entry.detect()
    }
}

impl Default for StreamingPatternMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar { open: o, high: h, low: l, close: c, volume: 1000.0, timestamp_ms: 0 }
    }

    #[test]
    fn doji_detected() {
        let mut det = PatternDetector::new();
        det.push(bar(10.0, 12.0, 8.0, 10.0)); // open == close, has range → Doji
        let sigs = det.detect();
        assert!(
            sigs.iter().any(|s| s.pattern == PatternType::Doji),
            "expected Doji signal"
        );
    }

    #[test]
    fn inside_bar_detected() {
        let mut det = PatternDetector::new();
        det.push(bar(100.0, 110.0, 90.0, 105.0));
        det.push(bar(102.0, 107.0, 95.0, 104.0)); // entirely inside prior bar
        let sigs = det.detect();
        assert!(
            sigs.iter().any(|s| s.pattern == PatternType::InsideBar),
            "expected InsideBar signal"
        );
    }

    #[test]
    fn bullish_engulfing_detected() {
        let mut det = PatternDetector::new();
        det.push(bar(12.0, 13.0, 10.0, 10.5)); // bearish
        det.push(bar(9.5, 13.5, 9.0, 13.0));   // bullish engulfs
        let sigs = det.detect();
        assert!(
            sigs.iter().any(|s| s.pattern == PatternType::BullishEngulfing),
            "expected BullishEngulfing signal"
        );
    }

    #[test]
    fn morning_star_detected() {
        let mut det = PatternDetector::new();
        det.push(bar(105.0, 106.0, 95.0, 96.0));  // bearish
        det.push(bar(95.5, 96.0, 94.0, 95.7));    // small doji/star
        det.push(bar(96.0, 107.0, 95.0, 106.0));  // bullish, closes above midpoint of bar0
        let sigs = det.detect();
        assert!(
            sigs.iter().any(|s| s.pattern == PatternType::MorningStar),
            "expected MorningStar signal, got: {:?}",
            sigs.iter().map(|s| &s.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn streaming_monitor_multi_symbol() {
        let monitor = StreamingPatternMonitor::new();
        monitor.update("AAPL", bar(10.0, 12.0, 8.0, 10.0));
        monitor.update("TSLA", bar(200.0, 220.0, 180.0, 200.0));
        // Both symbols should have independent detectors.
        assert_eq!(monitor.detectors.len(), 2);
    }

    #[test]
    fn pin_bar_detected() {
        let mut det = PatternDetector::new();
        // Long lower wick: low = 90, open/close ~ 100, high = 101
        det.push(bar(100.0, 101.0, 90.0, 100.5));
        let sigs = det.detect();
        assert!(
            sigs.iter().any(|s| s.pattern == PatternType::PinBar),
            "expected PinBar signal"
        );
    }

    #[test]
    fn strength_in_range() {
        let mut det = PatternDetector::new();
        det.push(bar(10.0, 12.0, 8.0, 10.0));
        for sig in det.detect() {
            assert!(
                sig.strength >= 0.0 && sig.strength <= 1.0,
                "strength out of range: {}",
                sig.strength
            );
        }
    }
}
