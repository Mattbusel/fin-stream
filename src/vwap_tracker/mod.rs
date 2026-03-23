//! Real-time VWAP and TWAP computation with execution scheduling.
//!
//! Provides:
//! - [`VwapTracker`]: period-based VWAP with automatic interval resets.
//! - [`TwapTracker`]: rolling time-weighted average price over a sliding window.
//! - [`ExecutionScheduler`]: VWAP/TWAP/POV/Aggressive slice-based execution scheduling.

use std::collections::VecDeque;

/// Time interval for VWAP period resets.
#[derive(Debug, Clone, PartialEq)]
pub enum VwapInterval {
    /// One-minute interval.
    Minute,
    /// One-hour interval.
    Hour,
    /// One-day interval.
    Day,
    /// Custom interval in milliseconds.
    Custom(u64),
}

impl VwapInterval {
    /// Duration of the interval in milliseconds.
    pub fn as_ms(&self) -> u64 {
        match self {
            VwapInterval::Minute     => 60_000,
            VwapInterval::Hour       => 3_600_000,
            VwapInterval::Day        => 86_400_000,
            VwapInterval::Custom(ms) => *ms,
        }
    }
}

/// A single VWAP data point.
#[derive(Debug, Clone)]
pub struct VwapPoint {
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Trade price.
    pub price: f64,
    /// Trade volume.
    pub volume: f64,
    /// Cumulative volume since period start.
    pub cumulative_volume: f64,
    /// VWAP from period start to this point.
    pub vwap: f64,
}

/// Streaming VWAP tracker with automatic period resets.
#[derive(Debug)]
pub struct VwapTracker {
    /// Symbol being tracked.
    pub symbol: String,
    /// Interval at which periods reset.
    pub interval: VwapInterval,
    /// Historical points (all periods).
    pub points: Vec<VwapPoint>,
    /// Start of the current period (milliseconds).
    pub period_start_ms: u64,
    /// Sum of price * volume in the current period.
    pub sum_pv: f64,
    /// Sum of volume in the current period.
    pub sum_v: f64,
    /// Cumulative volume in the current period.
    pub period_volume: f64,
}

impl VwapTracker {
    /// Create a new `VwapTracker`.
    pub fn new(symbol: &str, interval: VwapInterval) -> Self {
        Self {
            symbol: symbol.to_string(),
            interval,
            points: Vec::new(),
            period_start_ms: 0,
            sum_pv: 0.0,
            sum_v: 0.0,
            period_volume: 0.0,
        }
    }

    /// Record a trade and return the resulting [`VwapPoint`].
    ///
    /// Resets the period accumulator when `timestamp_ms` crosses the period boundary.
    pub fn add_trade(&mut self, price: f64, volume: f64, timestamp_ms: u64) -> VwapPoint {
        let interval_ms = self.interval.as_ms();

        // Initialise period start on first trade.
        if self.period_start_ms == 0 {
            self.period_start_ms = timestamp_ms;
        }

        // Reset on period boundary.
        if timestamp_ms >= self.period_start_ms + interval_ms {
            let periods_elapsed = (timestamp_ms - self.period_start_ms) / interval_ms;
            self.period_start_ms += periods_elapsed * interval_ms;
            self.sum_pv = 0.0;
            self.sum_v = 0.0;
            self.period_volume = 0.0;
        }

        self.sum_pv += price * volume;
        self.sum_v += volume;
        self.period_volume += volume;

        let vwap = if self.sum_v > 0.0 { self.sum_pv / self.sum_v } else { price };

        let pt = VwapPoint {
            timestamp_ms,
            price,
            volume,
            cumulative_volume: self.period_volume,
            vwap,
        };
        self.points.push(pt.clone());
        pt
    }

    /// Current VWAP value (0.0 if no trades in current period).
    pub fn current_vwap(&self) -> f64 {
        if self.sum_v > 0.0 { self.sum_pv / self.sum_v } else { 0.0 }
    }
}

/// Rolling time-weighted average price tracker.
#[derive(Debug)]
pub struct TwapTracker {
    /// (timestamp_ms, price) pairs within the window.
    pub prices: VecDeque<(u64, f64)>,
    /// Rolling window size in milliseconds.
    pub window_ms: u64,
}

impl TwapTracker {
    /// Create a new `TwapTracker` with the given window.
    pub fn new(window_ms: u64) -> Self {
        Self { prices: VecDeque::new(), window_ms }
    }

    /// Add a price observation.
    pub fn add_price(&mut self, price: f64, timestamp_ms: u64) {
        self.prices.push_back((timestamp_ms, price));
    }

    /// Compute the time-weighted average price up to `now_ms`.
    ///
    /// Evicts observations older than `now_ms - window_ms`, then computes
    /// the simple average of remaining prices (equal time-weight approximation).
    pub fn current_twap(&mut self, now_ms: u64) -> f64 {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        while let Some(&(ts, _)) = self.prices.front() {
            if ts < cutoff {
                self.prices.pop_front();
            } else {
                break;
            }
        }
        if self.prices.is_empty() {
            return 0.0;
        }
        // Time-weighted: weight each interval between consecutive observations.
        let n = self.prices.len();
        if n == 1 {
            return self.prices[0].1;
        }
        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;
        for i in 1..n {
            let (t0, p0) = self.prices[i - 1];
            let (t1, _)  = self.prices[i];
            let w = (t1 - t0) as f64;
            weighted_sum += p0 * w;
            total_weight += w;
        }
        // Add last segment to now_ms.
        let (t_last, p_last) = self.prices[n - 1];
        let w_last = now_ms.saturating_sub(t_last) as f64;
        weighted_sum += p_last * w_last;
        total_weight += w_last;

        if total_weight > 0.0 { weighted_sum / total_weight } else { self.prices[n - 1].1 }
    }
}

/// Execution algorithm type.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionAlgo {
    /// Volume-Weighted Average Price participation.
    VWAP,
    /// Time-Weighted Average Price (equal slices over time).
    TWAP,
    /// Participate at a fixed fraction of market volume.
    POV(f64),
    /// Aggressive: execute as quickly as possible.
    Aggressive,
}

/// Defines an execution program: total qty, time window, number of slices, and algorithm.
#[derive(Debug, Clone)]
pub struct ExecutionSchedule {
    /// Total quantity to execute.
    pub total_qty: f64,
    /// Schedule start time (ms).
    pub start_ms: u64,
    /// Schedule end time (ms).
    pub end_ms: u64,
    /// Number of time slices.
    pub num_slices: usize,
    /// Execution algorithm.
    pub algo: ExecutionAlgo,
}

/// A single time slice in the execution schedule.
#[derive(Debug, Clone)]
pub struct ScheduleSlice {
    /// Zero-based index of this slice.
    pub slice_index: usize,
    /// Target execution time for this slice (ms).
    pub target_time_ms: u64,
    /// Target quantity for this slice.
    pub target_qty: f64,
    /// Quantity actually executed in this slice.
    pub executed_qty: f64,
}

/// Manages execution slices and tracks progress against an `ExecutionSchedule`.
#[derive(Debug)]
pub struct ExecutionScheduler {
    /// The underlying schedule.
    pub schedule: ExecutionSchedule,
    /// All slices split from the schedule.
    pub slices: Vec<ScheduleSlice>,
    /// VWAP tracker for the scheduled symbol.
    pub vwap_tracker: VwapTracker,
}

impl ExecutionScheduler {
    /// Create a new scheduler. Splits total quantity evenly into slices.
    pub fn new(schedule: ExecutionSchedule, symbol: &str) -> Self {
        let n = schedule.num_slices.max(1);
        let qty_per_slice = schedule.total_qty / n as f64;
        let duration = schedule.end_ms.saturating_sub(schedule.start_ms);
        let slice_dur = duration / n as u64;

        let slices: Vec<ScheduleSlice> = (0..n).map(|i| {
            let target_time_ms = schedule.start_ms + i as u64 * slice_dur + slice_dur / 2;
            ScheduleSlice {
                slice_index: i,
                target_time_ms,
                target_qty: qty_per_slice,
                executed_qty: 0.0,
            }
        }).collect();

        let interval = VwapInterval::Custom(slice_dur.max(1));
        Self {
            vwap_tracker: VwapTracker::new(symbol, interval),
            schedule,
            slices,
        }
    }

    /// Return the active slice at `now_ms`, or `None` if outside the schedule.
    pub fn current_slice(&self, now_ms: u64) -> Option<&ScheduleSlice> {
        let n = self.slices.len();
        if n == 0 { return None; }
        let duration = self.schedule.end_ms.saturating_sub(self.schedule.start_ms);
        let slice_dur = (duration / n as u64).max(1);
        if now_ms < self.schedule.start_ms || now_ms >= self.schedule.end_ms {
            return None;
        }
        let elapsed = now_ms - self.schedule.start_ms;
        let idx = (elapsed / slice_dur).min(n as u64 - 1) as usize;
        self.slices.get(idx)
    }

    /// Record an execution against the specified slice.
    pub fn record_execution(&mut self, slice_index: usize, qty: f64, price: f64, now_ms: u64) {
        if let Some(slice) = self.slices.get_mut(slice_index) {
            slice.executed_qty += qty;
        }
        self.vwap_tracker.add_trade(price, qty, now_ms);
    }

    /// Total executed / total target quantity.
    pub fn participation_rate(&self) -> f64 {
        let executed: f64 = self.slices.iter().map(|s| s.executed_qty).sum();
        if self.schedule.total_qty > 0.0 {
            executed / self.schedule.total_qty
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vwap_weighted_correctly() {
        let mut tracker = VwapTracker::new("AAPL", VwapInterval::Minute);
        tracker.add_trade(100.0, 10.0, 1000);
        tracker.add_trade(200.0, 10.0, 2000);
        // VWAP = (100*10 + 200*10) / 20 = 150.
        let vwap = tracker.current_vwap();
        assert!((vwap - 150.0).abs() < 1e-9, "VWAP={vwap}");
    }

    #[test]
    fn test_vwap_period_reset() {
        let mut tracker = VwapTracker::new("AAPL", VwapInterval::Minute);
        tracker.add_trade(100.0, 10.0, 1_000);           // period 1
        tracker.add_trade(200.0, 10.0, 61_000);          // period 2 (reset)
        // After reset, only the second trade counts.
        let vwap = tracker.current_vwap();
        assert!((vwap - 200.0).abs() < 1e-9, "VWAP after reset should be 200, got {vwap}");
    }

    #[test]
    fn test_twap_evicts_old_prices() {
        let mut twap = TwapTracker::new(60_000); // 60-second window
        twap.add_price(100.0, 0);
        twap.add_price(200.0, 30_000);
        twap.add_price(300.0, 90_000);
        // At now=120_000, the first two are outside the 60s window (cutoff=60_000).
        // Only 300 remains (ts=90_000 >= 60_000).
        let t = twap.current_twap(120_000);
        // With only one remaining point or only 300 and 300, result should be 300.
        assert!((t - 300.0).abs() < 1.0, "TWAP after eviction should be ~300, got {t}");
    }

    #[test]
    fn test_schedule_slices_sum_to_total() {
        let schedule = ExecutionSchedule {
            total_qty: 1000.0,
            start_ms: 0,
            end_ms: 3_600_000,
            num_slices: 10,
            algo: ExecutionAlgo::TWAP,
        };
        let scheduler = ExecutionScheduler::new(schedule, "AAPL");
        let total: f64 = scheduler.slices.iter().map(|s| s.target_qty).sum();
        assert!((total - 1000.0).abs() < 1e-9, "Slices should sum to total_qty, got {total}");
        assert_eq!(scheduler.slices.len(), 10);
    }

    #[test]
    fn test_participation_rate() {
        let schedule = ExecutionSchedule {
            total_qty: 100.0,
            start_ms: 0,
            end_ms: 100_000,
            num_slices: 4,
            algo: ExecutionAlgo::VWAP,
        };
        let mut scheduler = ExecutionScheduler::new(schedule, "TSLA");
        scheduler.record_execution(0, 25.0, 150.0, 10_000);
        scheduler.record_execution(1, 25.0, 151.0, 30_000);
        let rate = scheduler.participation_rate();
        assert!((rate - 0.5).abs() < 1e-9, "Participation rate should be 0.5, got {rate}");
    }

    #[test]
    fn test_vwap_interval_as_ms() {
        assert_eq!(VwapInterval::Minute.as_ms(), 60_000);
        assert_eq!(VwapInterval::Hour.as_ms(), 3_600_000);
        assert_eq!(VwapInterval::Day.as_ms(), 86_400_000);
        assert_eq!(VwapInterval::Custom(5000).as_ms(), 5000);
    }
}
