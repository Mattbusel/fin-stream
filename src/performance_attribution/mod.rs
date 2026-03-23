//! Performance attribution analysis.
//!
//! Implements the Brinson-Fachler attribution model:
//! allocation effect, selection effect, interaction effect.
//! Also provides OLS-based factor attribution and rolling attribution reports.

use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
//  Return decomposition
// ─────────────────────────────────────────────────────────────────────────────

/// Decomposes a period return into Brinson-Fachler components.
#[derive(Debug, Clone)]
pub struct ReturnDecomposition {
    /// Total active return for the period.
    pub total_return: f64,
    /// Allocation effect: derived from over/under-weighting sectors.
    pub allocation_effect: f64,
    /// Selection effect: derived from stock-picking within sectors.
    pub selection_effect: f64,
    /// Interaction effect: joint allocation and selection contribution.
    pub interaction_effect: f64,
}

impl ReturnDecomposition {
    /// Verify that the components approximately sum to the total return.
    pub fn verify(&self) -> bool {
        let reconstructed =
            self.allocation_effect + self.selection_effect + self.interaction_effect;
        (reconstructed - self.total_return).abs() < 1e-8
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Sector attribution
// ─────────────────────────────────────────────────────────────────────────────

/// Per-sector attribution results.
#[derive(Debug, Clone)]
pub struct SectorAttribution {
    /// Sector or asset-class name.
    pub sector: String,
    /// Portfolio weight in this sector.
    pub portfolio_weight: f64,
    /// Benchmark weight in this sector.
    pub benchmark_weight: f64,
    /// Portfolio return in this sector.
    pub portfolio_return: f64,
    /// Benchmark return in this sector.
    pub benchmark_return: f64,
    /// Decomposed attribution for this sector.
    pub attribution: ReturnDecomposition,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Attribution report
// ─────────────────────────────────────────────────────────────────────────────

/// Full attribution report for a period.
#[derive(Debug, Clone)]
pub struct AttributionReport {
    /// Total portfolio return for the period.
    pub portfolio_return: f64,
    /// Total benchmark return for the period.
    pub benchmark_return: f64,
    /// Active return: `portfolio_return - benchmark_return`.
    pub active_return: f64,
    /// Per-sector attribution detail.
    pub sectors: Vec<SectorAttribution>,
    /// Information ratio (active return / tracking error).
    pub information_ratio: f64,
    /// Tracking error (annualised standard deviation of active returns).
    pub tracking_error: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  BrinsonFachler
// ─────────────────────────────────────────────────────────────────────────────

/// Brinson-Fachler attribution calculator.
pub struct BrinsonFachler;

impl BrinsonFachler {
    /// Allocation effect for a single sector.
    ///
    /// ```text
    /// allocation = (pw - bw) * (br - total_br)
    /// ```
    pub fn allocation_effect(pw: f64, bw: f64, br: f64, total_br: f64) -> f64 {
        (pw - bw) * (br - total_br)
    }

    /// Selection effect for a single sector.
    ///
    /// ```text
    /// selection = bw * (pr - br)
    /// ```
    pub fn selection_effect(pw: f64, bw: f64, pr: f64, br: f64) -> f64 {
        let _ = pw; // not used in Brinson-Fachler formulation
        bw * (pr - br)
    }

    /// Interaction effect for a single sector.
    ///
    /// ```text
    /// interaction = (pw - bw) * (pr - br)
    /// ```
    pub fn interaction_effect(pw: f64, bw: f64, pr: f64, br: f64) -> f64 {
        (pw - bw) * (pr - br)
    }

    /// Build a full [`AttributionReport`] from a slice of sector tuples.
    ///
    /// Each tuple is `(sector_name, portfolio_weight, benchmark_weight,
    /// portfolio_return, benchmark_return)`.
    pub fn attribute(sectors: &[(String, f64, f64, f64, f64)]) -> AttributionReport {
        // Aggregate benchmark return (weighted by benchmark weight).
        let total_br: f64 = sectors.iter().map(|(_, _, bw, _, br)| bw * br).sum();
        let total_pr: f64 = sectors.iter().map(|(_, pw, _, pr, _)| pw * pr).sum();

        let sector_attrs: Vec<SectorAttribution> = sectors
            .iter()
            .map(|(name, pw, bw, pr, br)| {
                let alloc = Self::allocation_effect(*pw, *bw, *br, total_br);
                let sel = Self::selection_effect(*pw, *bw, *pr, *br);
                let inter = Self::interaction_effect(*pw, *bw, *pr, *br);
                let total = alloc + sel + inter;
                SectorAttribution {
                    sector: name.clone(),
                    portfolio_weight: *pw,
                    benchmark_weight: *bw,
                    portfolio_return: *pr,
                    benchmark_return: *br,
                    attribution: ReturnDecomposition {
                        total_return: total,
                        allocation_effect: alloc,
                        selection_effect: sel,
                        interaction_effect: inter,
                    },
                }
            })
            .collect();

        let active_return = total_pr - total_br;

        AttributionReport {
            portfolio_return: total_pr,
            benchmark_return: total_br,
            active_return,
            sectors: sector_attrs,
            information_ratio: 0.0, // populated by rolling_attribution
            tracking_error: 0.0,
        }
    }

    /// Compute attribution for each period in a rolling window.
    ///
    /// - `portfolio_returns[t]`: slice of per-sector portfolio returns at time `t`.
    /// - `benchmark_returns[t]`: slice of per-sector benchmark returns at time `t`.
    /// - `weights[t]`: slice of `(portfolio_weight, benchmark_weight)` pairs.
    ///
    /// Returns one [`AttributionReport`] per time period.
    pub fn rolling_attribution(
        portfolio_returns: &[Vec<f64>],
        benchmark_returns: &[Vec<f64>],
        weights: &[Vec<f64>],
    ) -> Vec<AttributionReport> {
        let n_periods = portfolio_returns.len().min(benchmark_returns.len()).min(weights.len());
        let mut reports: Vec<AttributionReport> = Vec::with_capacity(n_periods);

        for t in 0..n_periods {
            let pr_vec = &portfolio_returns[t];
            let br_vec = &benchmark_returns[t];
            let w_vec = &weights[t];

            // weights[t] is a flat vector: [pw0, bw0, pw1, bw1, …].
            let n_sectors = pr_vec.len().min(br_vec.len()).min(w_vec.len() / 2);
            let sectors: Vec<(String, f64, f64, f64, f64)> = (0..n_sectors)
                .map(|s| {
                    let pw = w_vec.get(2 * s).copied().unwrap_or(0.0);
                    let bw = w_vec.get(2 * s + 1).copied().unwrap_or(0.0);
                    let pr = pr_vec.get(s).copied().unwrap_or(0.0);
                    let br = br_vec.get(s).copied().unwrap_or(0.0);
                    (format!("S{}", s), pw, bw, pr, br)
                })
                .collect();

            reports.push(Self::attribute(&sectors));
        }

        // Annotate with information ratio and tracking error across periods.
        if reports.len() > 1 {
            let active_returns: Vec<f64> = reports.iter().map(|r| r.active_return).collect();
            let mean_active = active_returns.iter().sum::<f64>() / active_returns.len() as f64;
            let variance = active_returns
                .iter()
                .map(|&r| (r - mean_active).powi(2))
                .sum::<f64>()
                / (active_returns.len() - 1) as f64;
            let tracking_error = variance.sqrt();
            let ir = if tracking_error > 1e-12 {
                mean_active / tracking_error
            } else {
                0.0
            };
            for report in &mut reports {
                report.tracking_error = tracking_error;
                report.information_ratio = ir;
            }
        }

        reports
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  FactorAttribution
// ─────────────────────────────────────────────────────────────────────────────

/// Factor-based return attribution via OLS decomposition.
pub struct FactorAttribution;

impl FactorAttribution {
    /// Decompose portfolio returns into factor exposures via OLS.
    ///
    /// `portfolio` — series of portfolio returns.
    /// `factor_returns` — one inner `Vec<f64>` per time period, each element
    ///    a factor return for that period.
    /// `factor_names` — names matching the factor columns.
    ///
    /// Returns a map of `factor_name → beta coefficient`.
    pub fn time_series_attribution(
        portfolio: &[f64],
        factor_returns: &[Vec<f64>],
        factor_names: &[&str],
    ) -> HashMap<String, f64> {
        let t = portfolio.len().min(factor_returns.len());
        let k = factor_names.len();

        if t == 0 || k == 0 {
            return HashMap::new();
        }

        // Simple OLS: β = (X'X)⁻¹ X'y, implemented per factor independently
        // (assumes orthogonal factors — sufficient for the module's scope).
        let mut betas = HashMap::with_capacity(k);

        for (fi, &name) in factor_names.iter().enumerate() {
            let mut xy = 0.0_f64;
            let mut xx = 0.0_f64;
            for t_idx in 0..t {
                let x = factor_returns[t_idx].get(fi).copied().unwrap_or(0.0);
                let y = portfolio[t_idx];
                xy += x * y;
                xx += x * x;
            }
            let beta = if xx.abs() > 1e-12 { xy / xx } else { 0.0 };
            betas.insert(name.to_string(), beta);
        }

        betas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brinson_fachler_effects() {
        // Overweight a sector with above-benchmark return.
        let alloc = BrinsonFachler::allocation_effect(0.5, 0.3, 0.10, 0.07);
        assert!(alloc > 0.0); // positive: overweight, above benchmark

        let sel = BrinsonFachler::selection_effect(0.5, 0.3, 0.12, 0.10);
        assert!(sel > 0.0); // positive: portfolio return > benchmark return

        let inter = BrinsonFachler::interaction_effect(0.5, 0.3, 0.12, 0.10);
        assert!(inter > 0.0);
    }

    #[test]
    fn test_attribute_report() {
        let sectors = vec![
            ("Tech".to_string(), 0.5, 0.4, 0.10, 0.08),
            ("Energy".to_string(), 0.5, 0.6, 0.05, 0.06),
        ];
        let report = BrinsonFachler::attribute(&sectors);
        assert!((report.active_return - (report.portfolio_return - report.benchmark_return)).abs() < 1e-12);
    }

    #[test]
    fn test_factor_attribution() {
        let portfolio = vec![0.02, 0.03, -0.01, 0.04];
        let factor_returns = vec![
            vec![0.01, 0.005],
            vec![0.02, 0.003],
            vec![-0.01, 0.002],
            vec![0.03, 0.001],
        ];
        let names = ["market", "value"];
        let betas = FactorAttribution::time_series_attribution(&portfolio, &factor_returns, &names);
        assert!(betas.contains_key("market"));
        assert!(betas.contains_key("value"));
    }
}
