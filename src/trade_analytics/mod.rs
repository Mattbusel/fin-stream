//! Execution quality analytics: slippage, implementation shortfall, VWAP deviation,
//! fill rate, venue comparison, and trade cost analysis reports.
//!
//! [`ExecutionAnalyzer`] accumulates [`ExecutedTrade`] records and provides
//! aggregate execution quality metrics across symbols and venues.

use std::collections::HashMap;

/// A single executed trade with pre-trade and benchmark reference prices.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedTrade {
    /// Unique trade identifier.
    pub trade_id: String,
    /// Ticker symbol.
    pub symbol: String,
    /// Trade side: +1 for buy, -1 for sell.
    pub side: i8,
    /// Number of shares/units executed.
    pub quantity: f64,
    /// Mid-market price at the time the order arrived (decision price).
    pub arrival_price: f64,
    /// The actual average execution price.
    pub executed_price: f64,
    /// Benchmark price (e.g. session VWAP) for implementation shortfall.
    pub benchmark_price: f64,
    /// Execution timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
    /// Venue or exchange where the trade was executed.
    pub venue: String,
}

/// Stateless execution quality metrics.
pub struct ExecutionQuality;

impl ExecutionQuality {
    /// Compute slippage in basis points relative to the arrival price.
    ///
    /// Slippage = (executed_price − arrival_price) / arrival_price × 10 000 × side
    ///
    /// A positive value means the execution was worse than the arrival price:
    /// - Buy paid more than the arrival mid.
    /// - Sell received less than the arrival mid.
    pub fn slippage_bps(trade: &ExecutedTrade) -> f64 {
        if trade.arrival_price == 0.0 {
            return 0.0;
        }
        (trade.executed_price - trade.arrival_price) / trade.arrival_price
            * 10_000.0
            * trade.side as f64
    }

    /// Compute implementation shortfall in basis points relative to the benchmark price.
    ///
    /// IS = (executed_price − benchmark_price) / benchmark_price × 10 000 × side
    ///
    /// Positive means underperformed the benchmark (e.g., paid above VWAP on a buy).
    pub fn implementation_shortfall_bps(trade: &ExecutedTrade) -> f64 {
        if trade.benchmark_price == 0.0 {
            return 0.0;
        }
        (trade.executed_price - trade.benchmark_price) / trade.benchmark_price
            * 10_000.0
            * trade.side as f64
    }

    /// Compute price improvement relative to the arrival mid-price.
    ///
    /// Price improvement = (arrival_price − executed_price) × side × quantity.
    ///
    /// Positive = execution was better than the arrival price.
    /// - Buy executed below the arrival mid → positive improvement.
    /// - Sell executed above the arrival mid → positive improvement.
    pub fn price_improvement(trade: &ExecutedTrade) -> f64 {
        (trade.arrival_price - trade.executed_price) * trade.side as f64 * trade.quantity
    }
}

/// VWAP computation and VWAP deviation analysis.
pub struct VwapAnalyzer;

impl VwapAnalyzer {
    /// Compute the volume-weighted average price from parallel price and volume slices.
    ///
    /// Returns 0.0 if `volumes` sums to zero.
    pub fn compute_vwap(prices: &[f64], volumes: &[f64]) -> f64 {
        let total_vol: f64 = volumes.iter().sum();
        if total_vol == 0.0 {
            return 0.0;
        }
        let weighted_sum: f64 = prices.iter().zip(volumes.iter()).map(|(p, v)| p * v).sum();
        weighted_sum / total_vol
    }

    /// Compute the deviation of a trade's executed price from the market VWAP, in basis points.
    ///
    /// Deviation = (executed_price − market_vwap) / market_vwap × 10 000 × side
    pub fn vwap_deviation(trade: &ExecutedTrade, market_vwap: f64) -> f64 {
        if market_vwap == 0.0 {
            return 0.0;
        }
        (trade.executed_price - market_vwap) / market_vwap * 10_000.0 * trade.side as f64
    }
}

/// Accumulates executed trades and provides aggregate execution quality analytics.
#[derive(Debug, Default)]
pub struct ExecutionAnalyzer {
    /// All recorded trades.
    trades: Vec<ExecutedTrade>,
}

impl ExecutionAnalyzer {
    /// Create a new, empty analyzer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an executed trade.
    pub fn record_trade(&mut self, trade: ExecutedTrade) {
        self.trades.push(trade);
    }

    /// Return the average slippage in basis points for `symbol`.
    ///
    /// Returns `None` if no trades for the symbol have been recorded.
    pub fn avg_slippage_bps(&self, symbol: &str) -> Option<f64> {
        let symbol_trades: Vec<&ExecutedTrade> = self
            .trades
            .iter()
            .filter(|t| t.symbol == symbol)
            .collect();
        if symbol_trades.is_empty() {
            return None;
        }
        let total: f64 = symbol_trades
            .iter()
            .map(|t| ExecutionQuality::slippage_bps(t))
            .sum();
        Some(total / symbol_trades.len() as f64)
    }

    /// Return the fill rate for `symbol`: the fraction of trades with zero or negative slippage.
    ///
    /// A "positive fill" means the execution was at or better than the arrival price.
    /// Returns 0.0 if no trades recorded.
    pub fn fill_rate(&self, symbol: &str) -> f64 {
        let symbol_trades: Vec<&ExecutedTrade> = self
            .trades
            .iter()
            .filter(|t| t.symbol == symbol)
            .collect();
        if symbol_trades.is_empty() {
            return 0.0;
        }
        let good_fills = symbol_trades
            .iter()
            .filter(|t| ExecutionQuality::slippage_bps(t) <= 0.0)
            .count();
        good_fills as f64 / symbol_trades.len() as f64
    }

    /// Return average slippage (in bps) per venue, sorted best-first (lowest avg slippage).
    pub fn venue_comparison(&self) -> Vec<(String, f64)> {
        let mut venue_stats: HashMap<&str, (f64, u64)> = HashMap::new();
        for trade in &self.trades {
            let entry = venue_stats.entry(trade.venue.as_str()).or_insert((0.0, 0));
            entry.0 += ExecutionQuality::slippage_bps(trade);
            entry.1 += 1;
        }
        let mut result: Vec<(String, f64)> = venue_stats
            .into_iter()
            .map(|(venue, (sum, count))| {
                let avg = if count > 0 { sum / count as f64 } else { 0.0 };
                (venue.to_string(), avg)
            })
            .collect();
        // Sort best (lowest slippage) first.
        result.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        result
    }

    /// Generate a formatted trade cost analysis report covering all recorded trades.
    pub fn trade_cost_analysis_report(&self) -> String {
        if self.trades.is_empty() {
            return "Trade Cost Analysis Report\n==========================\nNo trades recorded.\n".to_string();
        }

        let total_trades = self.trades.len();
        let all_slippage: Vec<f64> = self
            .trades
            .iter()
            .map(|t| ExecutionQuality::slippage_bps(t))
            .collect();
        let avg_slippage = all_slippage.iter().sum::<f64>() / total_trades as f64;
        let avg_is: f64 = self
            .trades
            .iter()
            .map(|t| ExecutionQuality::implementation_shortfall_bps(t))
            .sum::<f64>()
            / total_trades as f64;
        let total_pi: f64 = self
            .trades
            .iter()
            .map(|t| ExecutionQuality::price_improvement(t))
            .sum();
        let good_fills = all_slippage.iter().filter(|&&s| s <= 0.0).count();
        let overall_fill_rate = good_fills as f64 / total_trades as f64;

        let venue_cmp = self.venue_comparison();
        let venue_lines: String = venue_cmp
            .iter()
            .map(|(v, s)| format!("  {:>20}: {:+.2} bps\n", v, s))
            .collect();

        // Per-symbol summary.
        let mut symbols: Vec<&str> = self.trades.iter().map(|t| t.symbol.as_str()).collect();
        symbols.sort_unstable();
        symbols.dedup();
        let symbol_lines: String = symbols
            .iter()
            .map(|sym| {
                let avg = self.avg_slippage_bps(sym).unwrap_or(0.0);
                let fr = self.fill_rate(sym);
                format!("  {:>10}: avg_slippage={:+.2} bps  fill_rate={:.1}%\n", sym, avg, fr * 100.0)
            })
            .collect();

        format!(
            "Trade Cost Analysis Report\n\
             ==========================\n\
             Total trades          : {total_trades}\n\
             Avg slippage          : {avg_slippage:+.2} bps\n\
             Avg impl. shortfall   : {avg_is:+.2} bps\n\
             Total price improvement: ${total_pi:.2}\n\
             Overall fill rate     : {pct:.1}%\n\
             \n\
             Per-Symbol:\n\
             {symbol_lines}\
             \n\
             Venue Comparison (best → worst):\n\
             {venue_lines}",
            pct = overall_fill_rate * 100.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy_trade(id: &str, sym: &str, arr: f64, exec: f64, bench: f64, venue: &str) -> ExecutedTrade {
        ExecutedTrade {
            trade_id: id.to_string(),
            symbol: sym.to_string(),
            side: 1,
            quantity: 100.0,
            arrival_price: arr,
            executed_price: exec,
            benchmark_price: bench,
            timestamp_ms: 1_700_000_000_000,
            venue: venue.to_string(),
        }
    }

    fn sell_trade(id: &str, sym: &str, arr: f64, exec: f64, bench: f64, venue: &str) -> ExecutedTrade {
        ExecutedTrade {
            trade_id: id.to_string(),
            symbol: sym.to_string(),
            side: -1,
            quantity: 100.0,
            arrival_price: arr,
            executed_price: exec,
            benchmark_price: bench,
            timestamp_ms: 1_700_000_000_000,
            venue: venue.to_string(),
        }
    }

    #[test]
    fn test_slippage_bps_buy() {
        // Buy: executed at 100.10, arrival 100.00 → (0.10/100.00)*10000*1 = 10 bps
        let t = buy_trade("T1", "AAPL", 100.0, 100.10, 100.0, "NYSE");
        assert!((ExecutionQuality::slippage_bps(&t) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_slippage_bps_sell() {
        // Sell: executed at 99.90, arrival 100.00 → (99.90-100.00)/100.00*10000*(-1) = 10 bps
        let t = sell_trade("T2", "AAPL", 100.0, 99.90, 100.0, "NYSE");
        assert!((ExecutionQuality::slippage_bps(&t) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_implementation_shortfall_bps() {
        // Buy at 100.05 vs VWAP 100.00 → 5 bps IS
        let t = buy_trade("T3", "AAPL", 100.0, 100.05, 100.0, "NASDAQ");
        assert!((ExecutionQuality::implementation_shortfall_bps(&t) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn test_price_improvement_positive() {
        // Buy at 99.90, arrival 100.00 → improvement = (100-99.90)*1*100 = 10
        let t = buy_trade("T4", "AAPL", 100.0, 99.90, 100.0, "DARK");
        assert!((ExecutionQuality::price_improvement(&t) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_price_improvement_negative() {
        // Buy at 100.10, arrival 100.00 → improvement = (100-100.10)*1*100 = -10
        let t = buy_trade("T5", "AAPL", 100.0, 100.10, 100.0, "NYSE");
        assert!((ExecutionQuality::price_improvement(&t) - (-10.0)).abs() < 1e-6);
    }

    #[test]
    fn test_vwap_compute() {
        let prices = [100.0, 101.0, 99.0];
        let volumes = [200.0, 100.0, 300.0];
        // VWAP = (100*200 + 101*100 + 99*300) / 600 = (20000+10100+29700)/600 = 59800/600 = 99.6667
        let vwap = VwapAnalyzer::compute_vwap(&prices, &volumes);
        assert!((vwap - 59800.0 / 600.0).abs() < 1e-6);
    }

    #[test]
    fn test_vwap_zero_volume() {
        let prices = [100.0, 101.0];
        let volumes = [0.0, 0.0];
        assert_eq!(VwapAnalyzer::compute_vwap(&prices, &volumes), 0.0);
    }

    #[test]
    fn test_vwap_deviation() {
        let t = buy_trade("T6", "AAPL", 100.0, 100.10, 100.0, "NYSE");
        // dev = (100.10 - 100.00) / 100.00 * 10000 * 1 = 10 bps
        let dev = VwapAnalyzer::vwap_deviation(&t, 100.0);
        assert!((dev - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_avg_slippage_bps_some() {
        let mut ea = ExecutionAnalyzer::new();
        ea.record_trade(buy_trade("T1", "AAPL", 100.0, 100.10, 100.0, "NYSE")); // 10 bps
        ea.record_trade(buy_trade("T2", "AAPL", 100.0, 100.20, 100.0, "NYSE")); // 20 bps
        let avg = ea.avg_slippage_bps("AAPL").unwrap();
        assert!((avg - 15.0).abs() < 1e-6);
    }

    #[test]
    fn test_avg_slippage_bps_none() {
        let ea = ExecutionAnalyzer::new();
        assert!(ea.avg_slippage_bps("AAPL").is_none());
    }

    #[test]
    fn test_fill_rate() {
        let mut ea = ExecutionAnalyzer::new();
        // Good fill: buy below arrival
        ea.record_trade(buy_trade("T1", "AAPL", 100.0, 99.90, 100.0, "NYSE"));
        // Bad fill: buy above arrival
        ea.record_trade(buy_trade("T2", "AAPL", 100.0, 100.20, 100.0, "NYSE"));
        // Good fill: buy at exactly arrival
        ea.record_trade(buy_trade("T3", "AAPL", 100.0, 100.0, 100.0, "NYSE"));
        assert!((ea.fill_rate("AAPL") - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn test_fill_rate_missing_symbol() {
        let ea = ExecutionAnalyzer::new();
        assert_eq!(ea.fill_rate("UNKNOWN"), 0.0);
    }

    #[test]
    fn test_venue_comparison_sorted() {
        let mut ea = ExecutionAnalyzer::new();
        // NASDAQ: 5 bps slippage
        ea.record_trade(buy_trade("T1", "AAPL", 100.0, 100.05, 100.0, "NASDAQ"));
        // NYSE: 15 bps slippage
        ea.record_trade(buy_trade("T2", "AAPL", 100.0, 100.15, 100.0, "NYSE"));
        // DARK: -5 bps (price improvement)
        ea.record_trade(buy_trade("T3", "AAPL", 100.0, 99.95, 100.0, "DARK"));
        let cmp = ea.venue_comparison();
        // Best first: DARK (-5), NASDAQ (5), NYSE (15)
        assert_eq!(cmp[0].0, "DARK");
        assert_eq!(cmp[2].0, "NYSE");
    }

    #[test]
    fn test_report_no_trades() {
        let ea = ExecutionAnalyzer::new();
        let report = ea.trade_cost_analysis_report();
        assert!(report.contains("No trades recorded"));
    }

    #[test]
    fn test_report_with_trades() {
        let mut ea = ExecutionAnalyzer::new();
        ea.record_trade(buy_trade("T1", "AAPL", 100.0, 100.05, 100.0, "NYSE"));
        ea.record_trade(sell_trade("T2", "MSFT", 200.0, 199.80, 200.0, "NASDAQ"));
        let report = ea.trade_cost_analysis_report();
        assert!(report.contains("Trade Cost Analysis Report"));
        assert!(report.contains("Total trades          : 2"));
        assert!(report.contains("AAPL"));
        assert!(report.contains("MSFT"));
        assert!(report.contains("Venue Comparison"));
    }

    #[test]
    fn test_slippage_zero_arrival_price() {
        let t = buy_trade("T1", "AAPL", 0.0, 100.0, 100.0, "NYSE");
        assert_eq!(ExecutionQuality::slippage_bps(&t), 0.0);
    }

    #[test]
    fn test_is_zero_benchmark_price() {
        let t = buy_trade("T1", "AAPL", 100.0, 100.0, 0.0, "NYSE");
        assert_eq!(ExecutionQuality::implementation_shortfall_bps(&t), 0.0);
    }
}
