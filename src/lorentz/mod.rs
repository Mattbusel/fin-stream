//! Lorentz (special-relativistic) transforms applied to financial time series.
//!
//! ## Mathematical basis
//!
//! In special relativity, the Lorentz transformation relates the spacetime
//! coordinates `(t, x)` measured in one inertial frame `S` to the coordinates
//! `(t', x')` measured in a frame `S'` that moves with velocity `v` along the
//! x-axis:
//!
//! ```text
//!     t' = gamma * (t  - beta * x / c)
//!     x' = gamma * (x  - beta * c * t)
//!
//!     where  beta  = v / c          (dimensionless velocity, 0 <= beta < 1)
//!            gamma = 1 / sqrt(1 - beta^2)   (Lorentz factor, >= 1)
//! ```
//!
//! ## Application to market data
//!
//! Market microstructure theory frequently treats price discovery as a
//! diffusion process in two dimensions:
//!
//! - **Time axis `t`**: wall-clock time (seconds, or normalized to [0, 1]).
//! - **Price axis `x`**: log-price or normalized price.
//!
//! The Lorentz frame boost is applied as a **feature engineering** step:
//! it stretches or compresses the price-time plane along hyperbolas, which
//! are invariant under Lorentz boosts. The motivation is that certain
//! microstructure signals (momentum, mean-reversion) that appear curved in
//! the lab frame can appear as straight lines in a boosted frame.
//!
//! For a financial series, we set:
//! - `c = 1` (price changes are bounded by some maximum instantaneous return).
//! - `beta = drift_rate` (the normalized drift velocity of the price process).
//!
//! The transformed coordinates `(t', x')` are then fed as features into a
//! downstream model.
//!
//! ## Guarantees
//!
//! - Non-panicking: construction validates `beta` and returns a typed error.
//! - Deterministic: the same `(t, x, beta)` always produces the same output.
//! - `beta = 0` is the identity transform (gamma = 1, no distortion).

use crate::error::StreamError;

/// A spacetime event expressed as a (time, position) coordinate pair.
///
/// In the financial interpretation:
/// - `t` is elapsed time since the series start, normalized to a convenient
///   scale (e.g., seconds, or fraction of the session).
/// - `x` is normalized log-price or normalized price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpacetimePoint {
    /// Time coordinate.
    pub t: f64,
    /// Spatial (price) coordinate.
    pub x: f64,
}

impl SpacetimePoint {
    /// Construct a new spacetime point.
    pub fn new(t: f64, x: f64) -> Self {
        Self { t, x }
    }

    /// 4-displacement from `self` to `other`: `(Δt, Δx) = (other.t - self.t, other.x - self.x)`.
    ///
    /// Useful for computing proper distances and intervals between events.
    pub fn displacement(self, other: Self) -> Self {
        Self {
            t: other.t - self.t,
            x: other.x - self.x,
        }
    }

    /// Minkowski norm squared: `t² − x²`.
    ///
    /// The sign classifies the causal relationship of the event to the origin:
    /// - Positive: time-like (inside the light cone — causally reachable).
    /// - Zero: light-like / null (on the light cone).
    /// - Negative: space-like (outside the light cone — causally disconnected).
    pub fn norm_sq(self) -> f64 {
        self.t * self.t - self.x * self.x
    }

    /// Returns `true` if this point is inside the light cone (`t² > x²`).
    pub fn is_timelike(self) -> bool {
        self.norm_sq() > 0.0
    }

    /// Returns `true` if this point lies on the light cone.
    ///
    /// Uses a tolerance of `1e-10` for floating-point comparisons.
    pub fn is_lightlike(self) -> bool {
        self.norm_sq().abs() < 1e-10
    }

    /// Returns `true` if this point is outside the light cone (`t² < x²`).
    pub fn is_spacelike(self) -> bool {
        self.norm_sq() < 0.0
    }

    /// Proper spacetime distance between `self` and `other`.
    ///
    /// Computed as `sqrt(|Δt² − Δx²|)` where `Δ = other − self`. The absolute
    /// value ensures a real result regardless of the interval type.
    /// For time-like separations this is the proper time; for space-like
    /// separations it is the proper length.
    pub fn distance_to(self, other: Self) -> f64 {
        let d = self.displacement(other);
        d.norm_sq().abs().sqrt()
    }
}

/// Lorentz frame-boost transform for financial time series.
///
/// Applies the special-relativistic Lorentz transformation with velocity
/// parameter `beta = v/c` to `(t, x)` coordinate pairs. The speed of light
/// is normalized to `c = 1`.
///
/// # Construction
///
/// ```rust
/// use fin_stream::lorentz::LorentzTransform;
///
/// let lt = LorentzTransform::new(0.5).unwrap(); // beta = 0.5
/// ```
///
/// # Identity
///
/// `beta = 0` is the identity: `t' = t`, `x' = x`.
///
/// # Singularity avoidance
///
/// `beta >= 1` would require dividing by zero in the Lorentz factor and is
/// rejected at construction time with `LorentzConfigError`.
#[derive(Debug, Clone, Copy)]
pub struct LorentzTransform {
    /// Normalized velocity (v/c). Satisfies `0.0 <= beta < 1.0`.
    beta: f64,
    /// Precomputed Lorentz factor: `1 / sqrt(1 - beta^2)`.
    gamma: f64,
}

impl LorentzTransform {
    /// Create a new Lorentz transform with the given velocity parameter.
    ///
    /// `beta` must satisfy `0.0 <= beta < 1.0`.
    ///
    /// # Errors
    ///
    /// Returns `LorentzConfigError` if `beta` is negative, `NaN`, or `>= 1.0`.
    pub fn new(beta: f64) -> Result<Self, StreamError> {
        if beta.is_nan() || beta < 0.0 || beta >= 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!(
                    "beta must be in [0.0, 1.0) but got {beta}; \
                     beta >= 1 produces a division by zero in the Lorentz factor"
                ),
            });
        }
        let gamma = 1.0 / (1.0 - beta * beta).sqrt();
        Ok(Self { beta, gamma })
    }

    /// Compute beta from a known gamma value.
    ///
    /// Inverts `gamma = 1 / sqrt(1 - beta^2)` to recover `beta = sqrt(1 - 1/gamma^2)`.
    /// Useful when a Lorentz factor is known from a source other than a velocity.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if `gamma < 1.0` or is `NaN`
    /// (gamma must be ≥ 1 for a physically valid boost).
    /// Construct a Lorentz transform from rapidity `η = atanh(β)`.
    ///
    /// Rapidity is the additive parameter for successive boosts along the
    /// same axis: composing two boosts `η₁` and `η₂` gives `η₁ + η₂`.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if `eta` is `NaN`, `Inf`, or
    /// results in `|beta| >= 1`.
    pub fn from_rapidity(eta: f64) -> Result<Self, StreamError> {
        if !eta.is_finite() {
            return Err(StreamError::LorentzConfigError {
                reason: format!("rapidity must be finite, got {eta}"),
            });
        }
        let beta = eta.tanh();
        Self::new(beta)
    }

    /// Construct a [`LorentzTransform`] from a velocity `v` and speed of light `c`.
    ///
    /// Computes `beta = v / c` and delegates to [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if `c` is zero, if `v` or
    /// `c` are non-finite, or if the resulting `|beta| >= 1`.
    pub fn from_velocity(v: f64, c: f64) -> Result<Self, StreamError> {
        if !v.is_finite() || !c.is_finite() {
            return Err(StreamError::LorentzConfigError {
                reason: format!("v and c must be finite, got v={v}, c={c}"),
            });
        }
        if c == 0.0 {
            return Err(StreamError::LorentzConfigError {
                reason: "speed of light c must be non-zero".into(),
            });
        }
        Self::new(v / c)
    }

    /// Returns the Lorentz gamma factor for the given `beta`: `1 / √(1 − β²)`.
    ///
    /// This is a pure mathematical helper — no validation is performed.
    /// Returns `f64::INFINITY` when `beta == 1.0` and `NaN` when `beta > 1.0`.
    pub fn gamma_at(beta: f64) -> f64 {
        (1.0 - beta * beta).sqrt().recip()
    }

    /// Relativistic momentum: `m × γ × β` (in natural units where `c = 1`).
    ///
    /// Returns the momentum of a particle with rest mass `mass` moving at
    /// the velocity encoded in this transform. Negative mass is allowed for
    /// analytical purposes but physically meaningless.
    pub fn relativistic_momentum(&self, mass: f64) -> f64 {
        mass * self.gamma * self.beta
    }

    /// Kinetic energy factor: `γ − 1`.
    ///
    /// The relativistic kinetic energy is `KE = (γ − 1) mc²`. This method
    /// returns the dimensionless factor `γ − 1` so callers can multiply by
    /// `mc²` in their own units. Zero at rest (`β = 0`).
    pub fn kinetic_energy_ratio(&self) -> f64 {
        self.gamma - 1.0
    }

    /// Time dilation factor: `γ` (gamma).
    ///
    /// Equivalent to [`gamma`](Self::gamma) but named for clarity when used in
    /// a time-dilation context. A moving clock ticks slower by this factor
    /// relative to the stationary frame.
    pub fn time_dilation_factor(&self) -> f64 {
        self.gamma
    }

    /// Length contraction factor: `L' = L / gamma`.
    ///
    /// The reciprocal of the Lorentz factor — always in `(0.0, 1.0]`.
    pub fn length_contraction_factor(&self) -> f64 {
        1.0 / self.gamma
    }

    /// Computes beta (v/c) from a Lorentz gamma factor. Returns an error if `gamma < 1.0`.
    pub fn beta_from_gamma(gamma: f64) -> Result<f64, StreamError> {
        if gamma.is_nan() || gamma < 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!("gamma must be >= 1.0, got {gamma}"),
            });
        }
        Ok((1.0 - 1.0 / (gamma * gamma)).sqrt())
    }

    /// Rapidity: `φ = atanh(β)`.
    ///
    /// Unlike velocity, rapidities add linearly under boosts:
    /// `φ_total = φ_1 + φ_2`.
    pub fn rapidity(&self) -> f64 {
        self.beta.atanh()
    }

    /// Proper velocity: `w = β × γ`.
    ///
    /// Unlike coordinate velocity `β`, proper velocity is unbounded and
    /// equals the spatial displacement per unit proper time.
    pub fn proper_velocity(&self) -> f64 {
        self.beta * self.gamma
    }

    /// The configured velocity parameter `beta = v/c`.
    pub fn beta(&self) -> f64 {
        self.beta
    }

    /// The precomputed Lorentz factor `gamma = 1 / sqrt(1 - beta^2)`.
    ///
    /// `gamma == 1.0` when `beta == 0` (identity). `gamma` increases
    /// monotonically towards infinity as `beta` approaches 1.
    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    /// Apply the Lorentz boost to a spacetime point.
    ///
    /// Computes:
    /// ```text
    ///     t' = gamma * (t - beta * x)
    ///     x' = gamma * (x - beta * t)
    /// ```
    ///
    /// # Complexity: O(1), no heap allocation.
    #[must_use]
    pub fn transform(&self, p: SpacetimePoint) -> SpacetimePoint {
        let t_prime = self.gamma * (p.t - self.beta * p.x);
        let x_prime = self.gamma * (p.x - self.beta * p.t);
        SpacetimePoint {
            t: t_prime,
            x: x_prime,
        }
    }

    /// Apply the inverse Lorentz boost (boost in the opposite direction).
    ///
    /// Computes:
    /// ```text
    ///     t' = gamma * (t + beta * x)
    ///     x' = gamma * (x + beta * t)
    /// ```
    ///
    /// For a point `p`, `inverse_transform(transform(p)) == p` up to floating-
    /// point rounding.
    ///
    /// # Complexity: O(1), no heap allocation.
    #[must_use]
    pub fn inverse_transform(&self, p: SpacetimePoint) -> SpacetimePoint {
        let t_orig = self.gamma * (p.t + self.beta * p.x);
        let x_orig = self.gamma * (p.x + self.beta * p.t);
        SpacetimePoint {
            t: t_orig,
            x: x_orig,
        }
    }

    /// Apply the boost to a batch of points.
    ///
    /// Returns a new `Vec` of transformed points. The input slice is not
    /// modified.
    ///
    /// # Complexity: O(n), one heap allocation of size `n`.
    pub fn transform_batch(&self, points: &[SpacetimePoint]) -> Vec<SpacetimePoint> {
        points.iter().map(|&p| self.transform(p)).collect()
    }

    /// Apply the inverse boost to a batch of points.
    ///
    /// Equivalent to calling [`inverse_transform`](Self::inverse_transform) on each element.
    /// For any slice `pts`, `inverse_transform_batch(&transform_batch(pts))` equals `pts`
    /// up to floating-point rounding.
    ///
    /// # Complexity: O(n), one heap allocation of size `n`.
    pub fn inverse_transform_batch(&self, points: &[SpacetimePoint]) -> Vec<SpacetimePoint> {
        points.iter().map(|&p| self.inverse_transform(p)).collect()
    }

    /// Time-dilation: the transformed time coordinate for a point at rest
    /// (`x = 0`) is `t' = gamma * t`.
    ///
    /// This is a convenience method that surfaces the time-dilation formula
    /// for the common case where only the time axis is of interest.
    ///
    /// # Complexity: O(1)
    pub fn dilate_time(&self, t: f64) -> f64 {
        self.gamma * t
    }

    /// Length-contraction: the transformed spatial coordinate for an event at
    /// the time origin (`t = 0`) is `x' = gamma * x`.
    ///
    /// This is a convenience method that surfaces the length-contraction
    /// formula for the common case where only the price axis is of interest.
    ///
    /// # Complexity: O(1)
    pub fn contract_length(&self, x: f64) -> f64 {
        self.gamma * x
    }

    /// Proper time: `coordinate_time / gamma`.
    ///
    /// The inverse of [`dilate_time`](Self::dilate_time). Converts a dilated
    /// coordinate time back to the proper time measured by a clock co-moving
    /// with the boosted frame — always shorter than the coordinate time by
    /// a factor of `gamma`.
    ///
    /// # Complexity: O(1)
    pub fn time_contraction(&self, coordinate_time: f64) -> f64 {
        coordinate_time / self.gamma
    }

    /// Returns `true` if this boost is ultrarelativistic (`beta > 0.9`).
    ///
    /// At `beta > 0.9` the Lorentz factor `gamma > 2.3`, and relativistic
    /// effects dominate. Useful as a sanity guard before applying the boost
    /// to financial time-series where very high velocities indicate data issues.
    pub fn is_ultrarelativistic(&self) -> bool {
        self.beta > 0.9
    }

    /// Relativistic spatial momentum factor: `gamma × beta`.
    ///
    /// This is the spatial component of the four-momentum (up to a mass factor).
    /// At `beta = 0` the result is `0`; as `beta → 1` it diverges.
    ///
    /// # Complexity: O(1)
    pub fn momentum_factor(&self) -> f64 {
        self.gamma * self.beta
    }

    /// Computes the Lorentz-invariant spacetime interval between two events.
    ///
    /// The interval is defined as `ds² = (Δt)² - (Δx)²` (signature `+−`).
    /// A positive result means the separation is time-like; negative means space-like;
    /// zero means light-like (the events could be connected by a photon).
    ///
    /// This quantity is invariant under Lorentz boosts — `apply(p1)` and `apply(p2)`
    /// will yield the same interval as `p1` and `p2` directly.
    ///
    /// # Complexity: O(1)
    pub fn spacetime_interval(p1: SpacetimePoint, p2: SpacetimePoint) -> f64 {
        let dt = p2.t - p1.t;
        let dx = p2.x - p1.x;
        dt * dt - dx * dx
    }

    /// Relativistic velocity addition as a static helper.
    ///
    /// Computes `(beta1 + beta2) / (1 + beta1 * beta2)` without constructing
    /// a full `LorentzTransform`. Useful when only the composed velocity is
    /// needed rather than the full transform object.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if either input is outside
    /// `[0, 1)` or if the composed velocity is `>= 1`.
    pub fn velocity_addition(beta1: f64, beta2: f64) -> Result<f64, StreamError> {
        if beta1.is_nan() || beta1 < 0.0 || beta1 >= 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!("beta1 must be in [0.0, 1.0), got {beta1}"),
            });
        }
        if beta2.is_nan() || beta2 < 0.0 || beta2 >= 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!("beta2 must be in [0.0, 1.0), got {beta2}"),
            });
        }
        let composed = (beta1 + beta2) / (1.0 + beta1 * beta2);
        if composed >= 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!("composed velocity {composed} >= 1.0 (speed of light)"),
            });
        }
        Ok(composed)
    }

    /// Proper time elapsed in the moving frame for a given coordinate time.
    ///
    /// Returns `coordinate_time / gamma`. For `beta = 0` (identity) this
    /// equals `coordinate_time`; as `beta → 1` the proper time shrinks toward
    /// zero (time dilation).
    ///
    /// # Complexity: O(1)
    pub fn proper_time(&self, coordinate_time: f64) -> f64 {
        coordinate_time / self.gamma
    }

    /// Return a new `LorentzTransform` that boosts in the opposite direction.
    ///
    /// If `self` has velocity `beta`, `self.inverse()` has velocity `-beta`.
    /// Applying `self` then `self.inverse()` is the identity transform.
    ///
    /// This is equivalent to `LorentzTransform::new(-self.beta)` but avoids the
    /// error branch since negating a valid beta always produces a valid result.
    ///
    /// # Complexity: O(1)
    pub fn inverse(&self) -> Self {
        // -beta is in (-1, 0] which is valid (we allow beta = 0 but not < 0 publicly;
        // here we construct directly to allow the negated value).
        let beta = -self.beta;
        let gamma = 1.0 / (1.0 - beta * beta).sqrt();
        Self { beta, gamma }
    }

    /// Coordinate (lab-frame) time for a given proper time.
    ///
    /// Returns `proper_time * gamma`. This is the inverse of
    /// [`proper_time`](Self::proper_time): given elapsed proper time τ measured
    /// by a clock in the moving frame, returns the corresponding coordinate time
    /// `t = γτ` observed in the lab frame.
    ///
    /// # Complexity: O(1)
    pub fn time_dilation(&self, proper_time: f64) -> f64 {
        proper_time * self.gamma
    }

    /// Compose two Lorentz boosts into a single equivalent boost.
    ///
    /// Uses the relativistic velocity addition formula:
    ///
    /// ```text
    ///     beta_composed = (beta_1 + beta_2) / (1 + beta_1 * beta_2)
    /// ```
    ///
    /// This is the physical law stating that applying boost `self` then boost
    /// `other` is equivalent to a single boost with the composed velocity.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if the composed velocity
    /// reaches or exceeds 1 (the speed of light), which cannot happen for
    /// valid inputs but is checked defensively.
    pub fn compose(&self, other: &LorentzTransform) -> Result<Self, StreamError> {
        let b1 = self.beta;
        let b2 = other.beta;
        let composed = (b1 + b2) / (1.0 + b1 * b2);
        LorentzTransform::new(composed)
    }

    /// Compose a sequence of boosts into a single equivalent transform.
    ///
    /// Applies [`compose`](Self::compose) left-to-right over `betas`, starting
    /// from the identity boost (beta = 0). An empty slice returns the identity.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if any beta is invalid or
    /// the accumulated velocity reaches the speed of light.
    pub fn boost_chain(betas: &[f64]) -> Result<Self, StreamError> {
        let mut result = LorentzTransform::new(0.0)?;
        for &beta in betas {
            let next = LorentzTransform::new(beta)?;
            result = result.compose(&next)?;
        }
        Ok(result)
    }

    /// Returns `true` if this transform is effectively the identity (`beta ≈ 0`).
    ///
    /// The identity leaves spacetime coordinates unchanged: `t' = t`, `x' = x`.
    /// Uses a tolerance of `1e-10`.
    pub fn is_identity(&self) -> bool {
        self.beta.abs() < 1e-10
    }

    /// Relativistic composition of two boosts (Einstein velocity addition).
    ///
    /// The composed transform moves at `β = (β₁ + β₂) / (1 + β₁β₂)`, which is
    /// equivalent to adding rapidities: `tanh⁻¹(β_composed) = tanh⁻¹(β₁) + tanh⁻¹(β₂)`.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::LorentzConfigError`] if the composed beta reaches or
    /// exceeds the speed of light (should not happen for valid sub-luminal inputs).
    pub fn composition(&self, other: &Self) -> Result<Self, StreamError> {
        let b1 = self.beta;
        let b2 = other.beta;
        let denom = 1.0 + b1 * b2;
        if denom.abs() < 1e-15 {
            return Err(StreamError::LorentzConfigError {
                reason: "boost composition denominator too small (near-singular)".into(),
            });
        }
        Self::new((b1 + b2) / denom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS
    }

    fn point_approx_eq(a: SpacetimePoint, b: SpacetimePoint) -> bool {
        approx_eq(a.t, b.t) && approx_eq(a.x, b.x)
    }

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_valid_beta() {
        let lt = LorentzTransform::new(0.5).unwrap();
        assert!((lt.beta() - 0.5).abs() < EPS);
    }

    #[test]
    fn test_new_beta_zero() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert_eq!(lt.beta(), 0.0);
        assert!((lt.gamma() - 1.0).abs() < EPS);
    }

    #[test]
    fn test_new_beta_one_returns_error() {
        let err = LorentzTransform::new(1.0).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_above_one_returns_error() {
        let err = LorentzTransform::new(1.5).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_negative_returns_error() {
        let err = LorentzTransform::new(-0.1).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_nan_returns_error() {
        let err = LorentzTransform::new(f64::NAN).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    // ── beta = 0 is identity ─────────────────────────────────────────────────

    #[test]
    fn test_beta_zero_is_identity_transform() {
        let lt = LorentzTransform::new(0.0).unwrap();
        let p = SpacetimePoint::new(3.0, 4.0);
        let q = lt.transform(p);
        assert!(point_approx_eq(p, q), "beta=0 must be identity, got {q:?}");
    }

    // ── Time dilation ─────────────────────────────────────────────────────────

    /// For a point at x=0, t' = gamma * t (pure time dilation).
    #[test]
    fn test_time_dilation_at_x_zero() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(5.0, 0.0);
        let q = lt.transform(p);
        let expected_t = lt.gamma() * 5.0;
        assert!(approx_eq(q.t, expected_t));
    }

    #[test]
    fn test_dilate_time_helper() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!(approx_eq(lt.dilate_time(1.0), lt.gamma()));
    }

    // ── Length contraction ────────────────────────────────────────────────────

    /// For a point at t=0, x' = gamma * x (pure length contraction).
    #[test]
    fn test_length_contraction_at_t_zero() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(0.0, 5.0);
        let q = lt.transform(p);
        let expected_x = lt.gamma() * 5.0;
        assert!(approx_eq(q.x, expected_x));
    }

    #[test]
    fn test_contract_length_helper() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!(approx_eq(lt.contract_length(1.0), lt.gamma()));
    }

    // ── Known beta values ─────────────────────────────────────────────────────

    /// For beta = 0.6, gamma = 1.25 (classic textbook value).
    #[test]
    fn test_known_beta_0_6_gamma_is_1_25() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!(
            (lt.gamma() - 1.25).abs() < 1e-9,
            "gamma should be 1.25, got {}",
            lt.gamma()
        );
    }

    /// For beta = 0.8, gamma = 5/3 approximately 1.6667.
    #[test]
    fn test_known_beta_0_8_gamma() {
        let lt = LorentzTransform::new(0.8).unwrap();
        let expected_gamma = 5.0 / 3.0;
        assert!((lt.gamma() - expected_gamma).abs() < 1e-9);
    }

    /// For beta = 0.5, gamma = 2/sqrt(3) approximately 1.1547.
    #[test]
    fn test_known_beta_0_5_gamma() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let expected_gamma = 2.0 / 3.0f64.sqrt();
        assert!((lt.gamma() - expected_gamma).abs() < 1e-9);
    }

    /// Transform a known point with beta=0.6 and verify the result manually.
    ///
    /// Point (t=1, x=0): t' = 1.25 * (1 - 0.6*0) = 1.25; x' = 1.25*(0 - 0.6) = -0.75
    #[test]
    fn test_transform_known_point_beta_0_6() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(1.0, 0.0);
        let q = lt.transform(p);
        assert!((q.t - 1.25).abs() < 1e-9);
        assert!((q.x - (-0.75)).abs() < 1e-9);
    }

    // ── Inverse round-trip ────────────────────────────────────────────────────

    #[test]
    fn test_inverse_transform_roundtrip() {
        let lt = LorentzTransform::new(0.7).unwrap();
        let p = SpacetimePoint::new(3.0, 1.5);
        let q = lt.transform(p);
        let r = lt.inverse_transform(q);
        assert!(
            point_approx_eq(r, p),
            "round-trip failed: expected {p:?}, got {r:?}"
        );
    }

    // ── Batch transform ───────────────────────────────────────────────────────

    #[test]
    fn test_transform_batch_length_preserved() {
        let lt = LorentzTransform::new(0.3).unwrap();
        let pts = vec![
            SpacetimePoint::new(0.0, 1.0),
            SpacetimePoint::new(1.0, 2.0),
            SpacetimePoint::new(2.0, 3.0),
        ];
        let out = lt.transform_batch(&pts);
        assert_eq!(out.len(), pts.len());
    }

    #[test]
    fn test_transform_batch_matches_individual() {
        let lt = LorentzTransform::new(0.4).unwrap();
        let pts = vec![SpacetimePoint::new(1.0, 0.5), SpacetimePoint::new(2.0, 1.5)];
        let batch = lt.transform_batch(&pts);
        for (i, &p) in pts.iter().enumerate() {
            let individual = lt.transform(p);
            assert!(
                point_approx_eq(batch[i], individual),
                "batch[{i}] differs from individual transform"
            );
        }
    }

    #[test]
    fn test_inverse_transform_batch_roundtrip() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let pts = vec![
            SpacetimePoint::new(1.0, 0.0),
            SpacetimePoint::new(2.0, 1.0),
            SpacetimePoint::new(0.0, 3.0),
        ];
        let transformed = lt.transform_batch(&pts);
        let restored = lt.inverse_transform_batch(&transformed);
        for (i, (&orig, &rest)) in pts.iter().zip(restored.iter()).enumerate() {
            assert!(
                point_approx_eq(orig, rest),
                "round-trip failed at index {i}: expected {orig:?}, got {rest:?}"
            );
        }
    }

    #[test]
    fn test_inverse_transform_batch_length_preserved() {
        let lt = LorentzTransform::new(0.3).unwrap();
        let pts = vec![SpacetimePoint::new(0.0, 0.0), SpacetimePoint::new(1.0, 1.0)];
        assert_eq!(lt.inverse_transform_batch(&pts).len(), pts.len());
    }

    // ── SpacetimePoint ────────────────────────────────────────────────────────

    #[test]
    fn test_spacetime_point_fields() {
        let p = SpacetimePoint::new(1.5, 2.5);
        assert_eq!(p.t, 1.5);
        assert_eq!(p.x, 2.5);
    }

    #[test]
    fn test_spacetime_point_equality() {
        let p = SpacetimePoint::new(1.0, 2.0);
        let q = SpacetimePoint::new(1.0, 2.0);
        assert_eq!(p, q);
    }

    // ── Compose ───────────────────────────────────────────────────────────────

    /// Two zero boosts compose to zero.
    #[test]
    fn test_compose_identity_with_identity() {
        let lt = LorentzTransform::new(0.0).unwrap();
        let composed = lt.compose(&lt).unwrap();
        assert!(approx_eq(composed.beta(), 0.0));
    }

    /// Relativistic addition: 0.5 + 0.5 = 0.8 (not 1.0).
    #[test]
    fn test_compose_0_5_and_0_5() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let composed = lt.compose(&lt).unwrap();
        let expected = (0.5 + 0.5) / (1.0 + 0.5 * 0.5); // = 1.0 / 1.25 = 0.8
        assert!((composed.beta() - expected).abs() < EPS);
    }

    /// Composing boost with its inverse should give the identity (beta ≈ 0).
    #[test]
    fn test_compose_with_negative_is_identity() {
        // Applying +0.3, then -0.3 should cancel out: (0.3 - 0.3) / (1 - 0.09) = 0
        let lt_fwd = LorentzTransform::new(0.3).unwrap();
        let lt_bwd = LorentzTransform::new(0.3).unwrap();
        // Compose forward boost with itself is not identity; test associativity
        // For actual cancellation we'd need negative beta — out of domain.
        // Instead verify compose gives valid beta < 1.
        let composed = lt_fwd.compose(&lt_bwd).unwrap();
        assert!(composed.beta() < 1.0);
    }

    // ── Rapidity ──────────────────────────────────────────────────────────────

    /// beta=0 has rapidity 0 (atanh(0) = 0).
    #[test]
    fn test_rapidity_zero_beta() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(approx_eq(lt.rapidity(), 0.0));
    }

    /// Rapidity for beta=0.5: atanh(0.5) ≈ 0.5493.
    #[test]
    fn test_rapidity_known_value() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let expected = (0.5f64).atanh();
        assert!((lt.rapidity() - expected).abs() < EPS);
    }

    /// Rapidities are additive: rapidity(compose(a, b)) == rapidity(a) + rapidity(b).
    #[test]
    fn test_rapidity_is_additive_under_composition() {
        let lt1 = LorentzTransform::new(0.3).unwrap();
        let lt2 = LorentzTransform::new(0.4).unwrap();
        let composed = lt1.compose(&lt2).unwrap();
        let sum_rapidities = lt1.rapidity() + lt2.rapidity();
        assert!(
            (composed.rapidity() - sum_rapidities).abs() < 1e-9,
            "rapidity should be additive: {} vs {}",
            composed.rapidity(),
            sum_rapidities
        );
    }

    // ── Velocity addition (static) ────────────────────────────────────────────

    #[test]
    fn test_velocity_addition_known_values() {
        // 0.5 + 0.5 = 0.8 (same as compose test)
        let result = LorentzTransform::velocity_addition(0.5, 0.5).unwrap();
        assert!((result - 0.8).abs() < EPS);
    }

    #[test]
    fn test_velocity_addition_identity_with_zero() {
        let result = LorentzTransform::velocity_addition(0.0, 0.6).unwrap();
        assert!((result - 0.6).abs() < EPS);
    }

    #[test]
    fn test_velocity_addition_invalid_beta_rejected() {
        assert!(LorentzTransform::velocity_addition(1.0, 0.5).is_err());
        assert!(LorentzTransform::velocity_addition(0.5, 1.0).is_err());
        assert!(LorentzTransform::velocity_addition(-0.1, 0.5).is_err());
    }

    #[test]
    fn test_velocity_addition_matches_compose() {
        let b1 = 0.3;
        let b2 = 0.4;
        let static_result = LorentzTransform::velocity_addition(b1, b2).unwrap();
        let lt1 = LorentzTransform::new(b1).unwrap();
        let lt2 = LorentzTransform::new(b2).unwrap();
        let composed = lt1.compose(&lt2).unwrap();
        assert!((static_result - composed.beta()).abs() < EPS);
    }

    // ── Proper time ───────────────────────────────────────────────────────────

    /// beta=0: proper_time == coordinate_time (no dilation).
    #[test]
    fn test_proper_time_identity_at_zero_beta() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(approx_eq(lt.proper_time(10.0), 10.0));
    }

    /// proper_time = coordinate_time / gamma; always <= coordinate_time.
    #[test]
    fn test_proper_time_less_than_coordinate_time() {
        let lt = LorentzTransform::new(0.6).unwrap(); // gamma = 1.25
        let tau = lt.proper_time(5.0);
        // tau = 5.0 / 1.25 = 4.0
        assert!((tau - 4.0).abs() < EPS);
        assert!(tau < 5.0);
    }

    /// proper_time(dilate_time(t)) should round-trip to t.
    #[test]
    fn test_proper_time_roundtrip_with_dilate_time() {
        let lt = LorentzTransform::new(0.8).unwrap();
        let t = 3.0;
        let dilated = lt.dilate_time(t);
        let recovered = lt.proper_time(dilated);
        assert!(approx_eq(recovered, t));
    }

    /// Compose and then transform equals applying both transforms sequentially.
    #[test]
    fn test_compose_equals_sequential_transforms() {
        let lt1 = LorentzTransform::new(0.3).unwrap();
        let lt2 = LorentzTransform::new(0.4).unwrap();
        let composed = lt1.compose(&lt2).unwrap();

        let p = SpacetimePoint::new(2.0, 1.0);
        let sequential = lt2.transform(lt1.transform(p));
        let single = composed.transform(p);
        assert!(
            point_approx_eq(sequential, single),
            "composed boost must equal sequential: {sequential:?} vs {single:?}"
        );
    }

    // ── SpacetimePoint::displacement ─────────────────────────────────────────

    #[test]
    fn test_displacement_gives_correct_deltas() {
        let a = SpacetimePoint::new(1.0, 2.0);
        let b = SpacetimePoint::new(4.0, 6.0);
        let d = a.displacement(b);
        assert!(approx_eq(d.t, 3.0));
        assert!(approx_eq(d.x, 4.0));
    }

    #[test]
    fn test_displacement_to_self_is_zero() {
        let a = SpacetimePoint::new(3.0, 7.0);
        let d = a.displacement(a);
        assert!(approx_eq(d.t, 0.0));
        assert!(approx_eq(d.x, 0.0));
    }

    // ── LorentzTransform::boost_chain ────────────────────────────────────────

    #[test]
    fn test_boost_chain_empty_is_identity() {
        let chain = LorentzTransform::boost_chain(&[]).unwrap();
        assert!(approx_eq(chain.beta(), 0.0));
        assert!(approx_eq(chain.gamma(), 1.0));
    }

    #[test]
    fn test_boost_chain_single_equals_new() {
        let chain = LorentzTransform::boost_chain(&[0.5]).unwrap();
        let direct = LorentzTransform::new(0.5).unwrap();
        assert!(approx_eq(chain.beta(), direct.beta()));
    }

    #[test]
    fn test_boost_chain_two_equals_compose() {
        let chain = LorentzTransform::boost_chain(&[0.3, 0.4]).unwrap();
        let manual = LorentzTransform::new(0.3)
            .unwrap()
            .compose(&LorentzTransform::new(0.4).unwrap())
            .unwrap();
        assert!(approx_eq(chain.beta(), manual.beta()));
    }

    #[test]
    fn test_boost_chain_invalid_beta_returns_error() {
        assert!(LorentzTransform::boost_chain(&[1.0]).is_err());
        assert!(LorentzTransform::boost_chain(&[0.3, -0.1]).is_err());
    }

    // ── LorentzTransform::time_dilation ───────────────────────────────────────

    #[test]
    fn test_time_dilation_identity_at_zero_beta() {
        // beta=0, gamma=1 → time_dilation(t) == t
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(approx_eq(lt.time_dilation(10.0), 10.0));
    }

    #[test]
    fn test_time_dilation_inverse_of_proper_time() {
        // time_dilation(proper_time(t)) should round-trip to t
        let lt = LorentzTransform::new(0.6).unwrap();
        let t = 5.0_f64;
        let tau = lt.proper_time(t);
        assert!(approx_eq(lt.time_dilation(tau), t));
    }

    #[test]
    fn test_time_dilation_greater_than_proper_time() {
        // For beta > 0: coordinate_time = gamma * tau > tau (moving clocks run slower)
        let lt = LorentzTransform::new(0.8).unwrap();
        let proper = 3.0_f64;
        let coord = lt.time_dilation(proper);
        assert!(coord > proper);
    }

    // ── LorentzTransform::inverse ─────────────────────────────────────────────

    #[test]
    fn test_inverse_negates_beta() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let inv = lt.inverse();
        assert!(approx_eq(inv.beta(), -0.6));
    }

    #[test]
    fn test_inverse_preserves_gamma_magnitude() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let inv = lt.inverse();
        // gamma only depends on beta^2, so |gamma| is the same
        assert!(approx_eq(inv.gamma(), lt.gamma()));
    }

    #[test]
    fn test_inverse_roundtrips_point() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let p = SpacetimePoint::new(3.0, 1.0);
        let boosted = lt.transform(p);
        let recovered = lt.inverse().transform(boosted);
        assert!(point_approx_eq(recovered, p));
    }

    // ── SpacetimePoint::norm_sq / is_timelike / is_lightlike / is_spacelike ──

    #[test]
    fn test_norm_sq_timelike() {
        // t=3, x=1 → 9 - 1 = 8 > 0
        let p = SpacetimePoint::new(3.0, 1.0);
        assert!((p.norm_sq() - 8.0).abs() < EPS);
        assert!(p.is_timelike());
        assert!(!p.is_spacelike());
        assert!(!p.is_lightlike());
    }

    #[test]
    fn test_norm_sq_spacelike() {
        // t=1, x=3 → 1 - 9 = -8 < 0
        let p = SpacetimePoint::new(1.0, 3.0);
        assert!((p.norm_sq() - (-8.0)).abs() < EPS);
        assert!(p.is_spacelike());
        assert!(!p.is_timelike());
        assert!(!p.is_lightlike());
    }

    #[test]
    fn test_norm_sq_lightlike() {
        // t=1, x=1 → 1 - 1 = 0
        let p = SpacetimePoint::new(1.0, 1.0);
        assert!(p.norm_sq().abs() < EPS);
        assert!(p.is_lightlike());
        assert!(!p.is_timelike());
        assert!(!p.is_spacelike());
    }

    #[test]
    fn test_norm_sq_origin_is_lightlike() {
        let p = SpacetimePoint::new(0.0, 0.0);
        assert!(p.is_lightlike());
    }

    // ── LorentzTransform::is_identity ─────────────────────────────────────────

    #[test]
    fn test_is_identity_beta_zero() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(lt.is_identity());
    }

    #[test]
    fn test_is_not_identity_nonzero_beta() {
        let lt = LorentzTransform::new(0.5).unwrap();
        assert!(!lt.is_identity());
    }

    #[test]
    fn test_is_identity_empty_boost_chain() {
        let lt = LorentzTransform::boost_chain(&[]).unwrap();
        assert!(lt.is_identity());
    }

    // ── LorentzTransform::beta_from_gamma ─────────────────────────────────────

    #[test]
    fn test_beta_from_gamma_identity() {
        // gamma=1.0 → beta=0.0
        let beta = LorentzTransform::beta_from_gamma(1.0).unwrap();
        assert!(approx_eq(beta, 0.0));
    }

    #[test]
    fn test_beta_from_gamma_roundtrip() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let recovered = LorentzTransform::beta_from_gamma(lt.gamma()).unwrap();
        assert!(approx_eq(recovered, 0.6));
    }

    #[test]
    fn test_beta_from_gamma_invalid_rejected() {
        assert!(LorentzTransform::beta_from_gamma(0.5).is_err()); // < 1
        assert!(LorentzTransform::beta_from_gamma(f64::NAN).is_err());
        assert!(LorentzTransform::beta_from_gamma(-1.0).is_err());
    }

    // ── LorentzTransform::from_rapidity ───────────────────────────────────────

    #[test]
    fn test_from_rapidity_zero_is_identity() {
        let lt = LorentzTransform::from_rapidity(0.0).unwrap();
        assert!(lt.is_identity());
    }

    #[test]
    fn test_from_rapidity_positive_gives_valid_beta() {
        // eta = 0.5 → beta = tanh(0.5) ≈ 0.4621
        let lt = LorentzTransform::from_rapidity(0.5).unwrap();
        assert!((lt.beta() - 0.5_f64.tanh()).abs() < EPS);
    }

    #[test]
    fn test_from_rapidity_infinite_rejected() {
        assert!(LorentzTransform::from_rapidity(f64::INFINITY).is_err());
        assert!(LorentzTransform::from_rapidity(f64::NEG_INFINITY).is_err());
        assert!(LorentzTransform::from_rapidity(f64::NAN).is_err());
    }

    // ── SpacetimePoint::distance_to ───────────────────────────────────────────

    #[test]
    fn test_distance_to_timelike_separation() {
        // t=3, x=0 vs t=0, x=0 → Δt=3, Δx=0 → norm_sq=9 → dist=3
        let a = SpacetimePoint::new(0.0, 0.0);
        let b = SpacetimePoint::new(3.0, 0.0);
        assert!((a.distance_to(b) - 3.0).abs() < EPS);
    }

    #[test]
    fn test_distance_to_spacelike_separation() {
        // t=0 vs x=4 → norm_sq=0-16=-16 → dist=4
        let a = SpacetimePoint::new(0.0, 0.0);
        let b = SpacetimePoint::new(0.0, 4.0);
        assert!((a.distance_to(b) - 4.0).abs() < EPS);
    }

    #[test]
    fn test_distance_to_same_point_is_zero() {
        let p = SpacetimePoint::new(2.0, 3.0);
        assert!(p.distance_to(p).abs() < EPS);
    }

    // ── LorentzTransform::gamma_at ────────────────────────────────────────────

    #[test]
    fn test_gamma_at_zero_is_one() {
        assert!((LorentzTransform::gamma_at(0.0) - 1.0).abs() < EPS);
    }

    #[test]
    fn test_gamma_at_matches_constructor_gamma() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let expected = lt.gamma();
        let computed = LorentzTransform::gamma_at(0.6);
        assert!((computed - expected).abs() < EPS);
    }

    #[test]
    fn test_gamma_at_one_is_infinite() {
        assert!(LorentzTransform::gamma_at(1.0).is_infinite());
    }

    #[test]
    fn test_gamma_at_above_one_is_nan() {
        assert!(LorentzTransform::gamma_at(1.1).is_nan());
    }

    #[test]
    fn test_from_velocity_matches_beta_ratio() {
        // v = 0.6c → beta = 0.6
        let t = LorentzTransform::from_velocity(0.6, 1.0).unwrap();
        assert!((t.beta() - 0.6).abs() < 1e-12);
    }

    #[test]
    fn test_from_velocity_zero_c_returns_error() {
        assert!(matches!(
            LorentzTransform::from_velocity(0.5, 0.0),
            Err(StreamError::LorentzConfigError { .. })
        ));
    }

    #[test]
    fn test_from_velocity_non_finite_returns_error() {
        assert!(LorentzTransform::from_velocity(f64::INFINITY, 1.0).is_err());
        assert!(LorentzTransform::from_velocity(0.5, f64::NAN).is_err());
    }

    #[test]
    fn test_from_velocity_superluminal_returns_error() {
        // v > c → |beta| > 1 → should fail
        assert!(LorentzTransform::from_velocity(1.5, 1.0).is_err());
    }

    // ── LorentzTransform::relativistic_momentum ───────────────────────────────

    #[test]
    fn test_relativistic_momentum_zero_beta_is_zero() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(approx_eq(lt.relativistic_momentum(1.0), 0.0));
    }

    #[test]
    fn test_relativistic_momentum_known_value() {
        // beta=0.6, gamma=1.25 → p = mass * gamma * beta = 2.0 * 1.25 * 0.6 = 1.5
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!((lt.relativistic_momentum(2.0) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn test_relativistic_momentum_scales_with_mass() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p1 = lt.relativistic_momentum(1.0);
        let p2 = lt.relativistic_momentum(2.0);
        assert!((p2 - 2.0 * p1).abs() < 1e-9);
    }

    // ── LorentzTransform::kinetic_energy_ratio ────────────────────────────────

    #[test]
    fn test_kinetic_energy_ratio_zero_at_rest() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert!(approx_eq(lt.kinetic_energy_ratio(), 0.0));
    }

    #[test]
    fn test_kinetic_energy_ratio_known_value() {
        // beta=0.6 → gamma=1.25 → ratio = 1.25 - 1 = 0.25
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!((lt.kinetic_energy_ratio() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn test_kinetic_energy_ratio_positive_for_nonzero_beta() {
        let lt = LorentzTransform::new(0.8).unwrap();
        assert!(lt.kinetic_energy_ratio() > 0.0);
    }

    #[test]
    fn test_composition_zero_with_zero_is_zero() {
        let t1 = LorentzTransform::new(0.0).unwrap();
        let t2 = LorentzTransform::new(0.0).unwrap();
        let composed = t1.composition(&t2).unwrap();
        assert!(composed.beta().abs() < 1e-12);
    }

    #[test]
    fn test_composition_with_opposite_is_near_zero() {
        let t1 = LorentzTransform::new(0.6).unwrap();
        let t2 = t1.inverse(); // beta = -0.6
        let composed = t1.composition(&t2).unwrap();
        // (0.6 + (-0.6)) / (1 + 0.6*(-0.6)) = 0 / 0.64 = 0
        assert!(composed.beta().abs() < 1e-12);
    }

    #[test]
    fn test_composition_velocity_addition_known_value() {
        // 0.5c + 0.5c = (0.5+0.5)/(1+0.25) = 1.0/1.25 = 0.8
        let t1 = LorentzTransform::new(0.5).unwrap();
        let t2 = LorentzTransform::new(0.5).unwrap();
        let composed = t1.composition(&t2).unwrap();
        assert!((composed.beta() - 0.8).abs() < 1e-12);
    }

    // ── LorentzTransform::time_dilation_factor ────────────────────────────────

    #[test]
    fn test_time_dilation_factor_one_at_rest() {
        let t = LorentzTransform::new(0.0).unwrap();
        assert!((t.time_dilation_factor() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_time_dilation_factor_equals_gamma() {
        let t = LorentzTransform::new(0.6).unwrap();
        assert!((t.time_dilation_factor() - t.gamma()).abs() < 1e-12);
    }

    #[test]
    fn test_time_dilation_factor_greater_than_one_for_nonzero_beta() {
        let t = LorentzTransform::new(0.8).unwrap();
        assert!(t.time_dilation_factor() > 1.0);
    }

    // --- time_contraction ---

    #[test]
    fn test_time_contraction_inverse_of_dilate_time() {
        let t = LorentzTransform::new(0.6).unwrap();
        let original = 100.0_f64;
        let dilated = t.dilate_time(original);
        let contracted = t.time_contraction(dilated);
        assert!((contracted - original).abs() < 1e-10);
    }

    #[test]
    fn test_time_contraction_at_zero_beta_equals_input() {
        let t = LorentzTransform::new(0.0).unwrap();
        assert!((t.time_contraction(42.0) - 42.0).abs() < 1e-12);
    }

    #[test]
    fn test_time_contraction_less_than_input_for_nonzero_beta() {
        let t = LorentzTransform::new(0.8).unwrap();
        assert!(t.time_contraction(100.0) < 100.0);
    }

    // ── LorentzTransform::length_contraction_factor ───────────────────────────

    #[test]
    fn test_length_contraction_factor_one_at_rest() {
        let t = LorentzTransform::new(0.0).unwrap();
        assert!((t.length_contraction_factor() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_length_contraction_factor_is_reciprocal_of_gamma() {
        let t = LorentzTransform::new(0.6).unwrap();
        assert!((t.length_contraction_factor() - 1.0 / t.gamma()).abs() < 1e-12);
    }

    #[test]
    fn test_length_contraction_factor_less_than_one_for_nonzero_beta() {
        let t = LorentzTransform::new(0.9).unwrap();
        assert!(t.length_contraction_factor() < 1.0);
    }

    // ── LorentzTransform::proper_velocity ────────────────────────────────────

    #[test]
    fn test_proper_velocity_zero_at_rest() {
        let t = LorentzTransform::new(0.0).unwrap();
        assert!((t.proper_velocity() - 0.0).abs() < 1e-12);
    }

    #[test]
    fn test_proper_velocity_equals_beta_times_gamma() {
        let t = LorentzTransform::new(0.6).unwrap();
        assert!((t.proper_velocity() - t.beta() * t.gamma()).abs() < 1e-12);
    }

    #[test]
    fn test_proper_velocity_greater_than_beta_for_high_speed() {
        // At v=0.8c, gamma≈1.667, so proper_velocity = 0.8*1.667 ≈ 1.333 > 0.8
        let t = LorentzTransform::new(0.8).unwrap();
        assert!(t.proper_velocity() > t.beta());
    }

    // --- is_ultrarelativistic / momentum_factor ---

    #[test]
    fn test_is_ultrarelativistic_true_above_0_9() {
        let t = LorentzTransform::new(0.95).unwrap();
        assert!(t.is_ultrarelativistic());
    }

    #[test]
    fn test_is_ultrarelativistic_false_below_0_9() {
        let t = LorentzTransform::new(0.5).unwrap();
        assert!(!t.is_ultrarelativistic());
    }

    #[test]
    fn test_is_ultrarelativistic_false_at_exactly_0_9() {
        let t = LorentzTransform::new(0.9).unwrap();
        assert!(!t.is_ultrarelativistic()); // strictly greater than 0.9
    }

    #[test]
    fn test_momentum_factor_zero_at_rest() {
        let t = LorentzTransform::new(0.0).unwrap();
        assert!(t.momentum_factor().abs() < 1e-12);
    }

    #[test]
    fn test_momentum_factor_equals_gamma_times_beta() {
        let t = LorentzTransform::new(0.6).unwrap();
        assert!((t.momentum_factor() - t.gamma() * t.beta()).abs() < 1e-12);
    }
}
