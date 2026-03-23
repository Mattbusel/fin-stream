//! # Module: venue_selector
//!
//! ## Responsibility
//! Smart order routing: score venues by fill rate, fees, and latency;
//! select the best single venue; split large orders across multiple venues;
//! and track historical venue performance with exponential decay weighting.
//!
//! ## Guarantees
//! - No panics on the selection path; returns `StreamError` for empty inputs
//! - Exponential decay weights are applied on every `record_outcome` call
//! - Order splits are proportional to adjusted venue scores

use crate::error::StreamError;
use std::collections::HashMap;

/// Unique venue identifier (string alias for clarity).
pub type VenueId = String;

// ─────────────────────────────────────────
//  Venue
// ─────────────────────────────────────────

/// A trading venue with static characteristics.
///
/// # Example
/// ```rust
/// use fin_stream::venue_selector::Venue;
///
/// let v = Venue {
///     id: "NYSE".into(),
///     name: "New York Stock Exchange".into(),
///     fee_bps: 3.0,
///     latency_ms: 1,
///     fill_rate: 0.95,
/// };
/// let score = v.score();
/// assert!(score > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct Venue {
    /// Unique venue identifier.
    pub id: VenueId,
    /// Human-readable venue name.
    pub name: String,
    /// Trading fee in basis points (lower is better).
    pub fee_bps: f64,
    /// Median order-to-fill latency in milliseconds (lower is better).
    pub latency_ms: u64,
    /// Historical fill rate `[0.0, 1.0]` (higher is better).
    pub fill_rate: f64,
}

impl Venue {
    /// Composite venue score.
    ///
    /// `score = fill_rate * 0.4 + (1/fee_bps) * 0.3 + (1/latency_ms) * 0.3`
    ///
    /// Returns `0.0` if `fee_bps <= 0` or `latency_ms == 0`.
    #[must_use]
    pub fn score(&self) -> f64 {
        if self.fee_bps <= 0.0 || self.latency_ms == 0 {
            return 0.0;
        }
        self.fill_rate * 0.4
            + (1.0 / self.fee_bps) * 0.3
            + (1.0 / self.latency_ms as f64) * 0.3
    }
}

// ─────────────────────────────────────────
//  VenueScore
// ─────────────────────────────────────────

/// Derived composite score for a venue.
#[derive(Debug, Clone)]
pub struct VenueScore {
    /// Venue reference identifier.
    pub venue_id: VenueId,
    /// Composite score (higher = preferred).
    pub score: f64,
}

impl VenueScore {
    /// Compute the [`VenueScore`] from a [`Venue`].
    #[must_use]
    pub fn from_venue(venue: &Venue) -> Self {
        Self {
            venue_id: venue.id.clone(),
            score: venue.score(),
        }
    }
}

// ─────────────────────────────────────────
//  RoutingDecision
// ─────────────────────────────────────────

/// The output of the venue selector: where and how to route an order.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// The single best venue (or the largest slice for split orders).
    pub primary_venue: VenueId,
    /// For large orders: `(venue_id, fraction_of_order)` pairs summing to `≤ 1.0`.
    /// Empty when the order is routed entirely to the primary venue.
    pub split_orders: Vec<(VenueId, f64)>,
    /// Estimated weighted-average cost in basis points.
    pub estimated_cost_bps: f64,
}

// ─────────────────────────────────────────
//  PerformanceRecord (internal)
// ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct PerformanceRecord {
    decayed_fill_rate: f64,
    decayed_latency_ms: f64,
    count: u64,
}

impl PerformanceRecord {
    fn new(fill_rate: f64, latency_ms: f64) -> Self {
        Self {
            decayed_fill_rate: fill_rate,
            decayed_latency_ms: latency_ms,
            count: 1,
        }
    }

    fn update(&mut self, fill_rate: f64, latency_ms: f64, decay: f64) {
        self.decayed_fill_rate = decay * fill_rate + (1.0 - decay) * self.decayed_fill_rate;
        self.decayed_latency_ms =
            decay * latency_ms + (1.0 - decay) * self.decayed_latency_ms;
        self.count += 1;
    }
}

// ─────────────────────────────────────────
//  VenueSelector
// ─────────────────────────────────────────

/// Smart order router with historical performance tracking.
///
/// # Example
/// ```rust
/// use fin_stream::venue_selector::{Venue, VenueSelector};
///
/// let venues = vec![
///     Venue { id: "A".into(), name: "Alpha".into(), fee_bps: 3.0, latency_ms: 2, fill_rate: 0.95 },
///     Venue { id: "B".into(), name: "Beta".into(),  fee_bps: 5.0, latency_ms: 1, fill_rate: 0.90 },
/// ];
/// let mut selector = VenueSelector::new(0.1, 10_000.0, 3);
/// let best = selector.select_best(100.0, &venues).unwrap();
/// assert!(!best.id.is_empty());
/// ```
#[derive(Debug)]
pub struct VenueSelector {
    /// Exponential decay factor `(0, 1]` for historical performance updates.
    decay: f64,
    /// Orders at or above this size trigger splitting across multiple venues.
    split_threshold: f64,
    /// Maximum number of venues to split across.
    max_split_venues: usize,
    /// Per-venue historical performance records, keyed by venue id.
    history: HashMap<VenueId, PerformanceRecord>,
}

impl VenueSelector {
    /// Constructs a [`VenueSelector`].
    ///
    /// - `decay`: exponential decay `(0, 1]`; `0.1` = slow adaptation, `0.9` = fast.
    /// - `split_threshold`: order size above which splitting is applied.
    /// - `max_split_venues`: maximum venues to split across (clamped to `>= 1`).
    #[must_use]
    pub fn new(decay: f64, split_threshold: f64, max_split_venues: usize) -> Self {
        Self {
            decay: decay.clamp(1e-9, 1.0),
            split_threshold,
            max_split_venues: max_split_venues.max(1),
            history: HashMap::new(),
        }
    }

    /// Select the single best venue for the given order size.
    ///
    /// Uses historical adjusted scores when available, falls back to static scores.
    ///
    /// # Errors
    /// [`StreamError::InvalidInput`] if `venues` is empty.
    pub fn select_best<'a>(
        &self,
        _order_size: f64,
        venues: &'a [Venue],
    ) -> Result<&'a Venue, StreamError> {
        if venues.is_empty() {
            return Err(StreamError::InvalidInput(
                "venue list must not be empty".into(),
            ));
        }
        venues
            .iter()
            .max_by(|a, b| {
                let sa = self.adjusted_score(a);
                let sb = self.adjusted_score(b);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or_else(|| StreamError::InvalidInput("no venue could be scored".into()))
    }

    /// Route an order, splitting across multiple venues for large orders.
    ///
    /// Orders below `split_threshold` go entirely to the best venue.
    /// Larger orders are split proportionally by venue score across up to
    /// `max_split_venues` venues.
    ///
    /// # Errors
    /// [`StreamError::InvalidInput`] if `venues` is empty.
    pub fn route(
        &self,
        order_size: f64,
        venues: &[Venue],
    ) -> Result<RoutingDecision, StreamError> {
        if venues.is_empty() {
            return Err(StreamError::InvalidInput(
                "venue list must not be empty".into(),
            ));
        }
        if order_size < self.split_threshold || venues.len() == 1 {
            let best = self.select_best(order_size, venues)?;
            return Ok(RoutingDecision {
                primary_venue: best.id.clone(),
                split_orders: vec![],
                estimated_cost_bps: best.fee_bps,
            });
        }
        // Sort venues by adjusted score descending, take top N
        let mut scored: Vec<(&Venue, f64)> = venues
            .iter()
            .map(|v| (v, self.adjusted_score(v)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(self.max_split_venues);

        let total_score: f64 = scored.iter().map(|(_, s)| s).sum();
        let splits: Vec<(VenueId, f64)> = if total_score <= 0.0 {
            scored
                .iter()
                .map(|(v, _)| (v.id.clone(), 1.0 / scored.len() as f64))
                .collect()
        } else {
            scored
                .iter()
                .map(|(v, s)| (v.id.clone(), s / total_score))
                .collect()
        };

        // Weighted cost estimate
        let estimated_cost_bps: f64 = scored
            .iter()
            .map(|(v, s)| v.fee_bps * (s / total_score.max(f64::EPSILON)))
            .sum();

        let primary_venue = splits
            .first()
            .map(|(id, _)| id.clone())
            .unwrap_or_default();

        Ok(RoutingDecision {
            primary_venue,
            split_orders: splits,
            estimated_cost_bps,
        })
    }

    /// Record an observed outcome for a venue to update historical performance.
    ///
    /// - `venue_id`: the venue that executed.
    /// - `fill_rate`: observed fill rate for this execution `[0.0, 1.0]`.
    /// - `latency_ms`: observed end-to-end latency in milliseconds.
    pub fn record_outcome(&mut self, venue_id: &str, fill_rate: f64, latency_ms: f64) {
        let decay = self.decay;
        self.history
            .entry(venue_id.to_string())
            .and_modify(|r| r.update(fill_rate, latency_ms, decay))
            .or_insert_with(|| PerformanceRecord::new(fill_rate, latency_ms));
    }

    /// Compute the adjusted score for a venue, blending static and historical data.
    fn adjusted_score(&self, venue: &Venue) -> f64 {
        let static_score = venue.score();
        if let Some(record) = self.history.get(&venue.id) {
            let fill = record.decayed_fill_rate;
            let lat = record.decayed_latency_ms;
            if lat > 0.0 && venue.fee_bps > 0.0 {
                return fill * 0.4 + (1.0 / venue.fee_bps) * 0.3 + (1.0 / lat) * 0.3;
            }
        }
        static_score
    }

    /// Returns the number of venues with recorded history.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}

// ─────────────────────────────────────────
//  Unit Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_venue(id: &str, fee_bps: f64, latency_ms: u64, fill_rate: f64) -> Venue {
        Venue {
            id: id.into(),
            name: id.into(),
            fee_bps,
            latency_ms,
            fill_rate,
        }
    }

    #[test]
    fn venue_score_positive() {
        let v = make_venue("A", 3.0, 2, 0.95);
        assert!(v.score() > 0.0);
    }

    #[test]
    fn venue_score_zero_fee() {
        let v = make_venue("A", 0.0, 2, 0.95);
        assert_eq!(v.score(), 0.0);
    }

    #[test]
    fn select_best_picks_highest_score() {
        let venues = vec![
            make_venue("cheap", 1.0, 5, 0.95),
            make_venue("fast", 10.0, 1, 0.95),
            make_venue("balanced", 3.0, 3, 0.95),
        ];
        let sel = VenueSelector::new(0.1, 10_000.0, 3);
        let best = sel.select_best(100.0, &venues).unwrap();
        // cheap has fee_bps=1 → highest fee score; should win
        assert_eq!(best.id, "cheap");
    }

    #[test]
    fn select_best_empty_venues_errors() {
        let sel = VenueSelector::new(0.1, 10_000.0, 3);
        assert!(sel.select_best(100.0, &[]).is_err());
    }

    #[test]
    fn route_small_order_no_split() {
        let venues = vec![make_venue("A", 3.0, 2, 0.95)];
        let sel = VenueSelector::new(0.1, 10_000.0, 3);
        let decision = sel.route(100.0, &venues).unwrap();
        assert_eq!(decision.primary_venue, "A");
        assert!(decision.split_orders.is_empty());
    }

    #[test]
    fn route_large_order_splits() {
        let venues = vec![
            make_venue("A", 3.0, 2, 0.95),
            make_venue("B", 5.0, 1, 0.90),
            make_venue("C", 2.0, 3, 0.85),
        ];
        let sel = VenueSelector::new(0.1, 500.0, 3);
        let decision = sel.route(1_000.0, &venues).unwrap();
        assert!(!decision.split_orders.is_empty());
        let total: f64 = decision.split_orders.iter().map(|(_, f)| f).sum();
        assert!((total - 1.0).abs() < 1e-10);
    }

    #[test]
    fn record_outcome_updates_history() {
        let venues = vec![make_venue("A", 3.0, 2, 0.95)];
        let mut sel = VenueSelector::new(0.5, 10_000.0, 3);
        sel.record_outcome("A", 0.80, 5.0);
        assert_eq!(sel.history_len(), 1);
        // Select again — should still work with history
        let best = sel.select_best(100.0, &venues).unwrap();
        assert_eq!(best.id, "A");
    }

    #[test]
    fn venue_score_score_calculation() {
        // fee_bps=10, latency=10, fill_rate=0.5
        // score = 0.5*0.4 + (1/10)*0.3 + (1/10)*0.3 = 0.2 + 0.03 + 0.03 = 0.26
        let v = make_venue("X", 10.0, 10, 0.5);
        assert!((v.score() - 0.26).abs() < 1e-10);
    }
}
