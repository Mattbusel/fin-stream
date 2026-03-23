//! Order flow analytics and VPIN (Volume-Synchronized Probability of Informed Trading).

/// A single executed trade.
#[derive(Debug, Clone)]
pub struct Trade {
    /// Execution price.
    pub price: f64,
    /// Trade size (unsigned).
    pub size: f64,
    /// Trade side: `1` = buy-initiated, `-1` = sell-initiated.
    pub side: i8,
    /// Timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

/// Configuration for VPIN computation.
#[derive(Debug, Clone)]
pub struct VpinConfig {
    /// Volume per bucket (aggregate trades until this volume threshold is reached).
    pub bucket_size: f64,
    /// Number of buckets in the rolling VPIN window.
    pub window_buckets: usize,
}

/// VPIN calculator: classifies trade flow and computes order flow toxicity.
pub struct VpinCalculator;

impl VpinCalculator {
    /// Classifies trades into volume buckets of `config.bucket_size`.
    ///
    /// Returns a `Vec<(buy_vol, sell_vol)>` where each tuple is one filled bucket.
    pub fn classify_trades(trades: &[Trade], config: &VpinConfig) -> Vec<(f64, f64)> {
        let mut buckets: Vec<(f64, f64)> = Vec::new();
        let mut current_buy = 0.0_f64;
        let mut current_sell = 0.0_f64;
        let mut accumulated = 0.0_f64;

        for trade in trades {
            let mut remaining = trade.size;
            while remaining > 0.0 {
                let capacity = config.bucket_size - accumulated;
                let fill = remaining.min(capacity);

                if trade.side >= 0 {
                    current_buy += fill;
                } else {
                    current_sell += fill;
                }

                remaining -= fill;
                accumulated += fill;

                if accumulated >= config.bucket_size {
                    buckets.push((current_buy, current_sell));
                    current_buy = 0.0;
                    current_sell = 0.0;
                    accumulated = 0.0;
                }
            }
        }

        // Include any partially-filled final bucket.
        if accumulated > 0.0 {
            buckets.push((current_buy, current_sell));
        }

        buckets
    }

    /// Computes the rolling VPIN series from filled buckets.
    ///
    /// `VPIN[i] = mean(|buy_vol - sell_vol| / (buy_vol + sell_vol))` over a window of `window` buckets.
    ///
    /// Returns a `Vec<f64>` of length `buckets.len()`. Values before warm-up are `f64::NAN`.
    pub fn compute_vpin(buckets: &[(f64, f64)], window: usize) -> Vec<f64> {
        let n = buckets.len();
        let mut result = vec![f64::NAN; n];

        if window == 0 || n < window {
            return result;
        }

        for i in (window - 1)..n {
            let slice = &buckets[(i + 1 - window)..=i];
            let sum: f64 = slice
                .iter()
                .map(|(b, s)| {
                    let total = b + s;
                    if total == 0.0 { 0.0 } else { (b - s).abs() / total }
                })
                .sum::<f64>();
            result[i] = sum / window as f64;
        }

        result
    }

    /// Returns `true` if `vpin > threshold` (toxic order flow).
    pub fn is_toxic(vpin: f64, threshold: f64) -> bool {
        vpin > threshold
    }
}

/// Order flow imbalance metrics: net flow, price impact regression, and cumulative delta.
pub struct OrderFlowImbalanceMetrics;

impl OrderFlowImbalanceMetrics {
    /// Computes the net signed volume (sum of `side * size`) in the time window `[now_ms - window_ms, now_ms]`.
    pub fn net_order_flow(trades: &[Trade], window_ms: u64, now_ms: u64) -> f64 {
        let start_ms = now_ms.saturating_sub(window_ms);
        trades
            .iter()
            .filter(|t| t.timestamp_ms >= start_ms && t.timestamp_ms <= now_ms)
            .map(|t| t.side as f64 * t.size)
            .sum()
    }

    /// Computes OLS regression of price changes on signed volume:
    /// `Δprice = α + β * signed_volume`
    ///
    /// Returns `(alpha, beta)`. If fewer than 2 trades, returns `(0.0, 0.0)`.
    pub fn price_impact_regression(trades: &[Trade]) -> (f64, f64) {
        if trades.len() < 2 {
            return (0.0, 0.0);
        }

        // Build (x=signed_volume, y=price_change) pairs from consecutive trades.
        let n = trades.len() - 1;
        let x: Vec<f64> = trades[..n]
            .iter()
            .map(|t| t.side as f64 * t.size)
            .collect();
        let y: Vec<f64> = (1..=n)
            .map(|i| trades[i].price - trades[i - 1].price)
            .collect();

        let n_f = n as f64;
        let mean_x = x.iter().sum::<f64>() / n_f;
        let mean_y = y.iter().sum::<f64>() / n_f;

        let ss_xx: f64 = x.iter().map(|xi| (xi - mean_x).powi(2)).sum();
        let ss_xy: f64 = x.iter().zip(y.iter()).map(|(xi, yi)| (xi - mean_x) * (yi - mean_y)).sum();

        let beta = if ss_xx == 0.0 { 0.0 } else { ss_xy / ss_xx };
        let alpha = mean_y - beta * mean_x;

        (alpha, beta)
    }

    /// Computes the running cumulative delta (sum of `side * size`) for each trade.
    ///
    /// Returns a `Vec<f64>` of the same length as `trades`.
    pub fn cumulative_delta(trades: &[Trade]) -> Vec<f64> {
        let mut result = Vec::with_capacity(trades.len());
        let mut running = 0.0_f64;
        for t in trades {
            running += t.side as f64 * t.size;
            result.push(running);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trade(price: f64, size: f64, side: i8, ts: u64) -> Trade {
        Trade { price, size, side, timestamp_ms: ts }
    }

    fn sample_trades() -> Vec<Trade> {
        vec![
            make_trade(100.0, 10.0,  1,  1000),
            make_trade(100.1, 15.0, -1,  2000),
            make_trade(100.2,  5.0,  1,  3000),
            make_trade( 99.9, 20.0, -1,  4000),
            make_trade(100.3, 12.0,  1,  5000),
            make_trade(100.4,  8.0,  1,  6000),
            make_trade(100.0, 18.0, -1,  7000),
            make_trade( 99.8, 11.0, -1,  8000),
        ]
    }

    // --------------- VpinCalculator tests ---------------

    #[test]
    fn test_classify_trades_bucket_count() {
        let trades = sample_trades();
        let config = VpinConfig { bucket_size: 30.0, window_buckets: 2 };
        let buckets = VpinCalculator::classify_trades(&trades, &config);
        // Total volume = 99; 3 full buckets of 30 + 1 partial bucket
        assert!(buckets.len() >= 3, "expected at least 3 buckets, got {}", buckets.len());
    }

    #[test]
    fn test_classify_trades_volume_conservation() {
        let trades = sample_trades();
        let total_vol: f64 = trades.iter().map(|t| t.size).sum();
        let config = VpinConfig { bucket_size: 20.0, window_buckets: 2 };
        let buckets = VpinCalculator::classify_trades(&trades, &config);
        let bucket_vol: f64 = buckets.iter().map(|(b, s)| b + s).sum();
        assert!((bucket_vol - total_vol).abs() < 1e-6,
            "volume lost: total={total_vol}, bucketed={bucket_vol}");
    }

    #[test]
    fn test_classify_trades_non_negative() {
        let trades = sample_trades();
        let config = VpinConfig { bucket_size: 15.0, window_buckets: 3 };
        let buckets = VpinCalculator::classify_trades(&trades, &config);
        for (b, s) in &buckets {
            assert!(*b >= 0.0 && *s >= 0.0, "negative volume: buy={b}, sell={s}");
        }
    }

    #[test]
    fn test_compute_vpin_range() {
        let buckets = vec![
            (20.0, 10.0),
            (5.0, 25.0),
            (15.0, 15.0),
            (22.0, 8.0),
            (3.0, 27.0),
        ];
        let vpin = VpinCalculator::compute_vpin(&buckets, 3);
        for v in vpin.iter().filter(|v| !v.is_nan()) {
            assert!(*v >= 0.0 && *v <= 1.0, "VPIN out of [0,1]: {v}");
        }
    }

    #[test]
    fn test_compute_vpin_warm_up() {
        let buckets = vec![(10.0, 10.0); 5];
        let vpin = VpinCalculator::compute_vpin(&buckets, 3);
        assert!(vpin[0].is_nan());
        assert!(vpin[1].is_nan());
        assert!(!vpin[2].is_nan());
    }

    #[test]
    fn test_compute_vpin_balanced_buckets() {
        // Perfectly balanced: VPIN should be 0.
        let buckets = vec![(10.0, 10.0); 5];
        let vpin = VpinCalculator::compute_vpin(&buckets, 3);
        for v in vpin.iter().filter(|v| !v.is_nan()) {
            assert!(v.abs() < 1e-9, "balanced VPIN should be 0, got {v}");
        }
    }

    #[test]
    fn test_compute_vpin_one_sided() {
        // All buy volume: VPIN should be 1.
        let buckets = vec![(10.0, 0.0); 5];
        let vpin = VpinCalculator::compute_vpin(&buckets, 3);
        for v in vpin.iter().filter(|v| !v.is_nan()) {
            assert!((v - 1.0).abs() < 1e-9, "one-sided VPIN should be 1, got {v}");
        }
    }

    #[test]
    fn test_is_toxic() {
        assert!(VpinCalculator::is_toxic(0.8, 0.7));
        assert!(!VpinCalculator::is_toxic(0.6, 0.7));
        assert!(!VpinCalculator::is_toxic(0.7, 0.7));
    }

    // --------------- OrderFlowImbalanceMetrics tests ---------------

    #[test]
    fn test_net_order_flow_window() {
        let trades = sample_trades();
        // Window covers timestamps 3000..=6000
        let nof = OrderFlowImbalanceMetrics::net_order_flow(&trades, 3000, 6000);
        // Trades at ts 3000..=6000: (5, side=1), (20, side=-1), (12, side=1), (8, side=1)
        let expected = 5.0 - 20.0 + 12.0 + 8.0;
        assert!((nof - expected).abs() < 1e-9, "nof={nof}, expected={expected}");
    }

    #[test]
    fn test_net_order_flow_empty_window() {
        let trades = sample_trades();
        let nof = OrderFlowImbalanceMetrics::net_order_flow(&trades, 100, 500);
        assert_eq!(nof, 0.0, "no trades in window, nof should be 0");
    }

    #[test]
    fn test_price_impact_regression_sign() {
        // Buying pressure: price should rise with positive signed volume.
        let trades = vec![
            make_trade(100.0, 10.0,  1, 1000),
            make_trade(100.5, 20.0,  1, 2000),
            make_trade(101.0, 15.0,  1, 3000),
            make_trade(101.5, 25.0,  1, 4000),
        ];
        let (_, beta) = OrderFlowImbalanceMetrics::price_impact_regression(&trades);
        assert!(beta > 0.0, "beta should be positive for buying pressure, got {beta}");
    }

    #[test]
    fn test_price_impact_regression_too_few() {
        let trades = vec![make_trade(100.0, 10.0, 1, 1000)];
        let (alpha, beta) = OrderFlowImbalanceMetrics::price_impact_regression(&trades);
        assert_eq!(alpha, 0.0);
        assert_eq!(beta, 0.0);
    }

    #[test]
    fn test_cumulative_delta_all_buy() {
        let trades = vec![
            make_trade(100.0, 10.0, 1, 1000),
            make_trade(100.0, 20.0, 1, 2000),
            make_trade(100.0,  5.0, 1, 3000),
        ];
        let cd = OrderFlowImbalanceMetrics::cumulative_delta(&trades);
        assert_eq!(cd.len(), 3);
        assert!((cd[0] - 10.0).abs() < 1e-9);
        assert!((cd[1] - 30.0).abs() < 1e-9);
        assert!((cd[2] - 35.0).abs() < 1e-9);
    }

    #[test]
    fn test_cumulative_delta_mixed() {
        let trades = sample_trades();
        let cd = OrderFlowImbalanceMetrics::cumulative_delta(&trades);
        assert_eq!(cd.len(), trades.len());
        // Final delta = sum of (side * size)
        let expected: f64 = trades.iter().map(|t| t.side as f64 * t.size).sum();
        assert!((cd.last().unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn test_cumulative_delta_empty() {
        let cd = OrderFlowImbalanceMetrics::cumulative_delta(&[]);
        assert!(cd.is_empty());
    }
}
