//! # Module: alert
//!
//! Price and volume alert system with configurable conditions and severity levels.
//!
//! ## Key Types
//!
//! - [`AlertCondition`] — threshold, percentage-change, volume-spike, Bollinger, or crossover
//! - [`AlertSeverity`] — Info / Warning / Critical
//! - [`Alert`] — a configured alert with metadata and trigger state
//! - [`AlertManager`] — register, update, query, and remove alerts

use std::collections::{HashMap, VecDeque};

// ─────────────────────────────────────────
//  AlertCondition
// ─────────────────────────────────────────

/// Condition that triggers an alert when satisfied.
#[derive(Debug, Clone)]
pub enum AlertCondition {
    /// Triggers when price rises above the given threshold.
    PriceAbove {
        /// Price level that must be exceeded.
        threshold: f64,
    },
    /// Triggers when price falls below the given threshold.
    PriceBelow {
        /// Price level that must be breached downward.
        threshold: f64,
    },
    /// Triggers when the price changes by at least `pct` percent within `window_ms`.
    /// `direction`: +1 = upward move only, -1 = downward move only, 0 = either direction.
    PriceChangePct {
        /// Minimum absolute percentage change required (e.g. `5.0` = 5%).
        pct: f64,
        /// Required direction: +1 upward, -1 downward, 0 either.
        direction: i8,
        /// Rolling lookback window in milliseconds.
        window_ms: u64,
    },
    /// Triggers when volume exceeds `multiple` times the rolling average over `window_ms`.
    VolumeSpike {
        /// How many times the average volume must be exceeded.
        multiple: f64,
        /// Rolling lookback window in milliseconds.
        window_ms: u64,
    },
    /// Triggers when price moves outside N standard deviations of the rolling `window`-bar mean.
    BollingerBreakout {
        /// Number of standard deviations from the mean to define the band boundary.
        std_threshold: f64,
        /// Number of recent prices used to compute the mean and standard deviation.
        window: usize,
    },
    /// Triggers when the fast moving average crosses the slow moving average.
    CrossOver {
        /// Number of prices in the fast moving average.
        fast_window: usize,
        /// Number of prices in the slow moving average.
        slow_window: usize,
    },
}

// ─────────────────────────────────────────
//  AlertSeverity
// ─────────────────────────────────────────

/// How severe the alert is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertSeverity {
    /// Informational — no immediate action required.
    Info,
    /// Warning — monitor closely.
    Warning,
    /// Critical — immediate action may be required.
    Critical,
}

// ─────────────────────────────────────────
//  Alert
// ─────────────────────────────────────────

/// A configured alert, possibly triggered.
#[derive(Debug, Clone)]
pub struct Alert {
    /// Unique alert identifier.
    pub id: String,
    /// Ticker symbol this alert applies to.
    pub symbol: String,
    /// The condition that must be satisfied to fire this alert.
    pub condition: AlertCondition,
    /// Severity level.
    pub severity: AlertSeverity,
    /// Unix millisecond timestamp when this alert was created.
    pub created_at: u64,
    /// Unix millisecond timestamp when this alert was last triggered (`None` if never).
    pub triggered_at: Option<u64>,
    /// Human-readable message emitted when the alert fires.
    pub message: String,
}

// ─────────────────────────────────────────
//  Internal symbol state
// ─────────────────────────────────────────

/// Rolling tick state used to evaluate alert conditions.
struct SymbolState {
    /// Recent (price, volume, timestamp_ms) ticks.
    ticks: VecDeque<(f64, f64, u64)>,
    /// Recent prices (for MA / Bollinger calculations).
    prices: VecDeque<f64>,
}

impl SymbolState {
    fn new() -> Self {
        Self {
            ticks: VecDeque::new(),
            prices: VecDeque::new(),
        }
    }

    fn push(&mut self, price: f64, volume: f64, ts: u64) {
        self.ticks.push_back((price, volume, ts));
        self.prices.push_back(price);
        // Cap history to 1000 ticks to avoid unbounded growth.
        if self.ticks.len() > 1000 {
            self.ticks.pop_front();
        }
        if self.prices.len() > 1000 {
            self.prices.pop_front();
        }
    }

    fn latest_price(&self) -> Option<f64> {
        self.ticks.back().map(|t| t.0)
    }

    /// Simple moving average of the last `n` prices.
    fn sma(&self, n: usize) -> Option<f64> {
        if self.prices.len() < n || n == 0 {
            return None;
        }
        let sum: f64 = self.prices.iter().rev().take(n).sum();
        Some(sum / n as f64)
    }

    /// Bollinger band check: returns `true` if latest price is outside mean ± std_threshold * std.
    fn bollinger_breakout(&self, std_threshold: f64, window: usize) -> bool {
        if self.prices.len() < window || window < 2 {
            return false;
        }
        let recent: Vec<f64> = self.prices.iter().rev().take(window).cloned().collect();
        let mean = recent.iter().sum::<f64>() / window as f64;
        let variance = recent.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (window - 1) as f64;
        let std = variance.sqrt();
        let latest = recent[0]; // most-recent
        let band = std_threshold * std;
        latest > mean + band || latest < mean - band
    }

    /// Price-change-percent check within `window_ms`.
    fn price_change_pct(&self, now: u64, window_ms: u64) -> Option<f64> {
        let cutoff = now.saturating_sub(window_ms);
        // Find oldest tick within window.
        let baseline = self
            .ticks
            .iter()
            .find(|&&(_, _, ts)| ts >= cutoff)
            .map(|&(p, _, _)| p)?;
        let latest = self.latest_price()?;
        if baseline == 0.0 {
            return None;
        }
        Some((latest - baseline) / baseline * 100.0)
    }

    /// Average volume per tick within `window_ms`.
    fn avg_volume_in_window(&self, now: u64, window_ms: u64) -> f64 {
        let cutoff = now.saturating_sub(window_ms);
        let ticks: Vec<f64> = self
            .ticks
            .iter()
            .filter(|&&(_, _, ts)| ts >= cutoff)
            .map(|&(_, v, _)| v)
            .collect();
        if ticks.is_empty() {
            return 0.0;
        }
        ticks.iter().sum::<f64>() / ticks.len() as f64
    }
}

// ─────────────────────────────────────────
//  AlertManager
// ─────────────────────────────────────────

/// Manages a set of alerts across symbols and evaluates them on every tick.
pub struct AlertManager {
    alerts: HashMap<String, Alert>,
    state: HashMap<String, SymbolState>,
}

impl AlertManager {
    /// Create an empty alert manager.
    pub fn new() -> Self {
        Self {
            alerts: HashMap::new(),
            state: HashMap::new(),
        }
    }

    /// Register an alert. Returns the alert's ID.
    pub fn add_alert(&mut self, alert: Alert) -> String {
        let id = alert.id.clone();
        self.alerts.insert(id.clone(), alert);
        id
    }

    /// Remove an alert by ID. Returns `true` if it existed.
    pub fn remove_alert(&mut self, id: &str) -> bool {
        self.alerts.remove(id).is_some()
    }

    /// Feed a new price/volume tick for `symbol`, evaluate all alerts for that
    /// symbol, and return references to every alert that fired.
    pub fn update_price(
        &mut self,
        symbol: &str,
        price: f64,
        volume: f64,
        timestamp_ms: u64,
    ) -> Vec<&Alert> {
        // Update symbol state.
        self.state
            .entry(symbol.to_string())
            .or_insert_with(SymbolState::new)
            .push(price, volume, timestamp_ms);

        // Collect IDs of alerts that fire.
        let mut triggered_ids: Vec<String> = Vec::new();

        for (id, alert) in &self.alerts {
            if alert.symbol != symbol {
                continue;
            }
            let state = match self.state.get(symbol) {
                Some(s) => s,
                None => continue,
            };

            let fires = match &alert.condition {
                AlertCondition::PriceAbove { threshold } => price > *threshold,

                AlertCondition::PriceBelow { threshold } => price < *threshold,

                AlertCondition::PriceChangePct { pct, direction, window_ms } => {
                    match state.price_change_pct(timestamp_ms, *window_ms) {
                        None => false,
                        Some(change) => {
                            let magnitude_ok = change.abs() >= *pct;
                            let dir_ok = match direction {
                                1 => change > 0.0,
                                -1 => change < 0.0,
                                _ => true,
                            };
                            magnitude_ok && dir_ok
                        }
                    }
                }

                AlertCondition::VolumeSpike { multiple, window_ms } => {
                    let avg = state.avg_volume_in_window(timestamp_ms, *window_ms);
                    avg > 0.0 && volume > avg * multiple
                }

                AlertCondition::BollingerBreakout { std_threshold, window } => {
                    state.bollinger_breakout(*std_threshold, *window)
                }

                AlertCondition::CrossOver { fast_window, slow_window } => {
                    let prices = &state.prices;
                    let n = prices.len();
                    if n < *slow_window + 1 {
                        false
                    } else {
                        // Current and previous fast/slow MAs.
                        let sma_fast_now = state.sma(*fast_window).unwrap_or(0.0);
                        let sma_slow_now = state.sma(*slow_window).unwrap_or(0.0);
                        // Previous: compute from prices[..n-1].
                        let prev_fast: f64 = prices
                            .iter()
                            .rev()
                            .skip(1)
                            .take(*fast_window)
                            .sum::<f64>()
                            / *fast_window as f64;
                        let prev_slow: f64 = prices
                            .iter()
                            .rev()
                            .skip(1)
                            .take(*slow_window)
                            .sum::<f64>()
                            / *slow_window as f64;
                        // Crossover: fast was below slow, now above (or vice versa).
                        (prev_fast <= prev_slow && sma_fast_now > sma_slow_now)
                            || (prev_fast >= prev_slow && sma_fast_now < sma_slow_now)
                    }
                }
            };

            if fires {
                triggered_ids.push(id.clone());
            }
        }

        // Stamp triggered_at and collect references.
        for id in &triggered_ids {
            if let Some(a) = self.alerts.get_mut(id) {
                a.triggered_at = Some(timestamp_ms);
            }
        }

        // Return references.
        triggered_ids
            .iter()
            .filter_map(|id| self.alerts.get(id))
            .collect()
    }

    /// All alerts registered for `symbol` (triggered or not).
    pub fn active_alerts(&self, symbol: &str) -> Vec<&Alert> {
        self.alerts
            .values()
            .filter(|a| a.symbol == symbol)
            .collect()
    }

    /// All alerts that have been triggered at least once.
    pub fn triggered_alerts(&self) -> Vec<&Alert> {
        self.alerts
            .values()
            .filter(|a| a.triggered_at.is_some())
            .collect()
    }

    /// Total number of registered alerts.
    pub fn alert_count(&self) -> usize {
        self.alerts.len()
    }
}

impl Default for AlertManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_alert(id: &str, symbol: &str, condition: AlertCondition) -> Alert {
        Alert {
            id: id.to_string(),
            symbol: symbol.to_string(),
            condition,
            severity: AlertSeverity::Warning,
            created_at: 0,
            triggered_at: None,
            message: "test alert".to_string(),
        }
    }

    // ── add / remove / count ───────────────────────────────────────────────

    #[test]
    fn add_and_count() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 200.0 }));
        mgr.add_alert(make_alert("a2", "GOOG", AlertCondition::PriceBelow { threshold: 100.0 }));
        assert_eq!(mgr.alert_count(), 2);
    }

    #[test]
    fn remove_existing() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 200.0 }));
        assert!(mgr.remove_alert("a1"));
        assert_eq!(mgr.alert_count(), 0);
    }

    #[test]
    fn remove_nonexistent() {
        let mut mgr = AlertManager::new();
        assert!(!mgr.remove_alert("nonexistent"));
    }

    // ── PriceAbove ────────────────────────────────────────────────────────

    #[test]
    fn price_above_fires() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 150.0 }));
        let fired = mgr.update_price("AAPL", 151.0, 100.0, 1_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, "a1");
    }

    #[test]
    fn price_above_no_fire_below_threshold() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 150.0 }));
        let fired = mgr.update_price("AAPL", 149.0, 100.0, 1_000);
        assert!(fired.is_empty());
    }

    // ── PriceBelow ────────────────────────────────────────────────────────

    #[test]
    fn price_below_fires() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "TSLA", AlertCondition::PriceBelow { threshold: 200.0 }));
        let fired = mgr.update_price("TSLA", 199.0, 50.0, 1_000);
        assert_eq!(fired.len(), 1);
    }

    // ── PriceChangePct ────────────────────────────────────────────────────

    #[test]
    fn price_change_pct_fires_upward() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert(
            "a1",
            "AAPL",
            AlertCondition::PriceChangePct { pct: 5.0, direction: 1, window_ms: 60_000 },
        ));
        mgr.update_price("AAPL", 100.0, 10.0, 0);
        let fired = mgr.update_price("AAPL", 106.0, 10.0, 30_000);
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn price_change_pct_no_fire_wrong_direction() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert(
            "a1",
            "AAPL",
            AlertCondition::PriceChangePct { pct: 5.0, direction: -1, window_ms: 60_000 },
        ));
        mgr.update_price("AAPL", 100.0, 10.0, 0);
        let fired = mgr.update_price("AAPL", 106.0, 10.0, 30_000);
        assert!(fired.is_empty());
    }

    // ── VolumeSpike ───────────────────────────────────────────────────────

    #[test]
    fn volume_spike_fires() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert(
            "a1",
            "AAPL",
            AlertCondition::VolumeSpike { multiple: 3.0, window_ms: 60_000 },
        ));
        // Baseline ticks
        for i in 0..5u64 {
            mgr.update_price("AAPL", 100.0, 10.0, i * 5_000);
        }
        // Spike tick: 10× baseline avg
        let fired = mgr.update_price("AAPL", 100.0, 1000.0, 30_000);
        assert_eq!(fired.len(), 1);
    }

    // ── BollingerBreakout ─────────────────────────────────────────────────

    #[test]
    fn bollinger_breakout_fires() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert(
            "a1",
            "AAPL",
            AlertCondition::BollingerBreakout { std_threshold: 1.0, window: 5 },
        ));
        // Feed stable prices then a big outlier.
        for _ in 0..5 {
            mgr.update_price("AAPL", 100.0, 10.0, 0);
        }
        // 3-sigma jump above the mean
        let fired = mgr.update_price("AAPL", 200.0, 10.0, 1_000);
        assert!(!fired.is_empty());
    }

    // ── CrossOver ─────────────────────────────────────────────────────────

    #[test]
    fn crossover_fires_golden_cross() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert(
            "a1",
            "AAPL",
            AlertCondition::CrossOver { fast_window: 2, slow_window: 4 },
        ));
        // Start low: fast below slow
        for _ in 0..4 {
            mgr.update_price("AAPL", 100.0, 1.0, 0);
        }
        // Rapid jump — fast MA crosses above slow MA
        mgr.update_price("AAPL", 200.0, 1.0, 1_000);
        let fired = mgr.update_price("AAPL", 200.0, 1.0, 2_000);
        assert!(!fired.is_empty());
    }

    // ── active_alerts / triggered_alerts ──────────────────────────────────

    #[test]
    fn active_alerts_by_symbol() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 50.0 }));
        mgr.add_alert(make_alert("a2", "GOOG", AlertCondition::PriceAbove { threshold: 50.0 }));
        assert_eq!(mgr.active_alerts("AAPL").len(), 1);
        assert_eq!(mgr.active_alerts("GOOG").len(), 1);
        assert_eq!(mgr.active_alerts("TSLA").len(), 0);
    }

    #[test]
    fn triggered_alerts_populated() {
        let mut mgr = AlertManager::new();
        mgr.add_alert(make_alert("a1", "AAPL", AlertCondition::PriceAbove { threshold: 100.0 }));
        assert!(mgr.triggered_alerts().is_empty());
        mgr.update_price("AAPL", 101.0, 10.0, 1_000);
        assert_eq!(mgr.triggered_alerts().len(), 1);
    }
}
