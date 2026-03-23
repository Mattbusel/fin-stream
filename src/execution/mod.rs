//! Execution quality monitor.
//!
//! ## Responsibility
//!
//! Measure the true cost of order execution by decomposing the gap between
//! the **decision price** (price when the order was decided) and the
//! **final fill price** into its constituent components.
//!
//! ## Metrics
//!
//! ### Implementation Shortfall (IS)
//!
//! The gold-standard execution quality metric (Perold 1988):
//!
//! ```text
//! IS = (avg_fill_price - decision_price) × signed_qty / decision_price
//! ```
//!
//! Positive IS for a buy means you paid more than the decision price (worse).
//!
//! ### Market Impact
//!
//! The price move that your order *caused*:
//!
//! ```text
//! market_impact = (arrival_price - decision_price) × sign(qty)
//! ```
//!
//! where `arrival_price` is the mid-price when the first child order hit the
//! market.
//!
//! ### Arrival Price vs VWAP
//!
//! ```text
//! arrival_vs_vwap = (avg_fill - arrival_price) × sign(qty) / arrival_price
//! ```
//!
//! Negative means you traded better than VWAP.
//!
//! ### Slippage Decomposition
//!
//! ```text
//! temporary_impact ≈ IS - permanent_impact
//! permanent_impact ≈ (post_trade_mid - arrival_price) × sign(qty) / arrival_price
//! ```
//!
//! ## Usage
//!
//! ```rust
//! use fin_stream::execution::{ExecutionMonitor, OrderRecord, FillRecord};
//!
//! let order = OrderRecord::new("ord-1", 100.0, 1000.0, fin_stream::execution::Side::Buy);
//! let mut monitor = ExecutionMonitor::new();
//! monitor.record_order(order);
//! monitor.add_fill("ord-1", FillRecord { price: 100.05, quantity: 500.0, vwap_at_fill: 100.02 })
//!        .unwrap();
//! monitor.add_fill("ord-1", FillRecord { price: 100.08, quantity: 500.0, vwap_at_fill: 100.04 })
//!        .unwrap();
//! monitor.set_arrival_price("ord-1", 100.01).unwrap();
//! monitor.set_post_trade_mid("ord-1", 100.06).unwrap();
//!
//! let report = monitor.compute_report("ord-1").unwrap();
//! println!("{report:?}");
//! ```

use crate::error::StreamError;
use std::collections::HashMap;

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    /// Buy order.
    Buy,
    /// Sell order.
    Sell,
}

impl Side {
    /// +1 for Buy, -1 for Sell.
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// Metadata recorded when an order is submitted.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    /// Unique order identifier.
    pub order_id: String,
    /// Mid-price at decision time (before order submission).
    pub decision_price: f64,
    /// Total signed quantity (positive for buys, negative for sells internally).
    pub total_quantity: f64,
    /// Order side.
    pub side: Side,
    /// Mid-price when the first fill hit the market.
    pub arrival_price: Option<f64>,
    /// Mid-price after the last fill settled.
    pub post_trade_mid: Option<f64>,
}

impl OrderRecord {
    /// Construct an order record.
    pub fn new(order_id: impl Into<String>, decision_price: f64, total_quantity: f64, side: Side) -> Self {
        Self {
            order_id: order_id.into(),
            decision_price,
            total_quantity,
            side,
            arrival_price: None,
            post_trade_mid: None,
        }
    }
}

/// A single fill (partial execution) for an order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FillRecord {
    /// Fill price.
    pub price: f64,
    /// Quantity filled in this partial execution.
    pub quantity: f64,
    /// Market VWAP at the moment of this fill.
    pub vwap_at_fill: f64,
}

/// Execution quality report for a completed order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutionReport {
    /// Order identifier.
    pub order_id: String,
    /// Average fill price (quantity-weighted).
    pub avg_fill_price: f64,
    /// Quantity-weighted average VWAP across fills.
    pub avg_vwap: f64,
    /// Implementation shortfall as a fraction of decision price.
    /// Positive = worse than decision price for the direction.
    pub implementation_shortfall: f64,
    /// Market impact: price move caused by sending the order.
    pub market_impact: f64,
    /// Arrival price vs VWAP: signed relative difference.
    pub arrival_vs_vwap: f64,
    /// Permanent price impact (fraction of arrival price).
    pub permanent_impact: f64,
    /// Temporary (transient) price impact (fraction of arrival price).
    pub temporary_impact: f64,
    /// Total fills recorded.
    pub fill_count: usize,
    /// Total quantity filled.
    pub total_filled: f64,
}

// ── Monitor ───────────────────────────────────────────────────────────────────

/// Streaming execution quality monitor.
///
/// Accepts order records and fill records and computes execution quality
/// metrics on demand.
#[derive(Debug, Default)]
pub struct ExecutionMonitor {
    orders: HashMap<String, OrderRecord>,
    fills: HashMap<String, Vec<FillRecord>>,
}

impl ExecutionMonitor {
    /// Create a new monitor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new order.
    pub fn record_order(&mut self, order: OrderRecord) {
        self.fills.entry(order.order_id.clone()).or_default();
        self.orders.insert(order.order_id.clone(), order);
    }

    /// Append a fill for the given order.
    pub fn add_fill(&mut self, order_id: &str, fill: FillRecord) -> Result<(), StreamError> {
        if !self.orders.contains_key(order_id) {
            return Err(StreamError::InvalidInput(format!(
                "unknown order_id: {order_id}"
            )));
        }
        if fill.quantity <= 0.0 {
            return Err(StreamError::InvalidInput(
                "fill quantity must be positive".into(),
            ));
        }
        self.fills
            .entry(order_id.to_string())
            .or_default()
            .push(fill);
        Ok(())
    }

    /// Set the arrival price (mid when first child order reached the market).
    pub fn set_arrival_price(&mut self, order_id: &str, price: f64) -> Result<(), StreamError> {
        self.order_mut(order_id)?.arrival_price = Some(price);
        Ok(())
    }

    /// Set the post-trade mid-price (used to decompose permanent vs temporary impact).
    pub fn set_post_trade_mid(&mut self, order_id: &str, price: f64) -> Result<(), StreamError> {
        self.order_mut(order_id)?.post_trade_mid = Some(price);
        Ok(())
    }

    /// Compute the execution quality report for `order_id`.
    ///
    /// Requires at least one fill and a recorded decision price.
    pub fn compute_report(&self, order_id: &str) -> Result<ExecutionReport, StreamError> {
        let order = self.orders.get(order_id).ok_or_else(|| {
            StreamError::InvalidInput(format!("unknown order_id: {order_id}"))
        })?;
        let fills = self.fills.get(order_id).ok_or_else(|| {
            StreamError::InvalidInput(format!("no fills for order_id: {order_id}"))
        })?;
        if fills.is_empty() {
            return Err(StreamError::InvalidInput(
                "at least one fill is required".into(),
            ));
        }

        // Quantity-weighted averages.
        let total_qty: f64 = fills.iter().map(|f| f.quantity).sum();
        let avg_fill_price: f64 =
            fills.iter().map(|f| f.price * f.quantity).sum::<f64>() / total_qty;
        let avg_vwap: f64 =
            fills.iter().map(|f| f.vwap_at_fill * f.quantity).sum::<f64>() / total_qty;

        let s = order.side.sign();
        let dp = order.decision_price;

        // Implementation shortfall (fraction of decision price).
        let is = s * (avg_fill_price - dp) / dp;

        // Market impact (raw price units).
        let market_impact = order
            .arrival_price
            .map(|ap| s * (ap - dp))
            .unwrap_or(0.0);

        // Arrival vs VWAP.
        let arrival_vs_vwap = order
            .arrival_price
            .filter(|&ap| ap > 1e-12)
            .map(|ap| s * (avg_fill_price - ap) / ap)
            .unwrap_or(0.0);

        // Permanent impact (post-trade mid vs arrival).
        let permanent_impact = match (order.arrival_price, order.post_trade_mid) {
            (Some(ap), Some(pt)) if ap > 1e-12 => s * (pt - ap) / ap,
            _ => 0.0,
        };

        // Temporary impact = IS - permanent_impact (approximately).
        let temporary_impact = is - permanent_impact;

        Ok(ExecutionReport {
            order_id: order_id.to_string(),
            avg_fill_price,
            avg_vwap,
            implementation_shortfall: is,
            market_impact,
            arrival_vs_vwap,
            permanent_impact,
            temporary_impact,
            fill_count: fills.len(),
            total_filled: total_qty,
        })
    }

    fn order_mut(&mut self, order_id: &str) -> Result<&mut OrderRecord, StreamError> {
        self.orders.get_mut(order_id).ok_or_else(|| {
            StreamError::InvalidInput(format!("unknown order_id: {order_id}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> ExecutionMonitor {
        let mut m = ExecutionMonitor::new();
        m.record_order(OrderRecord::new("ord-1", 100.0, 1000.0, Side::Buy));
        m
    }

    #[test]
    fn basic_report_computed() {
        let mut m = setup();
        m.add_fill("ord-1", FillRecord { price: 100.05, quantity: 500.0, vwap_at_fill: 100.02 })
            .unwrap();
        m.add_fill("ord-1", FillRecord { price: 100.08, quantity: 500.0, vwap_at_fill: 100.04 })
            .unwrap();
        m.set_arrival_price("ord-1", 100.01).unwrap();
        m.set_post_trade_mid("ord-1", 100.06).unwrap();

        let r = m.compute_report("ord-1").unwrap();
        assert_eq!(r.fill_count, 2);
        assert!((r.avg_fill_price - 100.065).abs() < 1e-9);
        // IS should be positive (bought above decision price).
        assert!(r.implementation_shortfall > 0.0, "IS must be positive for above-decision buy");
    }

    #[test]
    fn sell_is_negative_when_filled_below_decision() {
        let mut m = ExecutionMonitor::new();
        m.record_order(OrderRecord::new("ord-s", 100.0, 500.0, Side::Sell));
        // Sold below decision price → IS negative (bad for seller).
        m.add_fill("ord-s", FillRecord { price: 99.9, quantity: 500.0, vwap_at_fill: 100.0 })
            .unwrap();
        let r = m.compute_report("ord-s").unwrap();
        assert!(r.implementation_shortfall < 0.0);
    }

    #[test]
    fn unknown_order_returns_error() {
        let m = ExecutionMonitor::new();
        assert!(m.compute_report("ghost").is_err());
    }

    #[test]
    fn no_fills_returns_error() {
        let m = setup();
        assert!(m.compute_report("ord-1").is_err());
    }

    #[test]
    fn zero_quantity_fill_rejected() {
        let mut m = setup();
        let result = m.add_fill(
            "ord-1",
            FillRecord { price: 100.0, quantity: 0.0, vwap_at_fill: 100.0 },
        );
        assert!(result.is_err());
    }
}
