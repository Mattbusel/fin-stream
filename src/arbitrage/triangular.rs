//! # Module: arbitrage::triangular
//!
//! Triangular arbitrage detection for crypto markets.
//!
//! ## Overview
//! A triangular arbitrage opportunity exists when three exchange rates are
//! inconsistent: starting with 1 unit of a quote currency, traversing three
//! currency pairs and back yields more than 1 unit.
//!
//! ## Common triads
//! - Forward:  USD → BTC → ETH → USD
//! - Reverse:  USD → ETH → BTC → USD
//!
//! ## NOT Responsible For
//! - Order execution
//! - Real-time WebSocket feed management (use `TriangularArbDetector::update_rate`)

use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ─── Symbol ──────────────────────────────────────────────────────────────────

/// A currency pair, e.g. BTC/USD.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Symbol {
    /// Base currency (e.g. "BTC").
    pub base: String,
    /// Quote currency (e.g. "USD").
    pub quote: String,
}

impl Symbol {
    /// Constructs a new symbol.
    pub fn new(base: impl Into<String>, quote: impl Into<String>) -> Self {
        Self { base: base.into(), quote: quote.into() }
    }

    /// Returns the canonical pair name "BASE/QUOTE".
    pub fn pair_name(&self) -> String {
        format!("{}/{}", self.base, self.quote)
    }
}

// ─── Trade direction ─────────────────────────────────────────────────────────

/// Direction of a trade leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TradeDirection {
    /// Buy the base currency (pay ask).
    Buy,
    /// Sell the base currency (receive bid).
    Sell,
}

// ─── Exchange rate ────────────────────────────────────────────────────────────

/// A live bid/ask quote for a currency pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExchangeRate {
    /// The currency pair this quote is for.
    pub symbol: Symbol,
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Quote timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

impl ExchangeRate {
    /// Mid-price: `(bid + ask) / 2`.
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }
}

// ─── Triangular path ─────────────────────────────────────────────────────────

/// A 3-leg currency cycle.
///
/// Each leg is a `(Symbol, TradeDirection)` pair.
/// The cycle starts and ends in the same currency.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TriangularPath {
    /// The three legs of the triangular path.
    pub legs: [(Symbol, TradeDirection); 3],
}

impl TriangularPath {
    /// Human-readable description of the path, e.g. "BTC/USD(Buy)→ETH/BTC(Buy)→ETH/USD(Sell)".
    pub fn description(&self) -> String {
        self.legs
            .iter()
            .map(|(sym, dir)| format!("{}({:?})", sym.pair_name(), dir))
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

// ─── Arbitrage opportunity ────────────────────────────────────────────────────

/// A detected triangular arbitrage opportunity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbOpportunity {
    /// The three-leg path that generates the profit.
    pub path: TriangularPath,
    /// Profit as a percentage: `(rate_product - 1) * 100`.
    pub profit_pct: f64,
    /// Product of execution rates through the three legs.
    pub rate_product: f64,
    /// Timestamp in milliseconds when the opportunity was detected.
    pub timestamp_ms: u64,
}

impl ArbOpportunity {
    /// Returns `true` if `profit_pct > min_profit_pct`.
    pub fn is_profitable(&self, min_profit_pct: f64) -> bool {
        self.profit_pct > min_profit_pct
    }
}

// ─── Detector ────────────────────────────────────────────────────────────────

/// Concurrent triangular arbitrage detector backed by a `DashMap` of live rates.
///
/// Call [`update_rate`] on every incoming quote, then [`scan_opportunities`] to
/// check all known triads for profitable paths.
pub struct TriangularArbDetector {
    /// Live exchange rates keyed by pair name (e.g. "BTC/USD").
    rates: Arc<DashMap<String, ExchangeRate>>,
    /// Minimum profit percentage to emit an opportunity.
    min_profit_pct: f64,
}

impl TriangularArbDetector {
    /// Create a new detector.
    ///
    /// - `min_profit_pct`: minimum net profit percentage (e.g. 0.05 = 5 bps).
    pub fn new(min_profit_pct: f64) -> Self {
        Self { rates: Arc::new(DashMap::new()), min_profit_pct }
    }

    /// Update the live rate for a currency pair.
    pub fn update_rate(&self, rate: ExchangeRate) {
        self.rates.insert(rate.symbol.pair_name(), rate);
    }

    /// Scan all known triads and return profitable opportunities.
    pub fn scan_opportunities(&self) -> Vec<ArbOpportunity> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let triads = Self::known_triads();
        let mut opportunities = Vec::new();

        for path in triads {
            // Gather the rates for each leg; skip if any is missing
            let rates: Option<Vec<ExchangeRate>> = path
                .legs
                .iter()
                .map(|(sym, _)| {
                    self.rates.get(&sym.pair_name()).map(|r| r.clone())
                })
                .collect();

            let rates = match rates {
                Some(r) => r,
                None => continue,
            };

            let directions: Vec<TradeDirection> = path.legs.iter().map(|(_, d)| *d).collect();
            let profit = Self::compute_path_profit(&rates, &directions);
            let profit_pct = profit * 100.0;

            if profit_pct > self.min_profit_pct {
                opportunities.push(ArbOpportunity {
                    path,
                    profit_pct,
                    rate_product: 1.0 + profit,
                    timestamp_ms: now_ms,
                });
            }
        }

        opportunities
    }

    /// Walk through a sequence of exchange-rate legs and compute net profit.
    ///
    /// Starting with 1 unit of capital:
    /// - `Buy`  leg: divide by ask (pay ask to acquire base).
    /// - `Sell` leg: multiply by bid (receive bid when selling base).
    ///
    /// Returns `final_amount - 1.0` (positive = profit, negative = loss).
    pub fn compute_path_profit(rates: &[ExchangeRate], directions: &[TradeDirection]) -> f64 {
        if rates.len() != directions.len() || rates.is_empty() {
            return -1.0;
        }
        let mut amount = 1.0_f64;
        for (rate, dir) in rates.iter().zip(directions.iter()) {
            amount = match dir {
                TradeDirection::Buy  => amount / rate.ask,
                TradeDirection::Sell => amount * rate.bid,
            };
            if !amount.is_finite() || amount <= 0.0 {
                return -1.0;
            }
        }
        amount - 1.0
    }

    /// Returns the hardcoded list of common crypto triangular paths.
    ///
    /// Covers the forward and reverse paths for the BTC/USD, ETH/USD, ETH/BTC triad.
    pub fn known_triads() -> Vec<TriangularPath> {
        // Notation: starting with USD
        // Forward:  Buy BTC (BTC/USD), Buy ETH with BTC (ETH/BTC), Sell ETH for USD (ETH/USD)
        // Reverse:  Buy ETH (ETH/USD), Sell ETH for BTC (ETH/BTC), Sell BTC for USD (BTC/USD)
        vec![
            TriangularPath {
                legs: [
                    (Symbol::new("BTC", "USD"), TradeDirection::Buy),
                    (Symbol::new("ETH", "BTC"), TradeDirection::Buy),
                    (Symbol::new("ETH", "USD"), TradeDirection::Sell),
                ],
            },
            TriangularPath {
                legs: [
                    (Symbol::new("ETH", "USD"), TradeDirection::Buy),
                    (Symbol::new("ETH", "BTC"), TradeDirection::Sell),
                    (Symbol::new("BTC", "USD"), TradeDirection::Sell),
                ],
            },
        ]
    }
}

// ─── Monitor ─────────────────────────────────────────────────────────────────

/// Aggregate statistics about arbitrage opportunities seen.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ArbStats {
    /// Total number of opportunities detected (including unprofitable).
    pub total_found: u64,
    /// Number of opportunities that passed the profitability threshold.
    pub profitable: u64,
    /// Average profit percentage across all profitable opportunities.
    pub avg_profit_pct: f64,
    /// Best (highest) profit percentage observed.
    pub best_profit_pct: f64,
}

/// Records and tracks arbitrage opportunities over time.
pub struct ArbMonitor {
    stats: ArbStats,
    total_profit_pct_sum: f64,
    missed: u64,
    /// Shared counter incremented by the detector.
    scan_count: Arc<AtomicU64>,
}

impl ArbMonitor {
    /// Create a new monitor.
    pub fn new() -> Self {
        Self {
            stats: ArbStats::default(),
            total_profit_pct_sum: 0.0,
            missed: 0,
            scan_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record a detected opportunity.
    ///
    /// Call this for every opportunity returned by [`TriangularArbDetector::scan_opportunities`].
    pub fn record_opportunity(&mut self, opp: ArbOpportunity) {
        self.stats.total_found += 1;
        self.scan_count.fetch_add(1, Ordering::Relaxed);

        if opp.profit_pct > 0.0 {
            self.stats.profitable += 1;
            self.total_profit_pct_sum += opp.profit_pct;
            self.stats.avg_profit_pct = self.total_profit_pct_sum / self.stats.profitable as f64;
            if opp.profit_pct > self.stats.best_profit_pct {
                self.stats.best_profit_pct = opp.profit_pct;
            }
        }
    }

    /// Record a missed opportunity (arrived too late to act on).
    pub fn record_missed(&mut self) {
        self.missed += 1;
    }

    /// Returns aggregate statistics.
    pub fn stats(&self) -> ArbStats {
        self.stats
    }

    /// Number of opportunities that arrived too late.
    pub fn missed_count(&self) -> u64 {
        self.missed
    }
}

impl Default for ArbMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rate(base: &str, quote: &str, bid: f64, ask: f64) -> ExchangeRate {
        ExchangeRate {
            symbol: Symbol::new(base, quote),
            bid,
            ask,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn test_symbol_pair_name() {
        let s = Symbol::new("BTC", "USD");
        assert_eq!(s.pair_name(), "BTC/USD");
    }

    #[test]
    fn test_exchange_rate_mid() {
        let r = make_rate("BTC", "USD", 49000.0, 51000.0);
        assert!((r.mid() - 50000.0).abs() < 1e-6);
    }

    #[test]
    fn test_arb_opportunity_is_profitable() {
        let triads = TriangularArbDetector::known_triads();
        let opp = ArbOpportunity {
            path: triads.into_iter().next().expect("has triads"),
            profit_pct: 0.1,
            rate_product: 1.001,
            timestamp_ms: 0,
        };
        assert!(opp.is_profitable(0.05));
        assert!(!opp.is_profitable(0.2));
    }

    #[test]
    fn test_compute_path_profit_profitable() {
        // Forward path: BTC/USD(Buy), ETH/BTC(Buy), ETH/USD(Sell)
        // Start: 1 USD
        // Buy BTC at 50000 ask → 1/50000 = 0.00002 BTC
        // Buy ETH at 0.04 ask → 0.00002/0.04 = 0.0005 ETH
        // Sell ETH at 3100 bid → 0.0005 * 3100 = 1.55 USD → profit = 55%
        let rates = vec![
            make_rate("BTC", "USD", 49999.0, 50000.0),
            make_rate("ETH", "BTC", 0.0399, 0.04),
            make_rate("ETH", "USD", 3100.0, 3101.0),
        ];
        let dirs = vec![TradeDirection::Buy, TradeDirection::Buy, TradeDirection::Sell];
        let profit = TriangularArbDetector::compute_path_profit(&rates, &dirs);
        assert!(profit > 0.0, "profit={profit}");
    }

    #[test]
    fn test_compute_path_profit_unprofitable() {
        // At fair prices: BTC=50000, ETH/USD=2000, ETH/BTC=0.04
        // 1/50000 / 0.04001 * 1999 ≈ 0.9995 → loss
        let rates = vec![
            make_rate("BTC", "USD", 49999.0, 50001.0),
            make_rate("ETH", "BTC", 0.03999, 0.04001),
            make_rate("ETH", "USD", 1999.0, 2001.0),
        ];
        let dirs = vec![TradeDirection::Buy, TradeDirection::Buy, TradeDirection::Sell];
        let profit = TriangularArbDetector::compute_path_profit(&rates, &dirs);
        assert!(profit < 0.0, "profit={profit}");
    }

    #[test]
    fn test_detector_update_and_scan_profitable() {
        let det = TriangularArbDetector::new(0.01); // 0.01% threshold
        // Inject skewed prices: ETH/USD at 3100 while BTC=50000, ETH/BTC=0.04 (fair ETH/USD=2000)
        det.update_rate(make_rate("BTC", "USD", 50000.0, 50000.0));
        det.update_rate(make_rate("ETH", "BTC", 0.04, 0.04));
        det.update_rate(make_rate("ETH", "USD", 3100.0, 3100.0));
        let opps = det.scan_opportunities();
        assert!(!opps.is_empty(), "should detect forward arb");
        assert!(opps.iter().all(|o| o.profit_pct > 0.01));
    }

    #[test]
    fn test_detector_no_opportunity_at_fair_prices() {
        let det = TriangularArbDetector::new(0.01);
        det.update_rate(make_rate("BTC", "USD", 49999.0, 50001.0));
        det.update_rate(make_rate("ETH", "BTC", 0.03999, 0.04001));
        det.update_rate(make_rate("ETH", "USD", 1999.0, 2001.0));
        let opps = det.scan_opportunities();
        assert!(opps.is_empty(), "no arb at fair prices");
    }

    #[test]
    fn test_detector_no_opportunity_missing_rate() {
        let det = TriangularArbDetector::new(0.01);
        // Only provide two of three rates
        det.update_rate(make_rate("BTC", "USD", 50000.0, 50000.0));
        det.update_rate(make_rate("ETH", "USD", 2000.0, 2000.0));
        let opps = det.scan_opportunities();
        assert!(opps.is_empty());
    }

    #[test]
    fn test_known_triads_has_forward_and_reverse() {
        let triads = TriangularArbDetector::known_triads();
        assert_eq!(triads.len(), 2, "should have forward and reverse triads");
    }

    #[test]
    fn test_arb_monitor_records_opportunities() {
        let mut monitor = ArbMonitor::new();
        let triads = TriangularArbDetector::known_triads();
        let opp = ArbOpportunity {
            path: triads.into_iter().next().expect("has triads"),
            profit_pct: 0.15,
            rate_product: 1.0015,
            timestamp_ms: 0,
        };
        monitor.record_opportunity(opp);
        let stats = monitor.stats();
        assert_eq!(stats.total_found, 1);
        assert_eq!(stats.profitable, 1);
        assert!((stats.avg_profit_pct - 0.15).abs() < 1e-9);
        assert!((stats.best_profit_pct - 0.15).abs() < 1e-9);
    }

    #[test]
    fn test_arb_monitor_missed_count() {
        let mut monitor = ArbMonitor::new();
        monitor.record_missed();
        monitor.record_missed();
        assert_eq!(monitor.missed_count(), 2);
    }
}
