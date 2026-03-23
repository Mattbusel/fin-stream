//! Trading session analysis: intraday volume/return patterns and seasonality metrics.
//!
//! [`SeasonalityAnalyzer`] accumulates session-level statistics and provides
//! intraday bucket profiles, day-of-week return effects, and session pattern classification.

use std::collections::HashMap;

/// Aggregate statistics for a single trading session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStats {
    /// Opening price of the session.
    pub open_price: f64,
    /// Closing price of the session.
    pub close_price: f64,
    /// Intraday high price.
    pub high: f64,
    /// Intraday low price.
    pub low: f64,
    /// Total session volume.
    pub volume: f64,
    /// Volume-weighted average price for the session.
    pub vwap: f64,
    /// Number of individual trades in the session.
    pub num_trades: u64,
}

/// Aggregated statistics for a specific intraday time bucket (hour:minute).
#[derive(Debug, Clone, PartialEq)]
pub struct IntradayBucket {
    /// Hour of day (0–23).
    pub hour: u8,
    /// Minute of hour (0–59).
    pub minute: u8,
    /// Average volume across all observations in this bucket.
    pub avg_volume: f64,
    /// Average return across all observations in this bucket.
    pub avg_return: f64,
    /// Average volatility (absolute return) across all observations.
    pub avg_volatility: f64,
    /// Number of observations used to compute the averages.
    pub num_observations: u64,
}

/// Classification of intraday volume/activity profile shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPattern {
    /// High volume at open and close, low volume mid-session.
    UShape,
    /// Elevated volume at open, declining toward close.
    LShape,
    /// Low volume at open, increasing toward close.
    JShape,
    /// Roughly uniform volume distribution throughout the day.
    Uniform,
}

/// Internal accumulator for one intraday bucket.
#[derive(Debug, Default, Clone)]
struct BucketAccum {
    sum_volume: f64,
    sum_return: f64,
    sum_volatility: f64,
    count: u64,
}

/// Internal record of a single session.
#[derive(Debug, Clone)]
struct SessionRecord {
    stats: SessionStats,
    day_of_week: u8,
    #[allow(dead_code)]
    date_epoch_days: u64,
}

/// Accumulates trading session data and answers seasonality queries.
///
/// Sessions are keyed by symbol; if you only work with a single instrument
/// pass the same symbol on every call.
#[derive(Debug, Default)]
pub struct SeasonalityAnalyzer {
    /// Per-symbol session history.
    sessions: HashMap<String, Vec<SessionRecord>>,
    /// Per-symbol intraday bucket accumulators. Key: (symbol, hour, minute).
    buckets: HashMap<(String, u8, u8), BucketAccum>,
}

impl SeasonalityAnalyzer {
    /// Create a new, empty analyzer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed trading session for a symbol.
    ///
    /// `day_of_week`: 0 = Sunday … 6 = Saturday (ISO: 1 = Monday … 7 = Sunday is also accepted;
    /// callers should be consistent).
    /// `date_epoch_days`: days since Unix epoch (1970-01-01 = 0).
    pub fn record_session(&mut self, symbol: &str, stats: SessionStats, day_of_week: u8, date_epoch_days: u64) {
        self.sessions
            .entry(symbol.to_string())
            .or_default()
            .push(SessionRecord {
                stats,
                day_of_week,
                date_epoch_days,
            });
    }

    /// Record an intraday trade bucket observation for a symbol.
    ///
    /// Call this for each time-of-day slot with the volume, return, and volatility
    /// observed in that slot during a single session.
    pub fn record_bucket(
        &mut self,
        symbol: &str,
        hour: u8,
        minute: u8,
        volume: f64,
        return_val: f64,
        volatility: f64,
    ) {
        let key = (symbol.to_string(), hour, minute);
        let acc = self.buckets.entry(key).or_default();
        acc.sum_volume += volume;
        acc.sum_return += return_val;
        acc.sum_volatility += volatility;
        acc.count += 1;
    }

    /// Return the averaged intraday profile for `symbol`, sorted by (hour, minute).
    pub fn intraday_profile(&self, symbol: &str) -> Vec<IntradayBucket> {
        let mut buckets: Vec<IntradayBucket> = self
            .buckets
            .iter()
            .filter(|((sym, _, _), _)| sym == symbol)
            .map(|((_, hour, minute), acc)| {
                let n = acc.count.max(1) as f64;
                IntradayBucket {
                    hour: *hour,
                    minute: *minute,
                    avg_volume: acc.sum_volume / n,
                    avg_return: acc.sum_return / n,
                    avg_volatility: acc.sum_volatility / n,
                    num_observations: acc.count,
                }
            })
            .collect();
        buckets.sort_by_key(|b| (b.hour, b.minute));
        buckets
    }

    /// Return the average session return by day of week.
    ///
    /// Index 0 = day 0 (Sunday or Monday, consistent with `record_session`).
    /// Returns `[0.0; 7]` if no data is recorded.
    pub fn day_of_week_effect(&self, symbol: &str) -> [f64; 7] {
        let mut sum = [0.0f64; 7];
        let mut count = [0u64; 7];
        if let Some(sessions) = self.sessions.get(symbol) {
            for rec in sessions {
                let dow = (rec.day_of_week as usize) % 7;
                let ret = if rec.stats.open_price > 0.0 {
                    (rec.stats.close_price - rec.stats.open_price) / rec.stats.open_price
                } else {
                    0.0
                };
                sum[dow] += ret;
                count[dow] += 1;
            }
        }
        let mut result = [0.0f64; 7];
        for i in 0..7 {
            if count[i] > 0 {
                result[i] = sum[i] / count[i] as f64;
            }
        }
        result
    }

    /// Classify the intraday session pattern from a bucket profile.
    ///
    /// Classification rules (based on relative volume):
    /// - **UShape**: first-third avg volume AND last-third avg volume both exceed mid-third.
    /// - **LShape**: first-third average > last-third average AND last < mid (declining).
    /// - **JShape**: last-third average > first-third average AND first < mid (rising).
    /// - **Uniform**: otherwise.
    pub fn detect_session_pattern(buckets: &[IntradayBucket]) -> SessionPattern {
        let n = buckets.len();
        if n < 3 {
            return SessionPattern::Uniform;
        }
        let third = n / 3;
        let first_avg: f64 = buckets[..third]
            .iter()
            .map(|b| b.avg_volume)
            .sum::<f64>()
            / third as f64;
        let mid_avg: f64 = buckets[third..2 * third]
            .iter()
            .map(|b| b.avg_volume)
            .sum::<f64>()
            / third as f64;
        let last_avg: f64 = buckets[2 * third..]
            .iter()
            .map(|b| b.avg_volume)
            .sum::<f64>()
            / (n - 2 * third) as f64;

        if first_avg > mid_avg && last_avg > mid_avg {
            SessionPattern::UShape
        } else if first_avg > last_avg && last_avg <= mid_avg {
            SessionPattern::LShape
        } else if last_avg > first_avg && first_avg <= mid_avg {
            SessionPattern::JShape
        } else {
            SessionPattern::Uniform
        }
    }

    /// Return the (hour, minute) pairs whose average volume is at or above
    /// `min_volume_pct` (0.0–1.0) of the maximum bucket volume.
    ///
    /// Useful for identifying "best trading hours" by volume activity.
    pub fn best_trading_hours(&self, symbol: &str, min_volume_pct: f64) -> Vec<(u8, u8)> {
        let profile = self.intraday_profile(symbol);
        if profile.is_empty() {
            return Vec::new();
        }
        let max_vol = profile
            .iter()
            .map(|b| b.avg_volume)
            .fold(f64::NEG_INFINITY, f64::max);
        if max_vol <= 0.0 {
            return Vec::new();
        }
        let threshold = max_vol * min_volume_pct;
        profile
            .into_iter()
            .filter(|b| b.avg_volume >= threshold)
            .map(|b| (b.hour, b.minute))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats(open: f64, close: f64) -> SessionStats {
        SessionStats {
            open_price: open,
            close_price: close,
            high: close.max(open) + 1.0,
            low: close.min(open) - 1.0,
            volume: 100_000.0,
            vwap: (open + close) / 2.0,
            num_trades: 500,
        }
    }

    #[test]
    fn test_record_and_profile() {
        let mut analyzer = SeasonalityAnalyzer::new();
        analyzer.record_bucket("AAPL", 9, 30, 50000.0, 0.002, 0.001);
        analyzer.record_bucket("AAPL", 9, 30, 60000.0, 0.001, 0.002);
        analyzer.record_bucket("AAPL", 10, 0, 20000.0, 0.0, 0.0005);
        let profile = analyzer.intraday_profile("AAPL");
        assert_eq!(profile.len(), 2);
        assert_eq!(profile[0].hour, 9);
        assert_eq!(profile[0].minute, 30);
        assert!((profile[0].avg_volume - 55000.0).abs() < 1e-6);
        assert_eq!(profile[0].num_observations, 2);
    }

    #[test]
    fn test_day_of_week_effect() {
        let mut analyzer = SeasonalityAnalyzer::new();
        // Monday (1): +2% session
        analyzer.record_session("SPY", make_stats(100.0, 102.0), 1, 1000);
        // Monday: -1% session
        analyzer.record_session("SPY", make_stats(100.0, 99.0), 1, 1007);
        // Wednesday (3): +3% session
        analyzer.record_session("SPY", make_stats(100.0, 103.0), 3, 1002);
        let effect = analyzer.day_of_week_effect("SPY");
        // Monday avg = (0.02 + (-0.01)) / 2 = 0.005
        assert!((effect[1] - 0.005).abs() < 1e-9);
        // Wednesday avg = 0.03
        assert!((effect[3] - 0.03).abs() < 1e-9);
        // Other days = 0
        assert_eq!(effect[0], 0.0);
    }

    #[test]
    fn test_detect_ushape() {
        let buckets = vec![
            IntradayBucket { hour: 9, minute: 30, avg_volume: 100.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 10, minute: 0, avg_volume: 30.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 10, minute: 30, avg_volume: 20.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 11, minute: 0, avg_volume: 25.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 11, minute: 30, avg_volume: 40.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 15, minute: 30, avg_volume: 120.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
        ];
        assert_eq!(SeasonalityAnalyzer::detect_session_pattern(&buckets), SessionPattern::UShape);
    }

    #[test]
    fn test_detect_lshape() {
        let buckets = vec![
            IntradayBucket { hour: 9, minute: 30, avg_volume: 100.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 11, minute: 0, avg_volume: 60.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 13, minute: 0, avg_volume: 20.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
        ];
        assert_eq!(SeasonalityAnalyzer::detect_session_pattern(&buckets), SessionPattern::LShape);
    }

    #[test]
    fn test_detect_jshape() {
        let buckets = vec![
            IntradayBucket { hour: 9, minute: 30, avg_volume: 20.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 12, minute: 0, avg_volume: 30.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 15, minute: 30, avg_volume: 100.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
        ];
        assert_eq!(SeasonalityAnalyzer::detect_session_pattern(&buckets), SessionPattern::JShape);
    }

    #[test]
    fn test_detect_uniform() {
        let buckets = vec![
            IntradayBucket { hour: 9, minute: 30, avg_volume: 50.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 12, minute: 0, avg_volume: 52.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 15, minute: 30, avg_volume: 48.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
        ];
        assert_eq!(SeasonalityAnalyzer::detect_session_pattern(&buckets), SessionPattern::Uniform);
    }

    #[test]
    fn test_best_trading_hours() {
        let mut analyzer = SeasonalityAnalyzer::new();
        analyzer.record_bucket("SPY", 9, 30, 100_000.0, 0.001, 0.002);
        analyzer.record_bucket("SPY", 12, 0, 20_000.0, 0.0, 0.001);
        analyzer.record_bucket("SPY", 15, 30, 90_000.0, 0.002, 0.002);
        // 80% threshold — 9:30 and 15:30 qualify (100k and 90k >= 80k)
        let hours = analyzer.best_trading_hours("SPY", 0.80);
        assert!(hours.contains(&(9, 30)));
        assert!(hours.contains(&(15, 30)));
        assert!(!hours.contains(&(12, 0)));
    }

    #[test]
    fn test_empty_profile() {
        let analyzer = SeasonalityAnalyzer::new();
        assert!(analyzer.intraday_profile("MISSING").is_empty());
        assert!(analyzer.best_trading_hours("MISSING", 0.5).is_empty());
        let effect = analyzer.day_of_week_effect("MISSING");
        assert_eq!(effect, [0.0; 7]);
    }

    #[test]
    fn test_detect_pattern_too_few_buckets() {
        let buckets = vec![
            IntradayBucket { hour: 9, minute: 30, avg_volume: 100.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
            IntradayBucket { hour: 10, minute: 0, avg_volume: 50.0, avg_return: 0.0, avg_volatility: 0.0, num_observations: 1 },
        ];
        assert_eq!(SeasonalityAnalyzer::detect_session_pattern(&buckets), SessionPattern::Uniform);
    }
}
