//! MEV (Maximal Extractable Value) detection scaffold.
//!
//! ## Responsibility
//! Provide heuristic detection of common MEV patterns in a stream of
//! [`NormalizedTick`]s. This is an **analysis scaffold**: it operates entirely
//! on price/quantity data from the feed and requires no Flashbots API access.
//!
//! ## Patterns Detected
//!
//! ### Sandwich Attack
//! A sandwich consists of three ordered ticks within a configurable window:
//! 1. A large buy (price impact above threshold) by the attacker.
//! 2. A victim's transaction (any tick at the elevated price).
//! 3. A large sell by the attacker, restoring the price.
//!
//! Detection heuristic: within the window `[t, t+window_size)`, find any
//! sequence where the price rises sharply (`buy`), stays high for at least
//! one tick, then drops back (`sell`), with the initial and final moves
//! exceeding `price_impact_threshold`.
//!
//! ### Frontrun
//! A frontrun occurs when a tick at a price very close to the previous tick
//! arrives within 2 ticks of a larger-volume trade at the same price,
//! suggesting the frontrunner copied the transaction with higher priority.
//!
//! ### Backrun
//! The mirror of frontrun: a sell immediately following a large buy within
//! 1–2 ticks at only slightly lower price.
//!
//! ## Limitations
//! The heuristics operate on a flat tick stream without mempool visibility.
//! The `estimated_profit_usd` field is a coarse estimate based on price
//! impact × quantity, not actual realized PnL. False positives are expected
//! in high-volatility conditions; callers should gate on `confidence`.

use crate::tick::NormalizedTick;
use rust_decimal::prelude::ToPrimitive;

/// Classification of the detected MEV pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MevPattern {
    /// Sandwich: large buy → victim tx → large sell within the window.
    Sandwich,
    /// Frontrun: attacker tx appears just before a large trade at same price.
    Frontrun,
    /// Backrun: attacker sell appears just after a large buy.
    Backrun,
}

impl std::fmt::Display for MevPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MevPattern::Sandwich => write!(f, "Sandwich"),
            MevPattern::Frontrun => write!(f, "Frontrun"),
            MevPattern::Backrun => write!(f, "Backrun"),
        }
    }
}

/// A single detected MEV candidate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MevCandidate {
    /// Hash-like identifier of the triggering transaction.
    ///
    /// For tick-based detection this is derived from the `trade_id` of the
    /// anchor tick (the first tick in the pattern). Set to `"unknown"` when
    /// no `trade_id` is present.
    pub tx_hash: String,
    /// The pattern that was detected.
    pub detected_pattern: MevPattern,
    /// Coarse estimated profit in USD.
    ///
    /// Computed as `|price_move| × quantity × price_impact_threshold`.
    /// This is an order-of-magnitude estimate only.
    pub estimated_profit_usd: f64,
    /// Detection confidence in [0.0, 1.0].
    ///
    /// Higher values indicate more characteristics of the pattern are present
    /// (e.g. price impact is 5× the threshold, not just barely over it).
    pub confidence: f64,
    /// Index of the anchor tick in the input slice.
    pub anchor_tick_index: usize,
}

/// Heuristic MEV detector operating on slices of [`NormalizedTick`].
///
/// ## Example
/// ```rust
/// use fin_stream::mev::MevDetector;
/// // construct detector with 0.5% price impact threshold
/// let detector = MevDetector::new(0.005);
/// // feed ticks from a block-equivalent window
/// // let candidates = detector.analyze_block(&ticks);
/// ```
#[derive(Debug, Clone)]
pub struct MevDetector {
    /// Minimum price impact (as a fraction, e.g. `0.005` = 0.5%) to flag a
    /// tick as a potential large move.
    pub price_impact_threshold: f64,
    /// Number of ticks to look ahead when searching for pattern completion.
    pub window_size: usize,
}

impl MevDetector {
    /// Create a new detector.
    ///
    /// # Arguments
    /// * `price_impact_threshold` — fraction of price move required to flag
    ///   a tick as a large move. Values are clamped to (0.0, 1.0].
    pub fn new(price_impact_threshold: f64) -> Self {
        let threshold = price_impact_threshold.clamp(f64::MIN_POSITIVE, 1.0);
        Self {
            price_impact_threshold: threshold,
            window_size: 20,
        }
    }

    /// Create a detector with a custom window size (default 20).
    pub fn with_window(price_impact_threshold: f64, window_size: usize) -> Self {
        Self {
            price_impact_threshold: price_impact_threshold.clamp(f64::MIN_POSITIVE, 1.0),
            window_size: window_size.max(3),
        }
    }

    /// Analyse a slice of ticks (a "block-equivalent" window) and return all
    /// detected MEV candidates.
    ///
    /// Ticks should be pre-filtered to a single symbol and sorted by timestamp.
    /// Mixed-symbol slices will produce false positives.
    ///
    /// Returns an empty `Vec` when the slice has fewer than 3 ticks or no
    /// patterns are detected.
    pub fn analyze_block(&self, ticks: &[NormalizedTick]) -> Vec<MevCandidate> {
        if ticks.len() < 3 {
            return Vec::new();
        }

        let prices: Vec<f64> = ticks
            .iter()
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();

        let quantities: Vec<f64> = ticks
            .iter()
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .collect();

        let mut candidates: Vec<MevCandidate> = Vec::new();

        // Run each detector pass.
        self.detect_sandwiches(&prices, &quantities, ticks, &mut candidates);
        self.detect_frontruns(&prices, &quantities, ticks, &mut candidates);
        self.detect_backruns(&prices, &quantities, ticks, &mut candidates);

        candidates
    }

    /// Detect sandwich patterns.
    ///
    /// Pattern: ticks[i] causes sharp upward price move → ticks[i+1..i+k-1]
    /// are "victim" ticks at the elevated price → ticks[i+k] causes a sharp
    /// downward move back to near the original price.
    fn detect_sandwiches(
        &self,
        prices: &[f64],
        quantities: &[f64],
        ticks: &[NormalizedTick],
        out: &mut Vec<MevCandidate>,
    ) {
        let n = prices.len();
        let win = self.window_size.min(n.saturating_sub(2));

        for i in 0..n.saturating_sub(2) {
            let p0 = prices[i];
            if p0 == 0.0 {
                continue;
            }

            // Scan forward for a matching sell within the window.
            let lookahead = (i + 2)..(i + win + 2).min(n);
            for j in lookahead {
                let p_peak = prices[j - 1]; // highest point
                let p_final = prices[j];    // price after sell

                let buy_impact = (p_peak - p0) / p0;
                let sell_impact = (p_peak - p_final) / p_peak;

                if buy_impact >= self.price_impact_threshold
                    && sell_impact >= self.price_impact_threshold
                {
                    // Intermediate ticks (the "victim" region) must exist.
                    if j - i < 2 {
                        continue;
                    }

                    let qty = quantities[i];
                    let estimated_profit =
                        (p_peak - p0) * qty * self.price_impact_threshold.recip().min(10.0);
                    let confidence = Self::confidence_from_impact(
                        buy_impact.min(sell_impact),
                        self.price_impact_threshold,
                    );

                    let tx_hash = ticks[i]
                        .trade_id
                        .clone()
                        .unwrap_or_else(|| format!("tick-{i}"));

                    out.push(MevCandidate {
                        tx_hash,
                        detected_pattern: MevPattern::Sandwich,
                        estimated_profit_usd: estimated_profit,
                        confidence,
                        anchor_tick_index: i,
                    });
                    break; // one sandwich per anchor tick
                }
            }
        }
    }

    /// Detect frontrun patterns.
    ///
    /// Heuristic: tick[i] has a price very close to tick[i+1] but tick[i+1]
    /// has significantly larger quantity, suggesting the smaller tick[i]
    /// replicated the larger tick[i+1] with higher priority.
    fn detect_frontruns(
        &self,
        prices: &[f64],
        quantities: &[f64],
        ticks: &[NormalizedTick],
        out: &mut Vec<MevCandidate>,
    ) {
        let n = prices.len();
        for i in 0..n.saturating_sub(1) {
            let j = i + 1;
            let p_i = prices[i];
            let p_j = prices[j];
            if p_i == 0.0 || p_j == 0.0 {
                continue;
            }

            let q_i = quantities[i];
            let q_j = quantities[j];

            // Price must be nearly identical (within 0.1% of each other).
            let price_diff = (p_i - p_j).abs() / p_j;
            if price_diff > 0.001 {
                continue;
            }

            // Follower tick must be significantly larger.
            if q_j < q_i * 3.0 || q_i == 0.0 {
                continue;
            }

            let estimated_profit = q_i * p_i * self.price_impact_threshold;
            // Confidence based on how much larger the second tick is.
            let ratio = (q_j / q_i).min(20.0);
            let confidence = (ratio / 20.0).clamp(0.1, 0.9);

            let tx_hash = ticks[i]
                .trade_id
                .clone()
                .unwrap_or_else(|| format!("tick-{i}"));

            out.push(MevCandidate {
                tx_hash,
                detected_pattern: MevPattern::Frontrun,
                estimated_profit_usd: estimated_profit,
                confidence,
                anchor_tick_index: i,
            });
        }
    }

    /// Detect backrun patterns.
    ///
    /// Heuristic: after a tick with a large upward price move (tick[i]),
    /// tick[i+1] or tick[i+2] sells at a slightly lower but still elevated
    /// price, capturing the spread.
    fn detect_backruns(
        &self,
        prices: &[f64],
        quantities: &[f64],
        ticks: &[NormalizedTick],
        out: &mut Vec<MevCandidate>,
    ) {
        let n = prices.len();
        for i in 1..n.saturating_sub(1) {
            let p_prev = prices[i - 1];
            let p_curr = prices[i];
            if p_prev == 0.0 {
                continue;
            }

            let buy_move = (p_curr - p_prev) / p_prev;
            if buy_move < self.price_impact_threshold {
                continue;
            }

            // Check up to 2 ticks ahead for a matching sell.
            for offset in 1..=2usize {
                let j = i + offset;
                if j >= n {
                    break;
                }
                let p_next = prices[j];
                if p_next == 0.0 {
                    continue;
                }

                // Sell should be slightly below the peak — still elevated.
                let sell_diff = (p_curr - p_next) / p_curr;
                if sell_diff >= 0.0 && sell_diff < buy_move {
                    let qty = quantities[j];
                    let estimated_profit = sell_diff * qty * p_next;
                    let confidence = Self::confidence_from_impact(buy_move, self.price_impact_threshold)
                        * (1.0 - sell_diff / buy_move);

                    let tx_hash = ticks[j]
                        .trade_id
                        .clone()
                        .unwrap_or_else(|| format!("tick-{j}"));

                    out.push(MevCandidate {
                        tx_hash,
                        detected_pattern: MevPattern::Backrun,
                        estimated_profit_usd: estimated_profit,
                        confidence: confidence.clamp(0.0, 1.0),
                        anchor_tick_index: j,
                    });
                    break;
                }
            }
        }
    }

    /// Compute a confidence score based on how far the observed impact exceeds
    /// the threshold. Saturates at 1.0 when impact is 10× the threshold.
    fn confidence_from_impact(impact: f64, threshold: f64) -> f64 {
        if threshold == 0.0 {
            return 0.0;
        }
        let ratio = impact / threshold;
        (ratio / 10.0).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn make_tick(price: &str, qty: &str, trade_id: Option<&str>) -> NormalizedTick {
        use crate::tick::Exchange;
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTC-USD".to_string(),
            price: Decimal::from_str(price).unwrap_or(Decimal::ZERO),
            quantity: Decimal::from_str(qty).unwrap_or(Decimal::ZERO),
            side: None,
            trade_id: trade_id.map(str::to_string),
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    #[test]
    fn too_few_ticks_returns_empty() {
        let det = MevDetector::new(0.005);
        let ticks = vec![make_tick("100", "1", None), make_tick("101", "1", None)];
        assert!(det.analyze_block(&ticks).is_empty());
    }

    #[test]
    fn sandwich_detected() {
        let det = MevDetector::with_window(0.005, 5);
        // Prices: 100 → 101.5 (+1.5%) → 101.5 → 100.2 (-1.3%)
        let ticks = vec![
            make_tick("100.00", "10", Some("tx1")),
            make_tick("101.50", "10", Some("tx2")), // large buy → victim
            make_tick("101.45", "1",  Some("tx3")), // victim tx
            make_tick("100.20", "10", Some("tx4")), // sell
        ];
        let candidates = det.analyze_block(&ticks);
        let sandwiches: Vec<_> = candidates
            .iter()
            .filter(|c| c.detected_pattern == MevPattern::Sandwich)
            .collect();
        assert!(
            !sandwiches.is_empty(),
            "expected sandwich detection, got: {:?}",
            candidates
        );
    }

    #[test]
    fn frontrun_detected() {
        let det = MevDetector::new(0.005);
        // Small tick immediately before a 5× larger tick at same price.
        let ticks = vec![
            make_tick("200.00", "1",  Some("front")), // small frontrun
            make_tick("200.00", "10", Some("victim")), // large victim
            make_tick("200.00", "1",  None),
        ];
        let candidates = det.analyze_block(&ticks);
        let frontruns: Vec<_> = candidates
            .iter()
            .filter(|c| c.detected_pattern == MevPattern::Frontrun)
            .collect();
        assert!(
            !frontruns.is_empty(),
            "expected frontrun detection, got: {:?}",
            candidates
        );
        assert_eq!(frontruns[0].tx_hash, "front");
    }

    #[test]
    fn backrun_detected() {
        let det = MevDetector::new(0.005);
        // Large upward move, then immediate sell at slightly lower elevated price.
        let ticks = vec![
            make_tick("300.00", "1",  None),
            make_tick("303.00", "10", None), // big buy (+1%)
            make_tick("302.80", "5",  Some("backrun")), // sell at elevated price
        ];
        let candidates = det.analyze_block(&ticks);
        let backruns: Vec<_> = candidates
            .iter()
            .filter(|c| c.detected_pattern == MevPattern::Backrun)
            .collect();
        assert!(
            !backruns.is_empty(),
            "expected backrun detection, got: {:?}",
            candidates
        );
    }

    #[test]
    fn no_false_positive_on_flat_prices() {
        let det = MevDetector::new(0.005);
        let ticks: Vec<NormalizedTick> = (0..10)
            .map(|_| make_tick("100.00", "1", None))
            .collect();
        assert!(
            det.analyze_block(&ticks).is_empty(),
            "flat price series should produce no candidates"
        );
    }

    #[test]
    fn mev_pattern_display() {
        assert_eq!(MevPattern::Sandwich.to_string(), "Sandwich");
        assert_eq!(MevPattern::Frontrun.to_string(), "Frontrun");
        assert_eq!(MevPattern::Backrun.to_string(), "Backrun");
    }

    #[test]
    fn confidence_clamped_to_one() {
        // Massive impact should not exceed confidence 1.0.
        let c = MevDetector::confidence_from_impact(1.0, 0.001);
        assert!(c <= 1.0);
    }

    #[test]
    fn price_impact_threshold_clamped_above_zero() {
        let det = MevDetector::new(-1.0);
        assert!(det.price_impact_threshold > 0.0);
    }

    #[test]
    fn window_size_minimum_three() {
        let det = MevDetector::with_window(0.005, 0);
        assert_eq!(det.window_size, 3);
    }
}
