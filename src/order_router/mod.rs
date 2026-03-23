//! Smart order routing (SOR).
//!
//! Provides venue registry, multi-strategy routing logic (best price, best fill
//! rate, lowest fee, TWAP/VWAP slicing, and proportional splits), and a
//! concurrent [`SmartOrderRouter`] backed by [`dashmap::DashMap`].

use dashmap::DashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

// ─────────────────────────────────────────────────────────────────────────────
//  Side / RoutingStrategy
// ─────────────────────────────────────────────────────────────────────────────

/// Direction of a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Buy / long entry.
    Buy,
    /// Sell / short entry.
    Sell,
}

/// Routing algorithm to apply to an order.
#[derive(Debug, Clone)]
pub enum RoutingStrategy {
    /// Send entire order to the venue with the best quoted price.
    BestPrice,
    /// Send entire order to the venue with the highest fill rate.
    BestFillRate,
    /// Send entire order to the venue with the lowest total fee.
    LowestFee,
    /// Route to minimise expected market impact.
    MinImpact,
    /// Split order across the N best-scoring venues proportionally.
    SplitBestN(usize),
    /// Time-weighted average price: divide into equal time slices.
    TWAP {
        /// Number of equal-size time slices.
        slices: usize,
    },
    /// Volume-weighted average price: size slices by historical volume profile.
    VWAP {
        /// Fraction of daily volume per bucket (must sum to ≈ 1.0).
        volume_profile: Vec<f64>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
//  Venue
// ─────────────────────────────────────────────────────────────────────────────

/// A trading venue with execution quality characteristics.
#[derive(Debug, Clone)]
pub struct Venue {
    /// Unique venue identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Average bid-ask half-spread in basis points.
    pub avg_spread_bps: f64,
    /// Historical fill rate (0–1).
    pub fill_rate: f64,
    /// Average round-trip latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Exchange/broker fee in basis points per side.
    pub fee_bps: f64,
    /// Whether the venue is currently accepting orders.
    pub available: bool,
}

impl Venue {
    /// Composite execution quality score (higher is better).
    ///
    /// ```text
    /// score = fill_rate * (1 - fee_bps/10_000) / (avg_spread_bps + avg_latency_ms/1_000)
    /// ```
    pub fn execution_quality_score(&self) -> f64 {
        let denom = self.avg_spread_bps + self.avg_latency_ms / 1_000.0;
        if denom <= 0.0 {
            return 0.0;
        }
        self.fill_rate * (1.0 - self.fee_bps / 10_000.0) / denom
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Order types
// ─────────────────────────────────────────────────────────────────────────────

/// An order submitted to the smart order router.
#[derive(Debug, Clone)]
pub struct RoutingOrder {
    /// Unique order identifier (set by the router).
    pub id: u64,
    /// Instrument symbol.
    pub symbol: String,
    /// Buy or sell.
    pub side: Side,
    /// Total quantity to execute.
    pub total_quantity: f64,
    /// Optional price limit (`None` = market).
    pub limit_price: Option<f64>,
    /// Algorithm to apply.
    pub routing_strategy: RoutingStrategy,
}

/// A single slice of a routed order destined for one venue.
#[derive(Debug, Clone)]
pub struct OrderSlice {
    /// Destination venue identifier.
    pub venue_id: String,
    /// Quantity allocated to this slice.
    pub quantity: f64,
    /// Price limit for this slice (may differ from parent order).
    pub limit_price: Option<f64>,
    /// Sequence number within the parent order (0-based).
    pub slice_num: usize,
}

/// Routing result returned to the caller.
#[derive(Debug, Clone)]
pub struct RoutingResult {
    /// Parent order identifier.
    pub order_id: u64,
    /// Individual venue slices.
    pub slices: Vec<OrderSlice>,
    /// Estimated all-in cost in basis points.
    pub estimated_cost_bps: f64,
    /// Blended expected fill rate across all slices.
    pub estimated_fill_rate: f64,
    /// Total quantity routed (should equal `RoutingOrder::total_quantity`).
    pub total_quantity: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  RouterStats
// ─────────────────────────────────────────────────────────────────────────────

/// Aggregate statistics about the router's venue universe.
#[derive(Debug, Clone)]
pub struct RouterStats {
    /// Total number of registered venues.
    pub venue_count: usize,
    /// Number of venues currently marked as available.
    pub active_venues: usize,
    /// Average spread across active venues (bps).
    pub avg_spread_bps: f64,
    /// Average fill rate across active venues.
    pub avg_fill_rate: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  SmartOrderRouter
// ─────────────────────────────────────────────────────────────────────────────

/// Concurrent smart order router backed by a [`DashMap`] venue registry.
///
/// All mutation methods take `&self` (interior mutability via `DashMap` and
/// atomics) so the router can be shared across async tasks without a `Mutex`.
pub struct SmartOrderRouter {
    venues: DashMap<String, Venue>,
    next_id: Arc<AtomicU64>,
}

impl SmartOrderRouter {
    /// Create a new router with an empty venue registry.
    pub fn new() -> Self {
        Self {
            venues: DashMap::new(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Register or overwrite a venue.
    pub fn add_venue(&self, venue: Venue) {
        self.venues.insert(venue.id.clone(), venue);
    }

    /// Deregister a venue by ID.
    pub fn remove_venue(&self, id: &str) {
        self.venues.remove(id);
    }

    /// Update live execution statistics for a venue.
    pub fn update_venue_stats(&self, id: &str, spread_bps: f64, fill_rate: f64) {
        if let Some(mut entry) = self.venues.get_mut(id) {
            entry.avg_spread_bps = spread_bps;
            entry.fill_rate = fill_rate.clamp(0.0, 1.0);
        }
    }

    /// Route an order according to its [`RoutingStrategy`].
    ///
    /// Assigns a monotonically increasing `id` to the order before routing.
    pub fn route(&self, mut order: RoutingOrder) -> RoutingResult {
        order.id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let available: Vec<Venue> = self
            .venues
            .iter()
            .filter(|e| e.available)
            .map(|e| e.value().clone())
            .collect();

        if available.is_empty() {
            return RoutingResult {
                order_id: order.id,
                slices: Vec::new(),
                estimated_cost_bps: 0.0,
                estimated_fill_rate: 0.0,
                total_quantity: order.total_quantity,
            };
        }

        let slices = match &order.routing_strategy {
            RoutingStrategy::BestPrice | RoutingStrategy::MinImpact => {
                // Select venue with smallest spread.
                let best = available
                    .iter()
                    .min_by(|a, b| {
                        a.avg_spread_bps
                            .partial_cmp(&b.avg_spread_bps)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|v| v.id.clone())
                    .unwrap_or_default();
                vec![OrderSlice {
                    venue_id: best,
                    quantity: order.total_quantity,
                    limit_price: order.limit_price,
                    slice_num: 0,
                }]
            }
            RoutingStrategy::BestFillRate => {
                let best = available
                    .iter()
                    .max_by(|a, b| {
                        a.fill_rate
                            .partial_cmp(&b.fill_rate)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|v| v.id.clone())
                    .unwrap_or_default();
                vec![OrderSlice {
                    venue_id: best,
                    quantity: order.total_quantity,
                    limit_price: order.limit_price,
                    slice_num: 0,
                }]
            }
            RoutingStrategy::LowestFee => {
                let best = available
                    .iter()
                    .min_by(|a, b| {
                        a.fee_bps
                            .partial_cmp(&b.fee_bps)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|v| v.id.clone())
                    .unwrap_or_default();
                vec![OrderSlice {
                    venue_id: best,
                    quantity: order.total_quantity,
                    limit_price: order.limit_price,
                    slice_num: 0,
                }]
            }
            RoutingStrategy::SplitBestN(n) => {
                let splits = Self::split_order(order.total_quantity, &available, *n);
                splits
                    .into_iter()
                    .enumerate()
                    .map(|(i, (vid, qty))| OrderSlice {
                        venue_id: vid,
                        quantity: qty,
                        limit_price: order.limit_price,
                        slice_num: i,
                    })
                    .collect()
            }
            RoutingStrategy::TWAP { slices } => {
                let venue_id = available
                    .iter()
                    .max_by(|a, b| {
                        a.execution_quality_score()
                            .partial_cmp(&b.execution_quality_score())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|v| v.id.clone())
                    .unwrap_or_default();
                Self::twap_slices(order.total_quantity, *slices, &venue_id)
            }
            RoutingStrategy::VWAP { volume_profile } => {
                let venue_id = available
                    .iter()
                    .max_by(|a, b| {
                        a.execution_quality_score()
                            .partial_cmp(&b.execution_quality_score())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|v| v.id.clone())
                    .unwrap_or_default();
                Self::vwap_slices(order.total_quantity, volume_profile, &venue_id)
            }
        };

        // Compute blended stats.
        let total_qty: f64 = slices.iter().map(|s| s.quantity).sum();
        let estimated_cost_bps = available
            .iter()
            .map(|v| v.avg_spread_bps + v.fee_bps)
            .sum::<f64>()
            / available.len().max(1) as f64;
        let estimated_fill_rate = available.iter().map(|v| v.fill_rate).sum::<f64>()
            / available.len().max(1) as f64;

        RoutingResult {
            order_id: order.id,
            slices,
            estimated_cost_bps,
            estimated_fill_rate,
            total_quantity: total_qty,
        }
    }

    /// Return the available venue with the highest execution quality score that
    /// meets the minimum fill rate requirement.
    pub fn best_venue(&self, min_fill_rate: f64) -> Option<Venue> {
        self.venues
            .iter()
            .filter(|e| e.available && e.fill_rate >= min_fill_rate)
            .max_by(|a, b| {
                a.execution_quality_score()
                    .partial_cmp(&b.execution_quality_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|e| e.value().clone())
    }

    /// Split `quantity` across the `n` best-scoring venues, proportional to
    /// their execution quality scores.
    pub fn split_order(quantity: f64, venues: &[Venue], n: usize) -> Vec<(String, f64)> {
        let n = n.min(venues.len());
        if n == 0 || quantity <= 0.0 {
            return Vec::new();
        }

        // Sort by quality descending, take top n.
        let mut sorted: Vec<&Venue> = venues.iter().collect();
        sorted.sort_by(|a, b| {
            b.execution_quality_score()
                .partial_cmp(&a.execution_quality_score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let top_n = &sorted[..n];

        let total_score: f64 = top_n.iter().map(|v| v.execution_quality_score()).sum();
        if total_score <= 0.0 {
            // Equal split as fallback.
            let per = quantity / n as f64;
            return top_n
                .iter()
                .map(|v| (v.id.clone(), per))
                .collect();
        }

        top_n
            .iter()
            .map(|v| {
                let frac = v.execution_quality_score() / total_score;
                (v.id.clone(), quantity * frac)
            })
            .collect()
    }

    /// Generate TWAP slices (equal size, single venue).
    pub fn twap_slices(quantity: f64, n_slices: usize, venue_id: &str) -> Vec<OrderSlice> {
        let n = n_slices.max(1);
        let per = quantity / n as f64;
        (0..n)
            .map(|i| OrderSlice {
                venue_id: venue_id.to_string(),
                quantity: per,
                limit_price: None,
                slice_num: i,
            })
            .collect()
    }

    /// Generate VWAP slices (quantity proportional to volume profile, single venue).
    pub fn vwap_slices(quantity: f64, volume_profile: &[f64], venue_id: &str) -> Vec<OrderSlice> {
        if volume_profile.is_empty() {
            return vec![OrderSlice {
                venue_id: venue_id.to_string(),
                quantity,
                limit_price: None,
                slice_num: 0,
            }];
        }
        let total: f64 = volume_profile.iter().sum();
        let total = if total <= 0.0 { 1.0 } else { total };
        volume_profile
            .iter()
            .enumerate()
            .map(|(i, &frac)| OrderSlice {
                venue_id: venue_id.to_string(),
                quantity: quantity * frac / total,
                limit_price: None,
                slice_num: i,
            })
            .collect()
    }

    /// Aggregate statistics over the current venue universe.
    pub fn router_stats(&self) -> RouterStats {
        let venue_count = self.venues.len();
        let active: Vec<_> = self
            .venues
            .iter()
            .filter(|e| e.available)
            .map(|e| e.value().clone())
            .collect();
        let active_venues = active.len();
        let avg_spread_bps = if active_venues > 0 {
            active.iter().map(|v| v.avg_spread_bps).sum::<f64>() / active_venues as f64
        } else {
            0.0
        };
        let avg_fill_rate = if active_venues > 0 {
            active.iter().map(|v| v.fill_rate).sum::<f64>() / active_venues as f64
        } else {
            0.0
        };
        RouterStats {
            venue_count,
            active_venues,
            avg_spread_bps,
            avg_fill_rate,
        }
    }
}

impl Default for SmartOrderRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_venue(id: &str, spread: f64, fill: f64, latency: f64, fee: f64) -> Venue {
        Venue {
            id: id.to_string(),
            name: id.to_string(),
            avg_spread_bps: spread,
            fill_rate: fill,
            avg_latency_ms: latency,
            fee_bps: fee,
            available: true,
        }
    }

    #[test]
    fn test_add_and_route_best_price() {
        let router = SmartOrderRouter::new();
        router.add_venue(make_venue("NYSE", 2.0, 0.95, 10.0, 0.5));
        router.add_venue(make_venue("NASDAQ", 1.5, 0.90, 8.0, 0.4));

        let order = RoutingOrder {
            id: 0,
            symbol: "AAPL".into(),
            side: Side::Buy,
            total_quantity: 1_000.0,
            limit_price: Some(180.0),
            routing_strategy: RoutingStrategy::BestPrice,
        };

        let result = router.route(order);
        assert_eq!(result.slices.len(), 1);
        assert_eq!(result.slices[0].venue_id, "NASDAQ"); // smallest spread
        assert!((result.total_quantity - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_split_best_n() {
        let venues = vec![
            make_venue("A", 2.0, 0.90, 10.0, 0.5),
            make_venue("B", 1.5, 0.95, 5.0, 0.3),
            make_venue("C", 3.0, 0.80, 15.0, 0.6),
        ];
        let splits = SmartOrderRouter::split_order(1_000.0, &venues, 2);
        assert_eq!(splits.len(), 2);
        let total: f64 = splits.iter().map(|(_, q)| q).sum();
        assert!((total - 1_000.0).abs() < 1e-6);
    }

    #[test]
    fn test_twap_slices() {
        let slices = SmartOrderRouter::twap_slices(600.0, 3, "NYSE");
        assert_eq!(slices.len(), 3);
        for s in &slices {
            assert!((s.quantity - 200.0).abs() < 1e-9);
        }
    }

    #[test]
    fn test_vwap_slices() {
        let profile = vec![0.3, 0.4, 0.3];
        let slices = SmartOrderRouter::vwap_slices(1_000.0, &profile, "NYSE");
        assert_eq!(slices.len(), 3);
        let total: f64 = slices.iter().map(|s| s.quantity).sum();
        assert!((total - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_router_stats() {
        let router = SmartOrderRouter::new();
        router.add_venue(make_venue("A", 2.0, 0.90, 10.0, 0.5));
        router.add_venue(make_venue("B", 3.0, 0.80, 12.0, 0.6));
        let stats = router.router_stats();
        assert_eq!(stats.venue_count, 2);
        assert_eq!(stats.active_venues, 2);
    }
}
