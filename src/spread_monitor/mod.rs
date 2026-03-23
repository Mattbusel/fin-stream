//! Bid-ask spread analytics and decomposition.
//!
//! Provides real-time spread monitoring, statistical summaries, decomposition
//! into adverse selection / inventory / order-processing components, and
//! effective/realized spread calculations.

use std::collections::VecDeque;

/// A single bid-ask spread observation.
#[derive(Debug, Clone)]
pub struct SpreadObservation {
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Absolute spread: ask - bid.
    pub spread: f64,
    /// Spread in basis points relative to mid: (ask - bid) / mid * 10_000.
    pub spread_bps: f64,
    /// Mid-price: (bid + ask) / 2.
    pub mid: f64,
}

/// Decomposition of bid-ask spread into economic components.
#[derive(Debug, Clone)]
pub struct SpreadDecomposition {
    /// Adverse selection component (informed trading cost).
    pub adverse_selection: f64,
    /// Inventory holding cost component.
    pub inventory_cost: f64,
    /// Order processing / administrative cost component.
    pub order_processing: f64,
    /// Total spread (sum of components).
    pub total_spread: f64,
}

/// Real-time bid-ask spread monitor with a sliding-window history.
#[derive(Debug)]
pub struct SpreadMonitor {
    /// Symbol being monitored.
    pub symbol: String,
    /// Rolling window of observations.
    pub observations: VecDeque<SpreadObservation>,
    /// Maximum number of observations to retain.
    pub window_size: usize,
}

impl SpreadMonitor {
    /// Create a new `SpreadMonitor`.
    pub fn new(symbol: &str, window_size: usize) -> Self {
        Self {
            symbol: symbol.to_string(),
            observations: VecDeque::with_capacity(window_size.max(1)),
            window_size: window_size.max(1),
        }
    }

    /// Record a new bid-ask pair and return the computed observation.
    pub fn add_observation(&mut self, bid: f64, ask: f64, timestamp_ms: u64) -> SpreadObservation {
        let spread = ask - bid;
        let mid = (bid + ask) / 2.0;
        let spread_bps = if mid > 0.0 { spread / mid * 10_000.0 } else { 0.0 };
        let obs = SpreadObservation { timestamp_ms, bid, ask, spread, spread_bps, mid };
        if self.observations.len() >= self.window_size {
            self.observations.pop_front();
        }
        self.observations.push_back(obs.clone());
        obs
    }

    /// Most recent observation, or `None` if empty.
    pub fn current_spread(&self) -> Option<&SpreadObservation> {
        self.observations.back()
    }

    /// Average spread in basis points over the window.
    pub fn avg_spread_bps(&self) -> f64 {
        if self.observations.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.observations.iter().map(|o| o.spread_bps).sum();
        sum / self.observations.len() as f64
    }

    /// Minimum spread in basis points over the window.
    pub fn min_spread_bps(&self) -> f64 {
        self.observations.iter().map(|o| o.spread_bps).fold(f64::INFINITY, f64::min)
    }

    /// Maximum spread in basis points over the window.
    pub fn max_spread_bps(&self) -> f64 {
        self.observations.iter().map(|o| o.spread_bps).fold(f64::NEG_INFINITY, f64::max)
    }

    /// Standard deviation of spread_bps over the window.
    pub fn spread_volatility(&self) -> f64 {
        let n = self.observations.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.avg_spread_bps();
        let var: f64 = self.observations.iter().map(|o| (o.spread_bps - mean).powi(2)).sum::<f64>()
            / (n - 1) as f64;
        var.sqrt()
    }

    /// Full statistical summary over the window.
    pub fn stats(&self) -> SpreadStats {
        let n = self.observations.len();
        let avg = self.avg_spread_bps();
        let min = self.min_spread_bps();
        let max = self.max_spread_bps();
        let std_dev = self.spread_volatility();

        // p95: sort a copy.
        let mut bps_vals: Vec<f64> = self.observations.iter().map(|o| o.spread_bps).collect();
        bps_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95 = if bps_vals.is_empty() {
            0.0
        } else {
            let idx = ((bps_vals.len() as f64 * 0.95) as usize).min(bps_vals.len() - 1);
            bps_vals[idx]
        };

        SpreadStats {
            avg_bps: avg,
            min_bps: min,
            max_bps: max,
            std_dev_bps: std_dev,
            p95_bps: p95,
            observations_count: n,
        }
    }
}

/// Decompose a total spread into its economic components.
///
/// - `adverse_selection` = `price_impact`
/// - `inventory_cost` = `inventory_cost_pct * total_spread`
/// - `order_processing` = remainder
pub fn decompose_spread(
    price_impact: f64,
    inventory_cost_pct: f64,
    total_spread: f64,
) -> SpreadDecomposition {
    let adverse_selection = price_impact;
    let inventory_cost = inventory_cost_pct * total_spread;
    let order_processing = total_spread - adverse_selection - inventory_cost;
    SpreadDecomposition { adverse_selection, inventory_cost, order_processing, total_spread }
}

/// Effective spread: twice the signed distance from fill to mid, in basis points.
///
/// `side`: "buy" or "sell". Returns a positive value for a cost to the trader.
pub fn effective_spread(fill_price: f64, mid: f64, side: &str) -> f64 {
    let signed = match side {
        "sell" => mid - fill_price,
        _      => fill_price - mid,  // "buy"
    };
    2.0 * signed.abs() / mid * 10_000.0
}

/// Realized spread: measures the market maker's profit after a short horizon.
///
/// For a buy: 2 * (fill_price - future_mid) / mid_at_fill * 10_000.
/// For a sell: 2 * (future_mid - fill_price) / mid_at_fill * 10_000.
pub fn realized_spread(
    fill_price: f64,
    future_mid: f64,
    side: &str,
    mid_at_fill: f64,
) -> f64 {
    if mid_at_fill <= 0.0 {
        return 0.0;
    }
    let signed = match side {
        "sell" => future_mid - fill_price,
        _      => fill_price - future_mid,  // "buy"
    };
    2.0 * signed / mid_at_fill * 10_000.0
}

/// Alert tracker for wide-spread conditions.
#[derive(Debug, Clone)]
pub struct SpreadAlerts {
    /// Threshold in basis points above which an alert fires.
    pub threshold_bps: f64,
    /// Number of alerts fired so far.
    pub alert_count: u64,
}

impl SpreadAlerts {
    /// Check if an observation exceeds the threshold. Increments `alert_count` if so.
    pub fn check_alerts(&mut self, obs: &SpreadObservation) -> bool {
        if obs.spread_bps > self.threshold_bps {
            self.alert_count += 1;
            true
        } else {
            false
        }
    }
}

/// Statistical summary of spread observations.
#[derive(Debug, Clone)]
pub struct SpreadStats {
    /// Mean spread in bps.
    pub avg_bps: f64,
    /// Minimum spread in bps.
    pub min_bps: f64,
    /// Maximum spread in bps.
    pub max_bps: f64,
    /// Standard deviation of spread in bps.
    pub std_dev_bps: f64,
    /// 95th-percentile spread in bps.
    pub p95_bps: f64,
    /// Number of observations in the window.
    pub observations_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spread_calculation() {
        let mut monitor = SpreadMonitor::new("AAPL", 100);
        let obs = monitor.add_observation(99.0, 101.0, 1000);
        assert!((obs.spread - 2.0).abs() < 1e-9);
        assert!((obs.mid - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_spread_bps_formula() {
        let mut monitor = SpreadMonitor::new("TEST", 100);
        // bid=99, ask=101 => spread=2, mid=100 => bps = 2/100 * 10000 = 200 bps.
        let obs = monitor.add_observation(99.0, 101.0, 1000);
        assert!((obs.spread_bps - 200.0).abs() < 1e-6, "Expected 200 bps, got {}", obs.spread_bps);
    }

    #[test]
    fn test_avg_over_window() {
        let mut monitor = SpreadMonitor::new("X", 10);
        // 100 bps observation.
        monitor.add_observation(99.5, 100.5, 1000);  // spread=1, mid=100 => 100 bps
        // 200 bps observation.
        monitor.add_observation(99.0, 101.0, 2000);  // spread=2, mid=100 => 200 bps
        let avg = monitor.avg_spread_bps();
        assert!((avg - 150.0).abs() < 0.1, "Expected avg ~150 bps, got {avg}");
    }

    #[test]
    fn test_effective_spread_sign() {
        // Buy at 100.5 when mid is 100: effective spread = 2*(0.5)/100*10000 = 100 bps.
        let es = effective_spread(100.5, 100.0, "buy");
        assert!(es > 0.0, "Effective spread should be positive");
        assert!((es - 100.0).abs() < 1e-6, "Expected 100 bps, got {es}");
    }

    #[test]
    fn test_decomposition_sums_to_total() {
        let d = decompose_spread(5.0, 0.3, 20.0);
        let sum = d.adverse_selection + d.inventory_cost + d.order_processing;
        assert!((sum - d.total_spread).abs() < 1e-9, "Components should sum to total: {sum} vs {}", d.total_spread);
    }

    #[test]
    fn test_spread_volatility_positive() {
        let mut monitor = SpreadMonitor::new("VOL", 100);
        monitor.add_observation(99.0, 101.0, 1000);   // 200 bps
        monitor.add_observation(99.5, 100.5, 2000);   // 100 bps
        monitor.add_observation(98.0, 102.0, 3000);   // 400 bps
        let vol = monitor.spread_volatility();
        assert!(vol > 0.0, "Spread volatility should be positive, got {vol}");
    }

    #[test]
    fn test_window_eviction() {
        let mut monitor = SpreadMonitor::new("EVT", 3);
        for i in 0..5u64 {
            monitor.add_observation(99.0, 101.0, i * 1000);
        }
        assert_eq!(monitor.observations.len(), 3, "Window should cap at 3");
    }

    #[test]
    fn test_alert_fires_on_wide_spread() {
        let mut alerts = SpreadAlerts { threshold_bps: 100.0, alert_count: 0 };
        let mut monitor = SpreadMonitor::new("ALT", 10);
        let narrow = monitor.add_observation(99.9, 100.1, 1000);  // 20 bps
        let wide   = monitor.add_observation(98.0, 102.0, 2000);  // 400 bps
        assert!(!alerts.check_alerts(&narrow));
        assert!(alerts.check_alerts(&wide));
        assert_eq!(alerts.alert_count, 1);
    }

    #[test]
    fn test_realized_spread() {
        // Buy at 100.5, future mid 100.0, mid at fill 100.0.
        // realized = 2*(100.5 - 100.0)/100.0 * 10000 = 100 bps.
        let rs = realized_spread(100.5, 100.0, "buy", 100.0);
        assert!((rs - 100.0).abs() < 1e-6, "Expected 100 bps, got {rs}");
    }
}
