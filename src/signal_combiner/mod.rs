//! IC-based alpha signal combination and orthogonalization.
//!
//! Combines multiple alpha signals using various weighting schemes,
//! computes pairwise correlations, measures diversification, and
//! orthogonalizes signals via Gram-Schmidt.

// ─── Core types ───────────────────────────────────────────────────────────────

/// A single alpha signal with associated performance metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AlphaSignal {
    /// Human-readable signal name.
    pub name: String,
    /// Signal values aligned with the asset universe or time series.
    pub values: Vec<f64>,
    /// Information coefficient ∈ [−1, 1]; higher = more predictive.
    pub ic: f64,
    /// Exponential decay factor applied when combining (not used directly in
    /// the base combination, but available for callers).
    pub decay_factor: f64,
}

/// Method used to compute combination weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CombinationMethod {
    /// 1/n weight for each signal.
    EqualWeight,
    /// Weight proportional to |IC|.
    ICWeight,
    /// Weight proportional to IC².
    ICSquaredWeight,
    /// Weight proportional to max(IC, 0) — only positive-IC signals.
    InformationRatio,
    /// Kelly-optimal: IC × (1 − IC²), clamped to non-negative.
    KellyCriterion,
}

// ─── SignalCombiner ───────────────────────────────────────────────────────────

/// Combines a set of alpha signals into a single composite signal.
#[derive(Debug, Clone)]
pub struct SignalCombiner {
    /// Registered alpha signals.
    pub signals: Vec<AlphaSignal>,
    /// Combination method.
    pub method: CombinationMethod,
}

impl SignalCombiner {
    /// Create a new combiner with the given method and no signals.
    pub fn new(method: CombinationMethod) -> Self {
        Self { signals: Vec::new(), method }
    }

    /// Add a signal to the combiner.
    pub fn add_signal(&mut self, signal: AlphaSignal) {
        self.signals.push(signal);
    }

    /// Compute normalised combination weights according to `self.method`.
    ///
    /// If all raw weights are zero the result is equal-weight.
    pub fn compute_weights(&self) -> Vec<f64> {
        let n = self.signals.len();
        if n == 0 {
            return Vec::new();
        }
        let raw: Vec<f64> = self.signals.iter().map(|s| {
            match self.method {
                CombinationMethod::EqualWeight => 1.0,
                CombinationMethod::ICWeight => s.ic.abs(),
                CombinationMethod::ICSquaredWeight => s.ic * s.ic,
                CombinationMethod::InformationRatio => s.ic.max(0.0),
                CombinationMethod::KellyCriterion => {
                    let k = s.ic * (1.0 - s.ic * s.ic);
                    k.max(0.0)
                }
            }
        }).collect();

        let total: f64 = raw.iter().sum();
        if total == 0.0 {
            // Fall back to equal weight
            vec![1.0 / n as f64; n]
        } else {
            raw.iter().map(|w| w / total).collect()
        }
    }

    /// Combine all signals into a single weighted-average value series.
    ///
    /// All signals must have the same length.
    pub fn combine(&self) -> Vec<f64> {
        if self.signals.is_empty() {
            return Vec::new();
        }
        let len = self.signals[0].values.len();
        let weights = self.compute_weights();
        let mut combined = vec![0.0_f64; len];
        for (sig, &w) in self.signals.iter().zip(weights.iter()) {
            for (c, &v) in combined.iter_mut().zip(sig.values.iter()) {
                *c += w * v;
            }
        }
        combined
    }

    /// Pearson correlation between two value slices.
    pub fn signal_correlation(a: &[f64], b: &[f64]) -> f64 {
        let n = a.len().min(b.len());
        if n < 2 {
            return 0.0;
        }
        let mean_a = a[..n].iter().sum::<f64>() / n as f64;
        let mean_b = b[..n].iter().sum::<f64>() / n as f64;
        let mut cov = 0.0_f64;
        let mut var_a = 0.0_f64;
        let mut var_b = 0.0_f64;
        for i in 0..n {
            let da = a[i] - mean_a;
            let db = b[i] - mean_b;
            cov += da * db;
            var_a += da * da;
            var_b += db * db;
        }
        let denom = (var_a * var_b).sqrt();
        if denom == 0.0 { 0.0 } else { cov / denom }
    }

    /// Average absolute pairwise correlation (lower = more diversified).
    ///
    /// Returns 0.0 if there are fewer than two signals.
    pub fn diversification_ratio(&self) -> f64 {
        let n = self.signals.len();
        if n < 2 {
            return 0.0;
        }
        let mut total = 0.0_f64;
        let mut count = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                total += Self::signal_correlation(&self.signals[i].values, &self.signals[j].values).abs();
                count += 1;
            }
        }
        if count == 0 { 0.0 } else { total / count as f64 }
    }

    /// Orthogonalize signals in-place using Gram-Schmidt.
    ///
    /// For each signal `i`, subtracts the projection onto each preceding signal `j < i`.
    pub fn orthogonalize(&mut self) {
        let n = self.signals.len();
        for i in 1..n {
            // Collect projections first to avoid borrow conflicts
            let len = self.signals[i].values.len();
            let projs: Vec<Vec<f64>> = (0..i)
                .map(|j| {
                    let a = &self.signals[j].values;
                    let b = &self.signals[i].values;
                    let dot_ab: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                    let dot_aa: f64 = a.iter().map(|x| x * x).sum();
                    if dot_aa == 0.0 {
                        vec![0.0; len]
                    } else {
                        let scale = dot_ab / dot_aa;
                        a.iter().map(|x| x * scale).collect()
                    }
                })
                .collect();
            // Apply all projections
            for proj in &projs {
                for (v, p) in self.signals[i].values.iter_mut().zip(proj.iter()) {
                    *v -= p;
                }
            }
        }
    }
}

// ─── Performance breakdown ────────────────────────────────────────────────────

/// Per-signal performance decomposition.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignalPerformance {
    /// Signal name.
    pub name: String,
    /// Information coefficient.
    pub ic: f64,
    /// Combination weight assigned to this signal.
    pub weight: f64,
    /// Contribution = corr(combined_signal, realized_returns) × weight.
    pub contribution: f64,
}

impl SignalCombiner {
    /// Decompose performance: for each signal compute its contribution to the
    /// combined signal's correlation with realized returns.
    ///
    /// `contribution` = correlation(combined_signal, realized_returns) × weight.
    pub fn performance_breakdown(&self, realized_returns: &[f64]) -> Vec<SignalPerformance> {
        let combined = self.combine();
        let weights = self.compute_weights();
        let corr_combined = Self::signal_correlation(&combined, realized_returns);
        self.signals
            .iter()
            .zip(weights.iter())
            .map(|(sig, &w)| SignalPerformance {
                name: sig.name.clone(),
                ic: sig.ic,
                weight: w,
                contribution: corr_combined * w,
            })
            .collect()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_signal(name: &str, values: Vec<f64>, ic: f64) -> AlphaSignal {
        AlphaSignal { name: name.to_string(), values, ic, decay_factor: 1.0 }
    }

    #[test]
    fn equal_weights_sum_to_one() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        combiner.add_signal(make_signal("a", vec![1.0, 2.0], 0.3));
        combiner.add_signal(make_signal("b", vec![3.0, 4.0], 0.5));
        combiner.add_signal(make_signal("c", vec![5.0, 6.0], 0.2));
        let w = combiner.compute_weights();
        assert_eq!(w.len(), 3);
        let sum: f64 = w.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10, "sum={sum}");
        for wi in &w {
            assert!((wi - 1.0 / 3.0).abs() < 1e-10);
        }
    }

    #[test]
    fn ic_weighting_proportional() {
        let mut combiner = SignalCombiner::new(CombinationMethod::ICWeight);
        // |IC| = 0.4 and 0.6 → weights 0.4 and 0.6
        combiner.add_signal(make_signal("a", vec![1.0], 0.4));
        combiner.add_signal(make_signal("b", vec![2.0], 0.6));
        let w = combiner.compute_weights();
        assert!((w[0] - 0.4).abs() < 1e-10, "w[0]={}", w[0]);
        assert!((w[1] - 0.6).abs() < 1e-10, "w[1]={}", w[1]);
    }

    #[test]
    fn ic_squared_weighting() {
        let mut combiner = SignalCombiner::new(CombinationMethod::ICSquaredWeight);
        combiner.add_signal(make_signal("a", vec![1.0], 0.5));
        combiner.add_signal(make_signal("b", vec![2.0], 1.0));
        let w = combiner.compute_weights();
        // 0.25 / 1.25 and 1.0 / 1.25
        assert!((w[0] - 0.2).abs() < 1e-10, "w[0]={}", w[0]);
        assert!((w[1] - 0.8).abs() < 1e-10, "w[1]={}", w[1]);
    }

    #[test]
    fn combine_with_two_signals() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        combiner.add_signal(make_signal("a", vec![1.0, 2.0, 3.0], 0.5));
        combiner.add_signal(make_signal("b", vec![3.0, 4.0, 5.0], 0.5));
        let combined = combiner.combine();
        // Equal weight: (1+3)/2=2, (2+4)/2=3, (3+5)/2=4
        assert!((combined[0] - 2.0).abs() < 1e-10);
        assert!((combined[1] - 3.0).abs() < 1e-10);
        assert!((combined[2] - 4.0).abs() < 1e-10);
    }

    #[test]
    fn correlation_perfect_positive() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b = vec![2.0, 4.0, 6.0, 8.0, 10.0];
        let c = SignalCombiner::signal_correlation(&a, &b);
        assert!((c - 1.0).abs() < 1e-10, "corr={c}");
    }

    #[test]
    fn correlation_perfect_negative() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b = vec![5.0, 4.0, 3.0, 2.0, 1.0];
        let c = SignalCombiner::signal_correlation(&a, &b);
        assert!((c - (-1.0)).abs() < 1e-10, "corr={c}");
    }

    #[test]
    fn diversification_ratio_uncorrelated() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        // Two orthogonal signals
        combiner.add_signal(make_signal("a", vec![1.0, -1.0, 1.0, -1.0], 0.3));
        combiner.add_signal(make_signal("b", vec![1.0, 1.0, -1.0, -1.0], 0.3));
        let dr = combiner.diversification_ratio();
        // Pearson correlation of these two signals = 0
        assert!(dr < 0.05, "dr={dr}");
    }

    #[test]
    fn diversification_ratio_correlated() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        let v = vec![1.0, 2.0, 3.0, 4.0];
        combiner.add_signal(make_signal("a", v.clone(), 0.3));
        combiner.add_signal(make_signal("b", v.clone(), 0.3));
        let dr = combiner.diversification_ratio();
        // Identical signals => |correlation| = 1
        assert!((dr - 1.0).abs() < 1e-10, "dr={dr}");
    }

    #[test]
    fn orthogonalize_makes_signals_uncorrelated() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        combiner.add_signal(make_signal("a", vec![1.0, 2.0, 3.0, 4.0], 0.5));
        // b is correlated with a
        combiner.add_signal(make_signal("b", vec![2.0, 3.0, 4.0, 5.0], 0.4));
        combiner.orthogonalize();
        let corr = SignalCombiner::signal_correlation(
            &combiner.signals[0].values,
            &combiner.signals[1].values,
        );
        assert!(corr.abs() < 1e-10, "after orthogonalize corr should be ~0, got {corr}");
    }

    #[test]
    fn information_ratio_excludes_negative_ic() {
        let mut combiner = SignalCombiner::new(CombinationMethod::InformationRatio);
        combiner.add_signal(make_signal("a", vec![1.0], -0.3)); // negative IC → 0 weight
        combiner.add_signal(make_signal("b", vec![1.0], 0.6));
        let w = combiner.compute_weights();
        assert!(w[0].abs() < 1e-10, "negative IC signal should have 0 weight, got {}", w[0]);
        assert!((w[1] - 1.0).abs() < 1e-10, "w[1]={}", w[1]);
    }

    #[test]
    fn performance_breakdown_weights_sum_to_one() {
        let mut combiner = SignalCombiner::new(CombinationMethod::EqualWeight);
        combiner.add_signal(make_signal("a", vec![1.0, 2.0, 3.0], 0.4));
        combiner.add_signal(make_signal("b", vec![2.0, 1.0, 0.5], 0.3));
        let returns = vec![1.5, 1.5, 2.0];
        let bp = combiner.performance_breakdown(&returns);
        let total_weight: f64 = bp.iter().map(|p| p.weight).sum();
        assert!((total_weight - 1.0).abs() < 1e-10);
    }
}
