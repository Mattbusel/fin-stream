//! Trading signal aggregator with confidence weighting and time decay.
//!
//! ## Architecture
//!
//! ```text
//! Individual signals (Momentum, MeanReversion, Breakout, …)
//!      │  weight
//!      ▼
//! SignalAggregator  (DashMap<symbol, VecDeque<WeightedSignal>>)
//!      │  decay_weight = exp(-(now - ts) / decay_ms)
//!      ▼
//! AggregatedSignal  { net_direction, confidence, dominant_type, … }
//!      │
//!      └──► top_signals  (sorted by |direction * confidence|)
//! ```
//!
//! ## Guarantees
//! - Zero panics; all divisions guarded.
//! - DashMap for concurrent access without a global lock.
//! - Signals older than `decay_ms` are pruned on every `record` call.

use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};

// ─────────────────────────────────────────────────────────────────────────────
//  SignalType
// ─────────────────────────────────────────────────────────────────────────────

/// The category of a trading signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalType {
    /// Price momentum signal.
    Momentum,
    /// Mean-reversion signal.
    MeanReversion,
    /// Breakout signal.
    Breakout,
    /// News or social-sentiment signal.
    Sentiment,
    /// Fundamental / macro signal.
    Fundamental,
    /// Technical indicator signal.
    Technical,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Signal
// ─────────────────────────────────────────────────────────────────────────────

/// A single trading signal for one symbol.
#[derive(Debug, Clone)]
pub struct Signal {
    /// Trading symbol (e.g. `"BTCUSDT"`).
    pub symbol: String,
    /// Category of the signal.
    pub signal_type: SignalType,
    /// Directional score in `[-1.0, 1.0]`: negative = bearish, positive = bullish.
    pub direction: f64,
    /// Confidence in `[0.0, 1.0]`: how certain the signal source is.
    pub confidence: f64,
    /// Strength in `[0.0, 1.0]`: magnitude of the underlying factor.
    pub strength: f64,
    /// Human-readable source identifier (e.g. model name, data feed).
    pub source: String,
    /// When the signal was generated (Unix milliseconds).
    pub timestamp_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  WeightedSignal
// ─────────────────────────────────────────────────────────────────────────────

/// A [`Signal`] paired with an external importance weight.
#[derive(Debug, Clone)]
pub struct WeightedSignal {
    /// The underlying signal.
    pub signal: Signal,
    /// Importance weight assigned by the aggregator user (e.g. 1.0 = normal).
    pub weight: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  AggregatedSignal
// ─────────────────────────────────────────────────────────────────────────────

/// Consensus signal for a symbol after weighting and time-decay.
#[derive(Debug, Clone)]
pub struct AggregatedSignal {
    /// Symbol this aggregation is for.
    pub symbol: String,
    /// Weighted-average directional score `[-1.0, 1.0]`.
    pub net_direction: f64,
    /// Weighted-average confidence `[0.0, 1.0]`.
    pub confidence: f64,
    /// Number of individual signals that contributed.
    pub signal_count: u32,
    /// The signal type that contributed the most (by effective weight).
    pub dominant_type: SignalType,
    /// Wall-clock timestamp of the aggregation (Unix milliseconds).
    pub timestamp_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  SignalAggregator
// ─────────────────────────────────────────────────────────────────────────────

/// Concurrent multi-symbol signal aggregator with time-exponential decay.
pub struct SignalAggregator {
    /// Per-symbol ring of weighted signals.
    signals: DashMap<String, VecDeque<WeightedSignal>>,
    /// Per-type base weight multiplier (optional override).
    weights: HashMap<SignalType, f64>,
    /// Time horizon in milliseconds; signals older than this are pruned.
    decay_ms: u64,
}

impl SignalAggregator {
    /// Create a new aggregator.
    ///
    /// # Arguments
    /// - `decay_ms` — signals older than this many milliseconds receive exponentially
    ///   decayed weight; signals older than 3× this are pruned entirely.
    pub fn new(decay_ms: u64) -> Self {
        Self {
            signals: DashMap::new(),
            weights: HashMap::new(),
            decay_ms,
        }
    }

    /// Register a type-level base weight for a `SignalType`.
    pub fn add_weight(&mut self, signal_type: SignalType, weight: f64) {
        self.weights.insert(signal_type, weight);
    }

    /// Record a new signal.
    ///
    /// Appends to the symbol's queue and prunes entries older than `3 * decay_ms`.
    pub fn record(&self, signal: Signal, weight: f64) {
        let cutoff_ms = signal.timestamp_ms.saturating_sub(3 * self.decay_ms);
        let symbol = signal.symbol.clone();
        let ws = WeightedSignal { signal, weight };
        let mut entry = self.signals.entry(symbol).or_default();
        // Prune stale signals
        while entry.front().map_or(false, |s| s.signal.timestamp_ms < cutoff_ms) {
            entry.pop_front();
        }
        entry.push_back(ws);
    }

    /// Aggregate all signals for `symbol` as of `now_ms`.
    ///
    /// Each signal's effective weight = `signal.weight * type_weight * decay_weight`
    /// where `decay_weight = exp(-(now_ms - ts) / decay_ms)`.
    ///
    /// Returns `None` if no signals exist for the symbol.
    pub fn aggregate(&self, symbol: &str, now_ms: u64) -> Option<AggregatedSignal> {
        let entry = self.signals.get(symbol)?;
        if entry.is_empty() {
            return None;
        }

        let decay_ms_f = self.decay_ms.max(1) as f64;
        let mut total_weight = 0.0_f64;
        let mut dir_sum = 0.0_f64;
        let mut conf_sum = 0.0_f64;
        let mut count = 0_u32;
        // Track dominant type by summed effective weight
        let mut type_weights: HashMap<SignalType, f64> = HashMap::new();

        for ws in entry.iter() {
            let age_ms = now_ms.saturating_sub(ws.signal.timestamp_ms) as f64;
            let decay_weight = (-age_ms / decay_ms_f).exp();
            let type_weight = self.weights.get(&ws.signal.signal_type).copied().unwrap_or(1.0);
            let eff_weight = ws.weight * type_weight * decay_weight;

            dir_sum += ws.signal.direction * eff_weight;
            conf_sum += ws.signal.confidence * eff_weight;
            total_weight += eff_weight;
            count += 1;
            *type_weights.entry(ws.signal.signal_type).or_default() += eff_weight;
        }

        if total_weight < f64::EPSILON {
            return None;
        }

        let dominant_type = type_weights
            .into_iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(t, _)| t)
            .unwrap_or(SignalType::Technical);

        Some(AggregatedSignal {
            symbol: symbol.to_string(),
            net_direction: dir_sum / total_weight,
            confidence: conf_sum / total_weight,
            signal_count: count,
            dominant_type,
            timestamp_ms: now_ms,
        })
    }

    /// Return aggregated signals for all symbols, sorted descending by
    /// `|net_direction * confidence|`.
    pub fn top_signals(&self, n: usize, now_ms: u64) -> Vec<AggregatedSignal> {
        let mut results: Vec<AggregatedSignal> = self
            .signals
            .iter()
            .filter_map(|entry| self.aggregate(entry.key(), now_ms))
            .collect();
        results.sort_by(|a, b| {
            let score_a = (a.net_direction * a.confidence).abs();
            let score_b = (b.net_direction * b.confidence).abs();
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(n);
        results
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  SignalFilter
// ─────────────────────────────────────────────────────────────────────────────

/// Filters a slice of signals by minimum confidence, strength, and allowed types.
pub struct SignalFilter {
    /// Discard signals with `confidence < min_confidence`.
    pub min_confidence: f64,
    /// Discard signals with `strength < min_strength`.
    pub min_strength: f64,
    /// Only keep signals whose type is in this list (empty = allow all).
    pub allowed_types: Vec<SignalType>,
}

impl SignalFilter {
    /// Apply the filter and return a `Vec` of passing signals (cloned).
    pub fn filter(&self, signals: &[Signal]) -> Vec<Signal> {
        signals
            .iter()
            .filter(|s| {
                s.confidence >= self.min_confidence
                    && s.strength >= self.min_strength
                    && (self.allowed_types.is_empty()
                        || self.allowed_types.contains(&s.signal_type))
            })
            .cloned()
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_signal(symbol: &str, direction: f64, confidence: f64, ts: u64) -> Signal {
        Signal {
            symbol: symbol.to_string(),
            signal_type: SignalType::Momentum,
            direction,
            confidence,
            strength: 0.8,
            source: "test".to_string(),
            timestamp_ms: ts,
        }
    }

    #[test]
    fn aggregate_single_signal() {
        let agg = SignalAggregator::new(60_000);
        let now = 1_000_000_u64;
        agg.record(make_signal("BTCUSDT", 0.8, 0.9, now), 1.0);
        let result = agg.aggregate("BTCUSDT", now).unwrap();
        assert!((result.net_direction - 0.8).abs() < 1e-9);
        assert!((result.confidence - 0.9).abs() < 1e-9);
        assert_eq!(result.signal_count, 1);
    }

    #[test]
    fn aggregate_weighted_average() {
        let agg = SignalAggregator::new(60_000);
        let now = 1_000_000_u64;
        // Two equal-age signals: direction 0.6 w=1 and 0.2 w=3 → (0.6+0.6)/4 = 0.3
        agg.record(make_signal("AAPL", 0.6, 0.5, now), 1.0);
        agg.record(make_signal("AAPL", 0.2, 0.5, now), 3.0);
        let result = agg.aggregate("AAPL", now).unwrap();
        // weighted dir = (0.6*1 + 0.2*3) / (1+3) = (0.6+0.6)/4 = 0.3
        assert!((result.net_direction - 0.3).abs() < 1e-9, "dir = {}", result.net_direction);
    }

    #[test]
    fn time_decay_reduces_old_signal_weight() {
        let decay_ms = 10_000_u64;
        let agg = SignalAggregator::new(decay_ms);
        let now = 100_000_u64;
        // Old signal: 50s ago (5× decay_ms → very small weight)
        let old_ts = now - 50_000;
        // New signal at now
        agg.record(make_signal("ETH", 1.0, 1.0, old_ts), 1.0);
        agg.record(make_signal("ETH", -1.0, 1.0, now), 1.0);

        let result = agg.aggregate("ETH", now).unwrap();
        // Old signal decayed strongly → net_direction should be close to -1 (new signal dominates)
        assert!(result.net_direction < 0.0, "new signal should dominate, dir = {}", result.net_direction);
    }

    #[test]
    fn empty_symbol_returns_none() {
        let agg = SignalAggregator::new(60_000);
        assert!(agg.aggregate("NONEXISTENT", 1_000_000).is_none());
    }

    #[test]
    fn top_signals_sorted_by_score() {
        let agg = SignalAggregator::new(60_000);
        let now = 1_000_000_u64;
        // BTC: direction=0.9, confidence=0.9 → score=0.81
        // ETH: direction=0.2, confidence=0.5 → score=0.10
        agg.record(make_signal("BTC", 0.9, 0.9, now), 1.0);
        agg.record(make_signal("ETH", 0.2, 0.5, now), 1.0);
        let top = agg.top_signals(2, now);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].symbol, "BTC");
    }

    #[test]
    fn signal_filter_by_confidence() {
        let filter = SignalFilter {
            min_confidence: 0.7,
            min_strength: 0.0,
            allowed_types: vec![],
        };
        let signals = vec![
            make_signal("A", 0.5, 0.9, 0),
            make_signal("B", 0.5, 0.5, 0),
        ];
        let filtered = filter.filter(&signals);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].symbol, "A");
    }

    #[test]
    fn signal_filter_by_type() {
        let filter = SignalFilter {
            min_confidence: 0.0,
            min_strength: 0.0,
            allowed_types: vec![SignalType::Breakout],
        };
        let mut s = make_signal("A", 0.5, 0.9, 0);
        s.signal_type = SignalType::Breakout;
        let signals = vec![s, make_signal("B", 0.5, 0.9, 0)]; // second is Momentum
        let filtered = filter.filter(&signals);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].symbol, "A");
    }
}
