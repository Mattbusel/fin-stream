//! Flow toxicity via VPIN (Volume-synchronised Probability of Informed Trading).
//!
//! ## Responsibility
//!
//! Estimate the probability that recent order flow is driven by informed
//! traders (high VPIN = toxic flow) using the algorithm of Easley, López de
//! Prado & O'Hara (2012).
//!
//! ## Algorithm
//!
//! 1. **Volume bucketing**: accumulate trades until a fixed volume threshold
//!    `V_bucket` is reached; each such accumulation defines one *bucket*.
//! 2. **Imbalance per bucket**: classify each trade as buyer-initiated or
//!    seller-initiated using the tick-test rule (trade at or above the last
//!    price → buy, below → sell).  Bucket imbalance =
//!    `|buy_vol - sell_vol| / total_vol`.
//! 3. **VPIN**: rolling average of bucket imbalances over the last `n` buckets.
//!
//! High VPIN (> configurable alert threshold) signals that informed traders are
//! taking liquidity, which is associated with adverse selection and recommends
//! wider bid-ask spreads.
//!
//! ## Guarantees
//! - Non-panicking: all operations return `Result` or `Option`.
//! - Memory bounded: only the last `n_buckets` completed buckets are retained.
//!
//! ## Usage
//!
//! ```rust
//! use fin_stream::toxicity::{VpinCalculator, VpinConfig, TradeTick};
//!
//! let cfg = VpinConfig::default();
//! let mut calc = VpinCalculator::new(cfg);
//!
//! calc.update(TradeTick { price: 100.0, volume: 50.0 });
//! calc.update(TradeTick { price: 100.05, volume: 80.0 });
//!
//! if let Some(vpin) = calc.vpin() {
//!     println!("VPIN = {vpin:.4}");
//! }
//! ```

/// A single trade tick used for VPIN computation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TradeTick {
    /// Trade price.
    pub price: f64,
    /// Trade volume (always positive).
    pub volume: f64,
}

/// Configuration for the VPIN calculator.
#[derive(Debug, Clone)]
pub struct VpinConfig {
    /// Volume per bucket `V`.  When the accumulated volume in the current
    /// bucket reaches this value, the bucket is finalised.
    pub bucket_volume: f64,

    /// Rolling window: number of recent completed buckets to average over.
    pub n_buckets: usize,

    /// VPIN value above which toxic flow is flagged.
    pub alert_threshold: f64,
}

impl Default for VpinConfig {
    fn default() -> Self {
        Self {
            bucket_volume: 1000.0,
            n_buckets: 50,
            alert_threshold: 0.70,
        }
    }
}

/// A finalised volume bucket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeBucket {
    /// Buyer-initiated volume in this bucket.
    pub buy_volume: f64,
    /// Seller-initiated volume in this bucket.
    pub sell_volume: f64,
    /// Order-flow imbalance `|buy_vol - sell_vol| / total_vol`.
    pub imbalance: f64,
}

/// VPIN alert level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToxicityAlert {
    /// VPIN below threshold — flow appears uninformed.
    Normal,
    /// VPIN above threshold — elevated probability of informed trading.
    Toxic,
}

impl std::fmt::Display for ToxicityAlert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToxicityAlert::Normal => write!(f, "Normal"),
            ToxicityAlert::Toxic => write!(f, "Toxic"),
        }
    }
}

/// Online VPIN estimator.
#[derive(Debug, Clone)]
pub struct VpinCalculator {
    cfg: VpinConfig,
    /// Last observed trade price for the tick-test rule.
    last_price: Option<f64>,
    /// Accumulated buy volume in the current (open) bucket.
    current_buy_vol: f64,
    /// Accumulated sell volume in the current (open) bucket.
    current_sell_vol: f64,
    /// Completed buckets ring buffer (capped at `n_buckets`).
    buckets: std::collections::VecDeque<VolumeBucket>,
    /// Total number of buckets completed since construction.
    bucket_count: u64,
}

impl VpinCalculator {
    /// Create a new calculator.
    pub fn new(cfg: VpinConfig) -> Self {
        Self {
            cfg,
            last_price: None,
            current_buy_vol: 0.0,
            current_sell_vol: 0.0,
            buckets: std::collections::VecDeque::new(),
            bucket_count: 0,
        }
    }

    /// Ingest a single trade tick.
    ///
    /// Internally finalises one or more buckets if the accumulated volume
    /// crosses the threshold.  Returns the number of buckets finalised.
    pub fn update(&mut self, tick: TradeTick) -> usize {
        if tick.volume <= 0.0 {
            return 0;
        }

        // Tick-test: classify as buy or sell.
        let is_buy = match self.last_price {
            Some(prev) => tick.price >= prev,
            None => true, // first tick: assume buy
        };
        self.last_price = Some(tick.price);

        let mut remaining = tick.volume;
        let mut finalised = 0;

        while remaining > 0.0 {
            let current_total = self.current_buy_vol + self.current_sell_vol;
            let capacity = (self.cfg.bucket_volume - current_total).max(0.0);

            let fill = remaining.min(capacity);
            if is_buy {
                self.current_buy_vol += fill;
            } else {
                self.current_sell_vol += fill;
            }
            remaining -= fill;

            // Check if bucket is full.
            let new_total = self.current_buy_vol + self.current_sell_vol;
            if new_total >= self.cfg.bucket_volume - 1e-9 {
                self.finalise_bucket();
                finalised += 1;
            }
        }

        finalised
    }

    /// Current VPIN estimate: rolling mean of bucket imbalances.
    /// Returns `None` if fewer than 2 buckets have been completed.
    pub fn vpin(&self) -> Option<f64> {
        if self.buckets.len() < 2 {
            return None;
        }
        let sum: f64 = self.buckets.iter().map(|b| b.imbalance).sum();
        Some(sum / self.buckets.len() as f64)
    }

    /// Current toxicity alert level.
    pub fn alert(&self) -> ToxicityAlert {
        match self.vpin() {
            Some(v) if v >= self.cfg.alert_threshold => ToxicityAlert::Toxic,
            _ => ToxicityAlert::Normal,
        }
    }

    /// All completed buckets (most recent `n_buckets`).
    pub fn buckets(&self) -> &std::collections::VecDeque<VolumeBucket> {
        &self.buckets
    }

    /// Total number of buckets finalised since construction.
    pub fn bucket_count(&self) -> u64 {
        self.bucket_count
    }

    /// Volume accumulated in the current open bucket.
    pub fn current_bucket_volume(&self) -> f64 {
        self.current_buy_vol + self.current_sell_vol
    }

    // ── private ───────────────────────────────────────────────────────────────

    fn finalise_bucket(&mut self) {
        let total = self.current_buy_vol + self.current_sell_vol;
        let imbalance = if total > 1e-12 {
            (self.current_buy_vol - self.current_sell_vol).abs() / total
        } else {
            0.0
        };
        self.buckets.push_back(VolumeBucket {
            buy_volume: self.current_buy_vol,
            sell_volume: self.current_sell_vol,
            imbalance,
        });
        if self.buckets.len() > self.cfg.n_buckets {
            self.buckets.pop_front();
        }
        self.current_buy_vol = 0.0;
        self.current_sell_vol = 0.0;
        self.bucket_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_calc(bucket_volume: f64, n_buckets: usize) -> VpinCalculator {
        VpinCalculator::new(VpinConfig {
            bucket_volume,
            n_buckets,
            alert_threshold: 0.70,
        })
    }

    #[test]
    fn no_vpin_before_two_buckets() {
        let mut calc = make_calc(100.0, 10);
        calc.update(TradeTick { price: 100.0, volume: 50.0 });
        assert!(calc.vpin().is_none());
    }

    #[test]
    fn bucket_finalised_when_volume_reached() {
        let mut calc = make_calc(100.0, 10);
        let finalised = calc.update(TradeTick { price: 100.0, volume: 100.0 });
        assert_eq!(finalised, 1);
        assert_eq!(calc.bucket_count(), 1);
    }

    #[test]
    fn balanced_flow_yields_low_vpin() {
        let mut calc = make_calc(100.0, 10);
        // Alternate buy/sell ticks of equal volume → low imbalance.
        let mut price = 100.0_f64;
        for i in 0..20 {
            let delta = if i % 2 == 0 { 0.01 } else { -0.01 };
            price += delta;
            calc.update(TradeTick { price, volume: 50.0 });
        }
        if let Some(v) = calc.vpin() {
            assert!(v < 0.5, "balanced flow should yield VPIN < 0.5, got {v}");
        }
    }

    #[test]
    fn one_sided_buy_flow_yields_high_vpin() {
        let mut calc = make_calc(100.0, 5);
        let mut price = 100.0_f64;
        // All buy ticks → maximum imbalance.
        for _ in 0..15 {
            price += 0.01;
            calc.update(TradeTick { price, volume: 50.0 });
        }
        if let Some(v) = calc.vpin() {
            assert!(v > 0.5, "one-sided buy flow should yield VPIN > 0.5, got {v}");
        }
    }

    #[test]
    fn bucket_ring_bounded_to_n_buckets() {
        let n = 5;
        let mut calc = make_calc(100.0, n);
        let mut price = 100.0_f64;
        for _ in 0..100 {
            price += 0.01;
            calc.update(TradeTick { price, volume: 20.0 });
        }
        assert!(calc.buckets().len() <= n);
    }

    #[test]
    fn zero_volume_tick_is_ignored() {
        let mut calc = make_calc(100.0, 10);
        let finalised = calc.update(TradeTick { price: 100.0, volume: 0.0 });
        assert_eq!(finalised, 0);
        assert_eq!(calc.bucket_count(), 0);
    }
}
