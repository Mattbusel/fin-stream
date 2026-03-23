//! Signal processing pipeline: normalization, filtering, smoothing, and combination.

use std::collections::HashMap;
use std::collections::VecDeque;

/// A single signal observation.
#[derive(Debug, Clone)]
pub struct SignalValue {
    /// Signal magnitude.
    pub value: f64,
    /// Nanosecond timestamp.
    pub timestamp_ns: u64,
    /// Symbol this signal applies to.
    pub symbol: String,
    /// Source identifier.
    pub source: String,
}

/// Normalization method.
#[derive(Debug, Clone, PartialEq)]
pub enum NormMethod {
    /// Min-max scaling to [0, 1].
    MinMax,
    /// Z-score standardization.
    ZScore,
    /// Robust scaling using median and IQR.
    Robust,
}

/// A single stage in the signal processing pipeline.
#[derive(Debug, Clone)]
pub enum ProcessingStage {
    /// Normalize using the given method.
    Normalize {
        /// Normalization method.
        method: NormMethod,
    },
    /// Low-pass IIR filter with given cutoff frequency.
    Filter {
        /// Cutoff frequency (0 < cutoff <= 1).
        cutoff: f64,
        /// Filter order (currently only first-order IIR is applied).
        order: usize,
    },
    /// Rolling simple-moving-average smoothing.
    Smooth {
        /// Window length.
        window: usize,
    },
    /// Rolling z-score.
    ZScore {
        /// Window length.
        window: usize,
    },
    /// Clip values to [min, max].
    Clip {
        /// Minimum value.
        min: f64,
        /// Maximum value.
        max: f64,
    },
    /// Lag the signal by N periods.
    Lag(usize),
    /// First difference.
    Diff,
    /// Natural logarithm.
    Log,
    /// Exponential.
    Exp,
    /// Percentile rank within rolling window.
    Rank {
        /// Window length.
        window: usize,
    },
}

/// A composable signal processing pipeline.
pub struct SignalPipeline {
    /// Processing stages to apply in order.
    pub stages: Vec<ProcessingStage>,
    /// Per-symbol rolling value buffers.
    pub buffers: HashMap<String, VecDeque<f64>>,
}

impl SignalPipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self {
            stages: Vec::new(),
            buffers: HashMap::new(),
        }
    }

    /// Append a processing stage.
    pub fn add_stage(&mut self, stage: ProcessingStage) {
        self.stages.push(stage);
    }

    /// Process one signal value through all pipeline stages.
    /// Returns `None` if the pipeline cannot produce an output yet.
    pub fn process(&mut self, signal: SignalValue) -> Option<f64> {
        let key = format!("{}:{}", signal.symbol, signal.source);
        let buf = self.buffers.entry(key).or_insert_with(VecDeque::new);

        // Determine required buffer capacity
        let required = self.stages.iter().map(|s| match s {
            ProcessingStage::Smooth { window } => *window,
            ProcessingStage::ZScore { window } => *window,
            ProcessingStage::Rank { window } => *window,
            ProcessingStage::Lag(n) => *n + 1,
            ProcessingStage::Diff => 2,
            _ => 1,
        }).max().unwrap_or(1);

        buf.push_back(signal.value);
        while buf.len() > required.max(1) * 2 + 10 {
            buf.pop_front();
        }

        let values: Vec<f64> = buf.iter().copied().collect();
        let mut current = signal.value;

        for stage in &self.stages {
            let result = Self::apply_stage(&values, stage)?;
            current = result;
        }
        Some(current)
    }

    /// Apply a single processing stage to a buffer slice, returning the latest output value.
    pub fn apply_stage(values: &[f64], stage: &ProcessingStage) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        let last = *values.last()?;
        match stage {
            ProcessingStage::Normalize { method } => match method {
                NormMethod::MinMax => Some(Self::normalize_minmax(values)),
                NormMethod::ZScore => Some(Self::rolling_zscore(values)),
                NormMethod::Robust => {
                    let mut sorted = values.to_vec();
                    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let n = sorted.len();
                    let median = sorted[n / 2];
                    let q1 = sorted[n / 4];
                    let q3 = sorted[3 * n / 4];
                    let iqr = q3 - q1;
                    if iqr.abs() < 1e-12 {
                        Some(0.0)
                    } else {
                        Some((last - median) / iqr)
                    }
                }
            },
            ProcessingStage::Filter { cutoff, .. } => {
                Some(Self::exponential_filter(values, *cutoff))
            }
            ProcessingStage::Smooth { window } => {
                let w = (*window).min(values.len());
                let slice = &values[values.len() - w..];
                Some(slice.iter().sum::<f64>() / w as f64)
            }
            ProcessingStage::ZScore { window } => {
                let w = (*window).min(values.len());
                let slice = &values[values.len() - w..];
                Some(Self::rolling_zscore(slice))
            }
            ProcessingStage::Clip { min, max } => Some(last.clamp(*min, *max)),
            ProcessingStage::Lag(n) => {
                if values.len() > *n {
                    Some(values[values.len() - 1 - n])
                } else {
                    None
                }
            }
            ProcessingStage::Diff => {
                if values.len() >= 2 {
                    Some(values[values.len() - 1] - values[values.len() - 2])
                } else {
                    None
                }
            }
            ProcessingStage::Log => {
                if last > 0.0 { Some(last.ln()) } else { None }
            }
            ProcessingStage::Exp => Some(last.exp()),
            ProcessingStage::Rank { window } => {
                let w = (*window).min(values.len());
                let slice = &values[values.len() - w..];
                Some(Self::rank(slice))
            }
        }
    }

    /// Min-max normalize: scales last value to [0, 1] using window min and max.
    pub fn normalize_minmax(values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1e-12 {
            return 0.0;
        }
        let last = *values.last().unwrap_or(&0.0);
        (last - min) / (max - min)
    }

    /// Rolling z-score of the last value in the slice.
    pub fn rolling_zscore(values: &[f64]) -> f64 {
        let n = values.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let mean = values.iter().sum::<f64>() / n;
        let std = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        if std < 1e-12 {
            return 0.0;
        }
        let last = *values.last().unwrap_or(&0.0);
        (last - mean) / std
    }

    /// Percentile rank of the last value in the slice (0.0 to 1.0).
    pub fn rank(values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let last = *values.last().unwrap_or(&0.0);
        let below = values.iter().filter(|&&v| v < last).count();
        below as f64 / values.len() as f64
    }

    /// First-order IIR (exponential) low-pass filter. Returns filtered last value.
    /// alpha = 2 * pi * cutoff / (2 * pi * cutoff + 1)  (discrete approximation).
    pub fn exponential_filter(values: &[f64], cutoff: f64) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let alpha = (2.0 * std::f64::consts::PI * cutoff)
            / (2.0 * std::f64::consts::PI * cutoff + 1.0);
        let alpha = alpha.clamp(0.0, 1.0);
        let mut filtered = values[0];
        for &v in &values[1..] {
            filtered = alpha * v + (1.0 - alpha) * filtered;
        }
        filtered
    }
}

impl Default for SignalPipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Utility for combining and scoring signals.
pub struct SignalCombiner;

impl SignalCombiner {
    /// Weighted average of (signal, weight) pairs.
    pub fn combine(signals: &[(f64, f64)]) -> f64 {
        let total_weight: f64 = signals.iter().map(|(_, w)| w).sum();
        if total_weight.abs() < 1e-12 {
            return 0.0;
        }
        signals.iter().map(|(s, w)| s * w).sum::<f64>() / total_weight
    }

    /// Cross-sectional z-score: demean and standardize across symbols.
    pub fn cross_sectional_zscore(signals: &[(String, f64)]) -> Vec<(String, f64)> {
        if signals.is_empty() {
            return vec![];
        }
        let n = signals.len() as f64;
        let mean = signals.iter().map(|(_, v)| v).sum::<f64>() / n;
        let std = (signals.iter().map(|(_, v)| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        signals.iter().map(|(sym, v)| {
            let z = if std < 1e-12 { 0.0 } else { (v - mean) / std };
            (sym.clone(), z)
        }).collect()
    }

    /// Pearson information coefficient between predicted signals and realized returns.
    pub fn information_coefficient(signals: &[f64], forward_returns: &[f64]) -> f64 {
        let n = signals.len().min(forward_returns.len()) as f64;
        if n < 2.0 {
            return 0.0;
        }
        let mean_s = signals.iter().take(n as usize).sum::<f64>() / n;
        let mean_r = forward_returns.iter().take(n as usize).sum::<f64>() / n;
        let cov: f64 = signals.iter().zip(forward_returns.iter()).take(n as usize)
            .map(|(s, r)| (s - mean_s) * (r - mean_r))
            .sum();
        let std_s = (signals.iter().take(n as usize).map(|s| (s - mean_s).powi(2)).sum::<f64>() / n).sqrt();
        let std_r = (forward_returns.iter().take(n as usize).map(|r| (r - mean_r).powi(2)).sum::<f64>() / n).sqrt();
        if std_s < 1e-12 || std_r < 1e-12 {
            return 0.0;
        }
        cov / (n * std_s * std_r)
    }

    /// Combine signals with exponential decay weights (most recent = index 0 has highest weight if
    /// signals are ordered oldest-first, weight_i = exp(-lambda * (n - 1 - i))).
    pub fn decay_weighted_combine(signals: &[f64], half_life: f64) -> f64 {
        if signals.is_empty() {
            return 0.0;
        }
        let lambda = std::f64::consts::LN_2 / half_life.max(1e-12);
        let n = signals.len();
        let mut total_weight = 0.0;
        let mut weighted_sum = 0.0;
        for (i, &s) in signals.iter().enumerate() {
            let age = (n - 1 - i) as f64;
            let w = (-lambda * age).exp();
            weighted_sum += s * w;
            total_weight += w;
        }
        if total_weight < 1e-12 {
            return 0.0;
        }
        weighted_sum / total_weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_minmax() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let out = SignalPipeline::normalize_minmax(&v);
        assert!((out - 1.0).abs() < 1e-9, "last value should normalize to 1.0, got {out}");
    }

    #[test]
    fn test_rolling_zscore() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let z = SignalPipeline::rolling_zscore(&v);
        // last value = 5.0, should have positive z-score
        assert!(z > 0.0, "z={z}");
    }

    #[test]
    fn test_combine_weighted() {
        let signals = vec![(1.0, 0.5), (3.0, 0.5)];
        let out = SignalCombiner::combine(&signals);
        assert!((out - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_information_coefficient_perfect() {
        let s = vec![1.0, 2.0, 3.0, 4.0];
        let r = vec![0.1, 0.2, 0.3, 0.4];
        let ic = SignalCombiner::information_coefficient(&s, &r);
        assert!((ic - 1.0).abs() < 1e-6, "ic={ic}");
    }

    #[test]
    fn test_decay_weighted_combine() {
        // All equal signals should return the same value regardless of decay
        let s = vec![5.0; 10];
        let out = SignalCombiner::decay_weighted_combine(&s, 3.0);
        assert!((out - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_cross_sectional_zscore() {
        let signals = vec![
            ("A".to_string(), 10.0),
            ("B".to_string(), 20.0),
            ("C".to_string(), 30.0),
        ];
        let zscores = SignalCombiner::cross_sectional_zscore(&signals);
        assert_eq!(zscores.len(), 3);
        // B should be at z=0 (mean)
        let b = zscores.iter().find(|(s, _)| s == "B").unwrap().1;
        assert!(b.abs() < 1e-9, "b_zscore={b}");
    }

    #[test]
    fn test_pipeline_process() {
        let mut pipeline = SignalPipeline::new();
        pipeline.add_stage(ProcessingStage::Smooth { window: 3 });
        for i in 0..5 {
            let sig = SignalValue {
                value: i as f64,
                timestamp_ns: i as u64 * 1_000_000,
                symbol: "TEST".to_string(),
                source: "mock".to_string(),
            };
            let _ = pipeline.process(sig);
        }
    }
}
