# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

---

## [2.8.8] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 137)**
- `NormalizedTick::price_downside_ratio(ticks)` — fraction of ticks with price below the mean.
- `NormalizedTick::avg_trade_lag(ticks)` — mean inter-tick interval in milliseconds.
- `NormalizedTick::qty_max_run(ticks)` — longest consecutive run of increasing quantities.
- `NormalizedTick::tick_sell_fraction(ticks)` — fraction of ticks with Sell side.

**`ohlcv` module — `OhlcvBar` analytics (round 137)**
- `OhlcvBar::bar_body_mean(bars)` — mean absolute body size (|close - open|) across bars.
- `OhlcvBar::close_high_correlation(bars)` — Pearson correlation between close and high prices.
- `OhlcvBar::bar_close_above_midpoint(bars)` — fraction of bars where close > (high + low) / 2.
- `OhlcvBar::bar_open_gap_score(bars)` — mean gap between consecutive bars / prior bar range.

**`norm` module — `MinMaxNormalizer` analytics (round 137)**
- `MinMaxNormalizer::window_linear_trend_score()` — OLS slope normalized by window mean.
- `MinMaxNormalizer::window_zscore_min()` — minimum z-score value in the window.
- `MinMaxNormalizer::window_zscore_max()` — maximum z-score value in the window.
- `MinMaxNormalizer::window_diff_variance()` — variance of consecutive differences in the window.

**`norm` module — `ZScoreNormalizer` analytics (round 137)**
- `ZScoreNormalizer::window_linear_trend_score()` — OLS slope normalized by window mean.
- `ZScoreNormalizer::window_zscore_min()` — minimum z-score value in the window.
- `ZScoreNormalizer::window_zscore_max()` — maximum z-score value in the window.
- `ZScoreNormalizer::window_diff_variance()` — variance of consecutive differences in the window.

---

## [2.8.7] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 136)**
- `NormalizedTick::tick_vol_ratio(ticks)` — std of price changes / mean absolute price change.
- `NormalizedTick::qty_std_ratio(ticks)` — std / mean of quantities (relative dispersion).
- `NormalizedTick::side_qty_concentration(ticks)` — dominant side quantity / total sided quantity.
- `NormalizedTick::price_reversion_speed(ticks)` — fraction of steps moving toward the mean.

**`ohlcv` module — `OhlcvBar` analytics (round 136)**
- `OhlcvBar::bar_open_efficiency(bars)` — mean of |close - open| / (high - low) per bar.
- `OhlcvBar::close_oscillation_amplitude(bars)` — std of close prices across bars.
- `OhlcvBar::bar_high_low_score(bars)` — mean of (high / low - 1) per bar.
- `OhlcvBar::bar_range_change(bars)` — mean of consecutive bar range differences.

**`norm` module — `MinMaxNormalizer` analytics (round 136)**
- `MinMaxNormalizer::window_step_dn_fraction()` — fraction of steps that are strictly downward.
- `MinMaxNormalizer::window_mean_abs_dev_ratio()` — mean absolute deviation / window range.
- `MinMaxNormalizer::window_recent_high()` — maximum value in the second half of the window.
- `MinMaxNormalizer::window_recent_low()` — minimum value in the second half of the window.

**`norm` module — `ZScoreNormalizer` analytics (round 136)**
- `ZScoreNormalizer::window_step_dn_fraction()` — fraction of steps that are strictly downward.
- `ZScoreNormalizer::window_mean_abs_dev_ratio()` — mean absolute deviation / window range.
- `ZScoreNormalizer::window_recent_high()` — maximum value in the second half of the window.
- `ZScoreNormalizer::window_recent_low()` — minimum value in the second half of the window.

---

## [2.8.6] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 135)**
- `NormalizedTick::tick_cluster_density(ticks)` — ticks per second based on timestamps.
- `NormalizedTick::qty_zscore_last(ticks)` — z-score of the last quantity in the series.
- `NormalizedTick::side_price_ratio(ticks)` — absolute relative spread between mean buy and sell prices.
- `NormalizedTick::qty_entropy_norm(ticks)` — Shannon entropy of quantity distribution normalized to [0, 1].

**`ohlcv` module — `OhlcvBar` analytics (round 135)**
- `OhlcvBar::bar_body_count(bars)` — count of bars with non-zero body.
- `OhlcvBar::range_contraction_ratio(bars)` — fraction of bars where range < prior bar range.
- `OhlcvBar::volume_trend_ratio(bars)` — last volume / mean volume.
- `OhlcvBar::bar_midpoint_score(bars)` — mean of (midpoint - open) / range per bar.

**`norm` module — `MinMaxNormalizer` analytics (round 135)**
- `MinMaxNormalizer::window_mean_crossing_rate()` — fraction of steps crossing the mean.
- `MinMaxNormalizer::window_var_to_mean()` — variance / |mean| (index of dispersion).
- `MinMaxNormalizer::window_coeff_var()` — std / |mean| (coefficient of variation).
- `MinMaxNormalizer::window_step_up_fraction()` — fraction of steps that are strictly upward.

**`norm` module — `ZScoreNormalizer` analytics (round 135)**
- `ZScoreNormalizer::window_mean_crossing_rate()` — fraction of steps crossing the mean.
- `ZScoreNormalizer::window_var_to_mean()` — variance / |mean| (index of dispersion).
- `ZScoreNormalizer::window_coeff_var()` — std / |mean| (coefficient of variation).
- `ZScoreNormalizer::window_step_up_fraction()` — fraction of steps that are strictly upward.

---

## [2.8.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 134)**
- `NormalizedTick::price_zscore_mean(ticks)` — mean absolute z-score across all prices.
- `NormalizedTick::tick_size_ratio(ticks)` — last quantity / mean quantity.
- `NormalizedTick::buy_tick_fraction(ticks)` — fraction of ticks with Buy side.
- `NormalizedTick::price_jump_count(ticks)` — count of price changes exceeding one std deviation.

**`ohlcv` module — `OhlcvBar` analytics (round 134)**
- `OhlcvBar::bar_high_trend(bars)` — mean of consecutive high differences.
- `OhlcvBar::bar_low_trend(bars)` — mean of consecutive low differences.
- `OhlcvBar::close_high_wick(bars)` — mean of (high - close) / (high - low) per bar.
- `OhlcvBar::bar_open_persistence(bars)` — fraction of bars where open exceeds prior open.

**`norm` module — `MinMaxNormalizer` analytics (round 134)**
- `MinMaxNormalizer::window_second_half_mean()` — mean of the second half of the window.
- `MinMaxNormalizer::window_local_min_count()` — count of local minima in the window.
- `MinMaxNormalizer::window_curvature()` — mean of second-order differences in the window.
- `MinMaxNormalizer::window_half_diff()` — second half mean minus first half mean.

**`norm` module — `ZScoreNormalizer` analytics (round 134)**
- `ZScoreNormalizer::window_second_half_mean()` — mean of the second half of the window.
- `ZScoreNormalizer::window_local_min_count()` — count of local minima in the window.
- `ZScoreNormalizer::window_curvature()` — mean of second-order differences in the window.
- `ZScoreNormalizer::window_half_diff()` — second half mean minus first half mean.

---

## [2.8.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 133)**
- `NormalizedTick::price_roc(ticks)` — (last price - first price) / first price.
- `NormalizedTick::qty_roc(ticks)` — (last qty - first qty) / first qty.
- `NormalizedTick::tick_timing_score(ticks)` — fraction of ticks in the first temporal half.
- `NormalizedTick::side_spread_ratio(ticks)` — std of buy prices / std of sell prices.

**`ohlcv` module — `OhlcvBar` analytics (round 133)**
- `OhlcvBar::bar_open_close_momentum(bars)` — sum of bullish/bearish signs across bars.
- `OhlcvBar::close_body_position(bars)` — mean of (close - low) / (high - low) per bar.
- `OhlcvBar::bar_close_persistence(bars)` — fraction where current close > prior open.
- `OhlcvBar::close_wick_ratio(bars)` — mean of upper wick / body per bar.

**`norm` module — `MinMaxNormalizer` analytics (round 133)**
- `MinMaxNormalizer::window_abs_diff_sum()` — sum of absolute consecutive differences.
- `MinMaxNormalizer::window_max_gap()` — maximum absolute gap between consecutive values.
- `MinMaxNormalizer::window_local_max_count()` — count of local maxima in the window.
- `MinMaxNormalizer::window_first_half_mean()` — mean of the first half of the window.

**`norm` module — `ZScoreNormalizer` analytics (round 133)**
- `ZScoreNormalizer::window_abs_diff_sum()` — sum of absolute consecutive differences.
- `ZScoreNormalizer::window_max_gap()` — maximum absolute gap between consecutive values.
- `ZScoreNormalizer::window_local_max_count()` — count of local maxima in the window.
- `ZScoreNormalizer::window_first_half_mean()` — mean of the first half of the window.

---

## [2.8.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 132)**
- `NormalizedTick::price_zscore_abs(ticks)` — absolute value of z-score of the last price.
- `NormalizedTick::tick_reversal_count(ticks)` — number of direction changes in consecutive price moves.
- `NormalizedTick::tick_price_range_ratio(ticks)` — price range / mean price.
- `NormalizedTick::price_range_skew(ticks)` — (mean - min) / (max - min) price distribution skew.

**`ohlcv` module — `OhlcvBar` analytics (round 132)**
- `OhlcvBar::bar_volume_trend(bars)` — mean of consecutive volume differences.
- `OhlcvBar::close_low_spread(bars)` — mean of (close - low) / (high - low) per bar.
- `OhlcvBar::bar_midpoint_trend(bars)` — mean of consecutive bar midpoint differences.
- `OhlcvBar::bar_spread_score(bars)` — mean of (high - low) / close per bar.

**`norm` module — `MinMaxNormalizer` analytics (round 132)**
- `MinMaxNormalizer::window_max_run_up()` — maximum consecutive run of increasing values.
- `MinMaxNormalizer::window_max_run_dn()` — maximum consecutive run of decreasing values.
- `MinMaxNormalizer::window_diff_sum()` — sum of all consecutive differences in the window.
- `MinMaxNormalizer::window_run_length()` — longest directional run (up or down) in window.

**`norm` module — `ZScoreNormalizer` analytics (round 132)**
- `ZScoreNormalizer::window_max_run_up()` — maximum consecutive run of increasing values.
- `ZScoreNormalizer::window_max_run_dn()` — maximum consecutive run of decreasing values.
- `ZScoreNormalizer::window_diff_sum()` — sum of all consecutive differences in the window.
- `ZScoreNormalizer::window_run_length()` — longest directional run (up or down) in window.

---

## [2.8.2] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 131)**
- `NormalizedTick::price_range_momentum(ticks)` — (last - first) / (max - min) price range.
- `NormalizedTick::qty_imbalance_ratio(ticks)` — |buy_qty - sell_qty| / total sided quantity.
- `NormalizedTick::tick_flow_entropy(ticks)` — Shannon entropy of price change directions.
- `NormalizedTick::side_price_spread(ticks)` — mean buy price minus mean sell price.

**`ohlcv` module — `OhlcvBar` analytics (round 131)**
- `OhlcvBar::close_trend_strength(bars)` — (last close - first close) / close range.
- `OhlcvBar::bar_body_skew(bars)` — mean of signed body / range per bar.
- `OhlcvBar::bar_range_mean_dev(bars)` — mean absolute deviation of bar ranges.
- `OhlcvBar::bar_close_momentum(bars)` — sum of signed close-to-close directions.

**`norm` module — `MinMaxNormalizer` analytics (round 131)**
- `MinMaxNormalizer::window_last_pct_change()` — percentage change from first to last window value.
- `MinMaxNormalizer::window_std_trend()` — std of 2nd half minus std of 1st half of window.
- `MinMaxNormalizer::window_nonzero_count()` — count of non-zero values in the window.
- `MinMaxNormalizer::window_pct_above_mean()` — fraction of window values above the mean.

**`norm` module — `ZScoreNormalizer` analytics (round 131)**
- `ZScoreNormalizer::window_last_pct_change()` — percentage change from first to last window value.
- `ZScoreNormalizer::window_std_trend()` — std of 2nd half minus std of 1st half of window.
- `ZScoreNormalizer::window_nonzero_count()` — count of non-zero values in the window.
- `ZScoreNormalizer::window_pct_above_mean()` — fraction of window values above the mean.

---

## [2.8.1] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 130)**
- `NormalizedTick::price_median_deviation(ticks)` — mean absolute deviation from the median price.
- `NormalizedTick::tick_autocorr_lag1(ticks)` — lag-1 autocorrelation of consecutive price changes.
- `NormalizedTick::side_momentum_ratio(ticks)` — (buy_qty - sell_qty) / total_qty.
- `NormalizedTick::price_stability_score(ticks)` — 1 minus coefficient of variation of prices.

**`ohlcv` module — `OhlcvBar` analytics (round 130)**
- `OhlcvBar::close_body_range_ratio(bars)` — mean of body size / (high - low) per bar.
- `OhlcvBar::avg_body_pct(bars)` — mean of |close - open| / open per bar.
- `OhlcvBar::bar_symmetry(bars)` — mean of |upper_shadow - lower_shadow| / range.
- `OhlcvBar::open_gap_direction(bars)` — fraction of bars where open exceeds prior close.

**`norm` module — `MinMaxNormalizer` analytics (round 130)**
- `MinMaxNormalizer::window_mad()` — mean absolute deviation from the window mean.
- `MinMaxNormalizer::window_entropy_ratio()` — actual entropy / max possible entropy for window size.
- `MinMaxNormalizer::window_plateau_count()` — count of consecutive equal adjacent values.
- `MinMaxNormalizer::window_direction_bias()` — fraction up minus fraction down over window steps.

**`norm` module — `ZScoreNormalizer` analytics (round 130)**
- `ZScoreNormalizer::window_mad()` — mean absolute deviation from the window mean.
- `ZScoreNormalizer::window_entropy_ratio()` — actual entropy / max possible entropy for window size.
- `ZScoreNormalizer::window_plateau_count()` — count of consecutive equal adjacent values.
- `ZScoreNormalizer::window_direction_bias()` — fraction up minus fraction down over window steps.

---

## [2.8.0] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 129)**
- `NormalizedTick::price_wave_ratio(ticks)` — fraction of consecutive price moves that are upward.
- `NormalizedTick::qty_entropy_score(ticks)` — Shannon entropy of the quantity distribution.
- `NormalizedTick::tick_burst_rate(ticks)` — ratio of max inter-tick gap to mean inter-tick gap.
- `NormalizedTick::side_weighted_price(ticks)` — quantity-weighted mean price, buy positive / sell negative.

**`ohlcv` module — `OhlcvBar` analytics (round 129)**
- `OhlcvBar::bar_energy(bars)` — mean of squared bar ranges (high - low)^2.
- `OhlcvBar::open_close_persistence(bars)` — fraction of bars where open equals prior bar's close.
- `OhlcvBar::bar_range_trend(bars)` — mean of consecutive bar range differences.
- `OhlcvBar::open_high_spread(bars)` — mean of (high - open) / open across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 129)**
- `MinMaxNormalizer::window_cumulative_sum()` — sum of all values in the window.
- `MinMaxNormalizer::window_spread_ratio()` — (max - min) / |mean| of the window.
- `MinMaxNormalizer::window_center_of_mass()` — index-weighted center of mass of window values.
- `MinMaxNormalizer::window_cycle_count()` — number of direction reversals in the window.

**`norm` module — `ZScoreNormalizer` analytics (round 129)**
- `ZScoreNormalizer::window_cumulative_sum()` — sum of all values in the window.
- `ZScoreNormalizer::window_spread_ratio()` — (max - min) / |mean| of the window.
- `ZScoreNormalizer::window_center_of_mass()` — index-weighted center of mass of window values.
- `ZScoreNormalizer::window_cycle_count()` — number of direction reversals in the window.

---

## [2.7.9] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 128)**
- `NormalizedTick::price_bollinger_score(ticks)` — z-score of the last price relative to the window mean/std.
- `NormalizedTick::qty_log_mean(ticks)` — geometric mean of quantities (exp of mean log).
- `NormalizedTick::tick_speed_variance(ticks)` — variance of absolute price changes between consecutive ticks.
- `NormalizedTick::relative_price_strength(ticks)` — mean buy price divided by mean sell price.

**`ohlcv` module — `OhlcvBar` analytics (round 128)**
- `OhlcvBar::avg_close_slope(bars)` — mean of consecutive close differences.
- `OhlcvBar::body_range_zscore(bars)` — z-score of the last bar's body size within the window.
- `OhlcvBar::volume_entropy(bars)` — Shannon entropy of the volume distribution across bars.
- `OhlcvBar::low_persistence(bars)` — fraction of bars where low is below the prior bar's low.

**`norm` module — `MinMaxNormalizer` analytics (round 128)**
- `MinMaxNormalizer::window_loss_count()` — count of consecutive negative differences in the window.
- `MinMaxNormalizer::window_net_change()` — difference between last and first window value.
- `MinMaxNormalizer::window_acceleration()` — mean of second-order differences (change-of-change).
- `MinMaxNormalizer::window_regime_score()` — fraction above mean minus fraction below mean.

**`norm` module — `ZScoreNormalizer` analytics (round 128)**
- `ZScoreNormalizer::window_loss_count()` — count of consecutive negative differences in the window.
- `ZScoreNormalizer::window_net_change()` — difference between last and first window value.
- `ZScoreNormalizer::window_acceleration()` — mean of second-order differences (change-of-change).
- `ZScoreNormalizer::window_regime_score()` — fraction above mean minus fraction below mean.

---

## [2.7.8] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 127)**
- `NormalizedTick::tick_dispersion_ratio(ticks)` — coefficient of variation of price values.
- `NormalizedTick::price_linear_fit_error(ticks)` — mean squared error of a linear fit to prices.
- `NormalizedTick::qty_harmonic_mean(ticks)` — harmonic mean of quantity values.
- `NormalizedTick::late_trade_fraction(ticks)` — fraction of ticks in the second half of the slice.

**`ohlcv` module — `OhlcvBar` analytics (round 127)**
- `OhlcvBar::close_velocity(bars)` — mean absolute close change between consecutive bars.
- `OhlcvBar::open_range_score(bars)` — mean of (open - low) / range per bar.
- `OhlcvBar::body_trend_direction(bars)` — net fraction of bullish minus bearish bars.
- `OhlcvBar::bar_tightness(bars)` — mean of (high - low) relative to bar midpoint.

**`norm` module — `MinMaxNormalizer` analytics (round 127)**
- `MinMaxNormalizer::window_entropy_normalized()` — Shannon entropy normalized to [0, 1].
- `MinMaxNormalizer::window_peak_value()` — maximum value in the window.
- `MinMaxNormalizer::window_trough_value()` — minimum value in the window.
- `MinMaxNormalizer::window_gain_count()` — count of positive successive differences.

**`norm` module — `ZScoreNormalizer` analytics (round 127)**
- `ZScoreNormalizer::window_entropy_normalized()` — Shannon entropy normalized to [0, 1].
- `ZScoreNormalizer::window_peak_value()` — maximum value in the window.
- `ZScoreNormalizer::window_trough_value()` — minimum value in the window.
- `ZScoreNormalizer::window_gain_count()` — count of positive successive differences.

---

## [2.7.7] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 126)**
- `NormalizedTick::qty_rms(ticks)` — root mean square of quantity values.
- `NormalizedTick::bid_ask_proxy(ticks)` — mean price gap across alternating buy/sell tick pairs.
- `NormalizedTick::tick_price_accel(ticks)` — mean second-order price change (acceleration).
- `NormalizedTick::price_entropy_iqr(ticks)` — IQR of absolute price changes between consecutive ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 126)**
- `OhlcvBar::body_ema(bars)` — EMA (α=0.2) of body sizes across bars.
- `OhlcvBar::avg_true_range_ratio(bars)` — mean of (high - low) / prior close across bars.
- `OhlcvBar::close_body_fraction(bars)` — mean fraction of bar range that is body.
- `OhlcvBar::bar_momentum_accel(bars)` — mean second-order close change (acceleration).

**`norm` module — `MinMaxNormalizer` analytics (round 126)**
- `MinMaxNormalizer::window_ema_deviation()` — deviation of last value from EMA (α=0.2).
- `MinMaxNormalizer::window_normalized_variance()` — variance divided by mean squared.
- `MinMaxNormalizer::window_median_ratio()` — ratio of last value to window median.
- `MinMaxNormalizer::window_half_life()` — steps from peak to first value at or below peak/2.

**`norm` module — `ZScoreNormalizer` analytics (round 126)**
- `ZScoreNormalizer::window_ema_deviation()` — deviation of last value from EMA (α=0.2).
- `ZScoreNormalizer::window_normalized_variance()` — variance divided by mean squared.
- `ZScoreNormalizer::window_median_ratio()` — ratio of last value to window median.
- `ZScoreNormalizer::window_half_life()` — steps from peak to first value at or below peak/2.

---

## [2.7.6] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 125)**
- `NormalizedTick::price_hurst_estimate(ticks)` — simple Hurst exponent estimate via log-range scaling.
- `NormalizedTick::qty_mean_reversion(ticks)` — mean absolute deviation of quantity divided by std.
- `NormalizedTick::avg_trade_impact(ticks)` — mean relative price change between consecutive ticks.
- `NormalizedTick::price_range_iqr(ticks)` — interquartile range of price values.

**`ohlcv` module — `OhlcvBar` analytics (round 125)**
- `OhlcvBar::close_lag1_autocorr(bars)` — lag-1 autocorrelation of close prices.
- `OhlcvBar::volume_skewness(bars)` — skewness of volume distribution across bars.
- `OhlcvBar::bar_height_rank(bars)` — rank of last bar's range among all ranges.
- `OhlcvBar::high_persistence(bars)` — fraction of bars where high exceeds prior bar's high.

**`norm` module — `MinMaxNormalizer` analytics (round 125)**
- `MinMaxNormalizer::window_exp_smoothed()` — exponentially smoothed last value (α=0.2).
- `MinMaxNormalizer::window_drawdown()` — maximum peak-to-trough decline in the window.
- `MinMaxNormalizer::window_drawup()` — maximum trough-to-peak gain in the window.
- `MinMaxNormalizer::window_trend_strength()` — ratio of net signed movement to total movement.

**`norm` module — `ZScoreNormalizer` analytics (round 125)**
- `ZScoreNormalizer::window_exp_smoothed()` — exponentially smoothed last value (α=0.2).
- `ZScoreNormalizer::window_drawdown()` — maximum peak-to-trough decline in the window.
- `ZScoreNormalizer::window_drawup()` — maximum trough-to-peak gain in the window.
- `ZScoreNormalizer::window_trend_strength()` — ratio of net signed movement to total movement.

---

## [2.7.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 124)**
- `NormalizedTick::price_cross_zero(ticks)` — count of times price crosses its own mean.
- `NormalizedTick::tick_momentum_score(ticks)` — mean of sign of price changes across consecutive ticks.
- `NormalizedTick::sell_side_ratio(ticks)` — fraction of sided ticks that are sells.
- `NormalizedTick::avg_qty_per_side(ticks)` — mean quantity per sided tick.

**`ohlcv` module — `OhlcvBar` analytics (round 124)**
- `OhlcvBar::close_oscillation_count(bars)` — count of close direction changes across bars.
- `OhlcvBar::bar_consolidation_ratio(bars)` — fraction of bars with range below the median range.
- `OhlcvBar::open_momentum_score(bars)` — fraction of bars where open exceeds the prior bar's open.
- `OhlcvBar::avg_volume_change(bars)` — mean of absolute volume changes between consecutive bars.

**`norm` module — `MinMaxNormalizer` analytics (round 124)**
- `MinMaxNormalizer::window_percentile_75()` — 75th percentile of window values.
- `MinMaxNormalizer::window_abs_slope()` — absolute slope: |last - first| / (n - 1).
- `MinMaxNormalizer::window_gain_loss_ratio()` — ratio of sum of gains to sum of losses.
- `MinMaxNormalizer::window_range_stability()` — 1 minus std of range-normalized values.

**`norm` module — `ZScoreNormalizer` analytics (round 124)**
- `ZScoreNormalizer::window_percentile_75()` — 75th percentile of window values.
- `ZScoreNormalizer::window_abs_slope()` — absolute slope: |last - first| / (n - 1).
- `ZScoreNormalizer::window_gain_loss_ratio()` — ratio of sum of gains to sum of losses.
- `ZScoreNormalizer::window_range_stability()` — 1 minus std of range-normalized values.

---

## [2.7.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 123)**
- `NormalizedTick::qty_kurtosis(ticks)` — excess kurtosis of quantity distribution across ticks.
- `NormalizedTick::price_monotonicity(ticks)` — fraction of consecutive price pairs that are rising.
- `NormalizedTick::tick_count_per_second(ticks)` — tick rate using `received_at_ms` timestamps.
- `NormalizedTick::price_range_std(ticks)` — standard deviation of price across all ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 123)**
- `OhlcvBar::close_range_pct(bars)` — mean of (close - low) / (high - low) per bar.
- `OhlcvBar::avg_bar_range_pct(bars)` — mean of (high - low) / open as a fraction across bars.
- `OhlcvBar::open_to_low_ratio(bars)` — mean of (open - low) / (high - low): open proximity to the low.
- `OhlcvBar::bar_close_rank(bars)` — rank of last close among all closes (0.0 = lowest, 1.0 = highest).

**`norm` module — `MinMaxNormalizer` analytics (round 123)**
- `MinMaxNormalizer::window_trend_reversal_count()` — count of trend reversals (sign changes in diffs).
- `MinMaxNormalizer::window_first_last_diff()` — difference between first and last values in the window.
- `MinMaxNormalizer::window_upper_half_count()` — count of values in the upper half of the range.
- `MinMaxNormalizer::window_lower_half_count()` — count of values in the lower half of the range.

**`norm` module — `ZScoreNormalizer` analytics (round 123)**
- `ZScoreNormalizer::window_trend_reversal_count()` — count of trend reversals.
- `ZScoreNormalizer::window_first_last_diff()` — difference between first and last values.
- `ZScoreNormalizer::window_upper_half_count()` — count of values in the upper half of the range.
- `ZScoreNormalizer::window_lower_half_count()` — count of values in the lower half of the range.

---

## [2.7.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 122)**
- `NormalizedTick::price_range_ema(ticks)` — EMA (α=0.2) of absolute price changes between consecutive ticks.
- `NormalizedTick::qty_trend_ema(ticks)` — EMA (α=0.2) of quantity values across ticks.
- `NormalizedTick::weighted_mid_price(ticks)` — volume-weighted mean price across ticks.
- `NormalizedTick::tick_buy_qty_fraction(ticks)` — fraction of total quantity on the buy side.

**`ohlcv` module — `OhlcvBar` analytics (round 122)**
- `OhlcvBar::body_to_range_pct(bars)` — mean of body size as fraction of bar range.
- `OhlcvBar::avg_open_close_gap(bars)` — mean absolute gap between consecutive bar open and prior close.
- `OhlcvBar::high_low_body_ratio(bars)` — mean of upper shadow as fraction of bar range.
- `OhlcvBar::close_above_prior_high(bars)` — fraction of bars where close exceeds the prior bar's high.

**`norm` module — `MinMaxNormalizer` analytics (round 122)**
- `MinMaxNormalizer::window_last_rank()` — rank of the last value within the window (0.0 = min, 1.0 = max).
- `MinMaxNormalizer::window_momentum_score()` — mean of sign of successive differences.
- `MinMaxNormalizer::window_oscillation_count()` — count of local maxima in the window.
- `MinMaxNormalizer::window_skew_direction()` — direction of skew relative to median.

**`norm` module — `ZScoreNormalizer` analytics (round 122)**
- `ZScoreNormalizer::window_last_rank()` — rank of the last value within the window.
- `ZScoreNormalizer::window_momentum_score()` — mean of sign of successive differences.
- `ZScoreNormalizer::window_oscillation_count()` — count of local maxima in the window.
- `ZScoreNormalizer::window_skew_direction()` — direction of skew relative to median.

---

## [2.7.2] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 121)**
- `NormalizedTick::price_vol_correlation(ticks)` — Pearson correlation between price and quantity.
- `NormalizedTick::qty_acceleration(ticks)` — mean second-order quantity change across ticks.
- `NormalizedTick::buy_sell_price_diff(ticks)` — mean buy price minus mean sell price.
- `NormalizedTick::tick_imbalance_score(ticks)` — order flow imbalance as (buy_qty - sell_qty) / total.

**`ohlcv` module — `OhlcvBar` analytics (round 121)**
- `OhlcvBar::close_gap_ratio(bars)` — fraction of bars where open falls within the prior bar's range.
- `OhlcvBar::volume_deceleration(bars)` — mean volume drop across bars with decreasing volume.
- `OhlcvBar::bar_trend_persistence(bars)` — fraction of bars that continue the prior bar's direction.
- `OhlcvBar::shadow_body_ratio(bars)` — ratio of total shadow length to total body length.

**`norm` module — `MinMaxNormalizer` analytics (round 121)**
- `MinMaxNormalizer::window_range_fraction()` — fraction of window values in the lower half of range.
- `MinMaxNormalizer::window_mean_above_last()` — 1.0 if window mean exceeds last value, else 0.0.
- `MinMaxNormalizer::window_volatility_trend()` — std of second half minus std of first half.
- `MinMaxNormalizer::window_sign_change_count()` — count of sign changes in successive differences.

**`norm` module — `ZScoreNormalizer` analytics (round 121)**
- `ZScoreNormalizer::window_range_fraction()` — fraction of window values in the lower half of range.
- `ZScoreNormalizer::window_mean_above_last()` — 1.0 if window mean exceeds last value, else 0.0.
- `ZScoreNormalizer::window_volatility_trend()` — std of second half minus std of first half.
- `ZScoreNormalizer::window_sign_change_count()` — count of sign changes in successive differences.

---

## [2.7.1] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 120)**
- `NormalizedTick::price_jitter(ticks)` — mean squared price change between consecutive ticks.
- `NormalizedTick::tick_flow_ratio(ticks)` — buy volume as a fraction of total sided volume.
- `NormalizedTick::qty_skewness_abs(ticks)` — absolute skewness of quantity distribution.
- `NormalizedTick::side_balance_score(ticks)` — absolute deviation of buy fraction from 0.5.

**`ohlcv` module — `OhlcvBar` analytics (round 120)**
- `OhlcvBar::close_range_stability(bars)` — 1 minus std of close-position within bar range.
- `OhlcvBar::avg_bar_volatility(bars)` — mean of (high - low) / open across bars.
- `OhlcvBar::open_range_bias(bars)` — fraction of bars where open is above bar midpoint.
- `OhlcvBar::body_volatility(bars)` — standard deviation of body sizes across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 120)**
- `MinMaxNormalizer::window_above_last()` — count of window values strictly above the last.
- `MinMaxNormalizer::window_below_last()` — count of window values strictly below the last.
- `MinMaxNormalizer::window_diff_mean()` — mean of successive differences across the window.
- `MinMaxNormalizer::window_last_zscore()` — z-score of the last value relative to the window.

**`norm` module — `ZScoreNormalizer` analytics (round 120)**
- `ZScoreNormalizer::window_above_last()` — count of window values strictly above the last.
- `ZScoreNormalizer::window_below_last()` — count of window values strictly below the last.
- `ZScoreNormalizer::window_diff_mean()` — mean of successive differences across the window.
- `ZScoreNormalizer::window_last_zscore()` — z-score of the last value relative to the window.

---

## [2.7.0] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 119)**
- `NormalizedTick::price_rebound_rate(ticks)` — fraction of pairs that reverse after a prior reversal.
- `NormalizedTick::weighted_spread(ticks)` — volume-weighted mean absolute price difference between consecutive ticks.
- `NormalizedTick::buy_price_advantage(ticks)` — mean buy price minus mean sell price.
- `NormalizedTick::qty_entropy(ticks)` — Shannon entropy of quantity distribution across 8 buckets.

**`ohlcv` module — `OhlcvBar` analytics (round 119)**
- `OhlcvBar::open_close_midpoint(bars)` — mean of `(open + close) / 2` across bars.
- `OhlcvBar::volume_concentration_ratio(bars)` — fraction of total volume in the top-third of bars by volume.
- `OhlcvBar::bar_gap_fill_ratio(bars)` — fraction of bars where open falls within the prior bar's body.
- `OhlcvBar::net_shadow_direction(bars)` — net fraction of bars with upper-dominant vs lower-dominant shadows.

**`norm` module — `MinMaxNormalizer` analytics (round 119)**
- `MinMaxNormalizer::window_max_minus_min()` — range `(max - min)` of window values.
- `MinMaxNormalizer::window_normalized_mean()` — `(mean - min) / (max - min)` of window values.
- `MinMaxNormalizer::window_variance_ratio()` — ratio of variance to mean squared.
- `MinMaxNormalizer::window_max_minus_last()` — maximum window value minus the last value.

**`norm` module — `ZScoreNormalizer` analytics (round 119)**
- `ZScoreNormalizer::window_max_minus_min()` — range `(max - min)` of window values.
- `ZScoreNormalizer::window_normalized_mean()` — `(mean - min) / (max - min)` of window values.
- `ZScoreNormalizer::window_variance_ratio()` — ratio of variance to mean squared.
- `ZScoreNormalizer::window_max_minus_last()` — maximum window value minus the last value.

---

## [2.6.9] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 118)**
- `NormalizedTick::price_entropy_rate(ticks)` — mean absolute log-return as an entropy proxy.
- `NormalizedTick::qty_lag1_corr(ticks)` — lag-1 autocorrelation of tick quantities.
- `NormalizedTick::tick_side_transition_rate(ticks)` — fraction of consecutive sided pairs that change side.
- `NormalizedTick::avg_price_per_unit(ticks)` — mean price divided by mean quantity.

**`ohlcv` module — `OhlcvBar` analytics (round 118)**
- `OhlcvBar::avg_close_range_pct(bars)` — mean `(close - low) / (high - low)` position per bar.
- `OhlcvBar::volume_ratio_to_max(bars)` — mean ratio of each bar's volume to the slice maximum.
- `OhlcvBar::bar_consolidation_score(bars)` — `1 - avg_body_efficiency`; higher means tighter consolidation.
- `OhlcvBar::shadow_asymmetry(bars)` — mean `(upper_shadow - lower_shadow) / range` per bar.

**`norm` module — `MinMaxNormalizer` analytics (round 118)**
- `MinMaxNormalizer::window_rolling_min()` — minimum value in the window.
- `MinMaxNormalizer::window_negative_fraction()` — fraction of strictly negative window values.
- `MinMaxNormalizer::window_positive_fraction()` — fraction of strictly positive window values.
- `MinMaxNormalizer::window_last_minus_min()` — last window value minus the window minimum.

**`norm` module — `ZScoreNormalizer` analytics (round 118)**
- `ZScoreNormalizer::window_rolling_min()` — minimum value in the window.
- `ZScoreNormalizer::window_negative_fraction()` — fraction of strictly negative window values.
- `ZScoreNormalizer::window_positive_fraction()` — fraction of strictly positive window values.
- `ZScoreNormalizer::window_last_minus_min()` — last window value minus the window minimum.

---

## [2.6.8] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 117)**
- `NormalizedTick::price_momentum_slope(ticks)` — OLS slope of price over tick index.
- `NormalizedTick::qty_dispersion(ticks)` — coefficient of variation of quantities.
- `NormalizedTick::tick_buy_pct(ticks)` — fraction of sided ticks that are buy-side.
- `NormalizedTick::consecutive_price_rise(ticks)` — longest run of consecutive rising prices.

**`ohlcv` module — `OhlcvBar` analytics (round 117)**
- `OhlcvBar::close_above_open_pct(bars)` — fraction of bars where close > open.
- `OhlcvBar::avg_low_to_close(bars)` — mean `low - close` across bars.
- `OhlcvBar::bar_trend_score(bars)` — fraction of consecutive close pairs that are rising.
- `OhlcvBar::volume_above_avg_count(bars)` — count of bars with above-average volume.

**`norm` module — `MinMaxNormalizer` analytics (round 117)**
- `MinMaxNormalizer::window_entropy_of_changes()` — entropy of absolute differences between consecutive values.
- `MinMaxNormalizer::window_level_crossing_rate()` — rate at which values cross the window mean.
- `MinMaxNormalizer::window_abs_mean()` — mean of absolute window values.
- `MinMaxNormalizer::window_rolling_max()` — maximum value in the window.

**`norm` module — `ZScoreNormalizer` analytics (round 117)**
- `ZScoreNormalizer::window_entropy_of_changes()` — entropy of absolute differences between consecutive values.
- `ZScoreNormalizer::window_level_crossing_rate()` — rate at which values cross the window mean.
- `ZScoreNormalizer::window_abs_mean()` — mean of absolute window values.
- `ZScoreNormalizer::window_rolling_max()` — maximum value in the window.

---

## [2.6.7] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 116)**
- `NormalizedTick::price_momentum_index(ticks)` — fraction of consecutive pairs with rising price.
- `NormalizedTick::qty_range_ratio(ticks)` — quantity range divided by mean quantity.
- `NormalizedTick::recent_price_change(ticks)` — price change from second-to-last to last tick.
- `NormalizedTick::sell_dominance_streak(ticks)` — longest consecutive run of sell-side ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 116)**
- `OhlcvBar::avg_high_to_close(bars)` — mean `high - close` across bars.
- `OhlcvBar::bar_size_entropy(bars)` — Shannon entropy of bar body-size distribution.
- `OhlcvBar::close_to_open_pct(bars)` — mean `(close - open) / open` percentage per bar.
- `OhlcvBar::body_direction_score(bars)` — fraction of bullish (close > open) bars.

**`norm` module — `MinMaxNormalizer` analytics (round 116)**
- `MinMaxNormalizer::window_max_deviation()` — maximum absolute deviation from the window mean.
- `MinMaxNormalizer::window_range_mean_ratio()` — ratio of window range to mean.
- `MinMaxNormalizer::window_step_up_count()` — count of strictly increasing consecutive pairs.
- `MinMaxNormalizer::window_step_down_count()` — count of strictly decreasing consecutive pairs.

**`norm` module — `ZScoreNormalizer` analytics (round 116)**
- `ZScoreNormalizer::window_max_deviation()` — maximum absolute deviation from the window mean.
- `ZScoreNormalizer::window_range_mean_ratio()` — ratio of window range to mean.
- `ZScoreNormalizer::window_step_up_count()` — count of strictly increasing consecutive pairs.
- `ZScoreNormalizer::window_step_down_count()` — count of strictly decreasing consecutive pairs.

---

## [2.6.6] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 115)**
- `NormalizedTick::mid_price_mean(ticks)` — mean mid-price between consecutive tick pairs.
- `NormalizedTick::tick_qty_range(ticks)` — range `max_qty - min_qty` across the slice.
- `NormalizedTick::buy_dominance_streak(ticks)` — longest consecutive run of buy-side ticks.
- `NormalizedTick::price_gap_mean(ticks)` — mean absolute price gap between consecutive ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 115)**
- `OhlcvBar::avg_open_gap(bars)` — mean absolute `|open - prev_close|` gap across bar pairs.
- `OhlcvBar::hl_ratio_mean(bars)` — mean ratio of high to low per bar.
- `OhlcvBar::shadow_to_range_ratio(bars)` — mean ratio of total shadow length to bar range.
- `OhlcvBar::avg_close_to_low(bars)` — mean `close - low` across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 115)**
- `MinMaxNormalizer::window_log_return()` — mean log return between consecutive window values.
- `MinMaxNormalizer::window_signed_rms()` — RMS with the sign of the window mean.
- `MinMaxNormalizer::window_inflection_count()` — count of local minima and maxima in the window.
- `MinMaxNormalizer::window_centroid()` — index-weighted centroid position of the window.

**`norm` module — `ZScoreNormalizer` analytics (round 115)**
- `ZScoreNormalizer::window_log_return()` — mean log return between consecutive window values.
- `ZScoreNormalizer::window_signed_rms()` — RMS with the sign of the window mean.
- `ZScoreNormalizer::window_inflection_count()` — count of local minima and maxima in the window.
- `ZScoreNormalizer::window_centroid()` — index-weighted centroid position of the window.

---

## [2.6.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 114)**
- `NormalizedTick::price_std_ratio(ticks)` — ratio of price std dev to mean price.
- `NormalizedTick::qty_trend_strength(ticks)` — Pearson correlation of quantity with tick index.
- `NormalizedTick::buy_to_sell_gap(ticks)` — mean absolute price gap at buy/sell transitions.
- `NormalizedTick::tick_range_efficiency(ticks)` — net price move as fraction of total price range.

**`ohlcv` module — `OhlcvBar` analytics (round 114)**
- `OhlcvBar::close_reversal_rate(bars)` — fraction of eligible pairs where close direction reverses.
- `OhlcvBar::avg_body_efficiency(bars)` — mean ratio of body size to bar range.
- `OhlcvBar::volume_zscore(bars)` — z-score of the last bar's volume relative to the slice.
- `OhlcvBar::body_skew(bars)` — skewness of bar body sizes across the slice.

**`norm` module — `MinMaxNormalizer` analytics (round 114)**
- `MinMaxNormalizer::window_crest_factor()` — peak absolute value divided by RMS.
- `MinMaxNormalizer::window_relative_range()` — `(max - min) / mean` of window values.
- `MinMaxNormalizer::window_outlier_count()` — count of values >2 std devs from the mean.
- `MinMaxNormalizer::window_decay_score()` — exponentially decay-weighted mean (alpha=0.5).

**`norm` module — `ZScoreNormalizer` analytics (round 114)**
- `ZScoreNormalizer::window_crest_factor()` — peak absolute value divided by RMS.
- `ZScoreNormalizer::window_relative_range()` — `(max - min) / mean` of window values.
- `ZScoreNormalizer::window_outlier_count()` — count of values >2 std devs from the mean.
- `ZScoreNormalizer::window_decay_score()` — exponentially decay-weighted mean (alpha=0.5).

---

## [2.6.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 113)**
- `NormalizedTick::avg_inter_tick_gap(ticks)` — mean inter-tick gap in milliseconds.
- `NormalizedTick::tick_intensity(ticks)` — number of ticks per second over the time span.
- `NormalizedTick::price_swing(ticks)` — `(max - min) / min` as a fractional price range.
- `NormalizedTick::qty_velocity(ticks)` — mean rate of quantity change between consecutive ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 113)**
- `OhlcvBar::range_to_volume_ratio(bars)` — mean ratio of bar range to volume.
- `OhlcvBar::avg_high_low_spread(bars)` — mean `high - low` spread across bars.
- `OhlcvBar::candle_persistence(bars)` — fraction of bars where close direction matches prior bar.
- `OhlcvBar::bar_range_zscore(bars)` — mean z-score of each bar's range relative to all ranges.

**`norm` module — `MinMaxNormalizer` analytics (round 113)**
- `MinMaxNormalizer::window_iqr_ratio()` — ratio of IQR to median.
- `MinMaxNormalizer::window_mean_reversion()` — fraction of steps moving toward the window mean.
- `MinMaxNormalizer::window_autocorrelation()` — lag-1 autocorrelation of window values.
- `MinMaxNormalizer::window_slope()` — OLS slope of window values over their index.

**`norm` module — `ZScoreNormalizer` analytics (round 113)**
- `ZScoreNormalizer::window_iqr_ratio()` — ratio of IQR to median.
- `ZScoreNormalizer::window_mean_reversion()` — fraction of steps moving toward the window mean.
- `ZScoreNormalizer::window_autocorrelation()` — lag-1 autocorrelation of window values.
- `ZScoreNormalizer::window_slope()` — OLS slope of window values over their index.

---

## [2.6.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 112)**
- `NormalizedTick::price_reversal_rate(ticks)` — fraction of consecutive direction pairs that reverse.
- `NormalizedTick::qty_ema(ticks)` — exponential moving average of trade quantities.
- `NormalizedTick::last_buy_price(ticks)` — price of the most recent buy-side tick.
- `NormalizedTick::last_sell_price(ticks)` — price of the most recent sell-side tick.

**`ohlcv` module — `OhlcvBar` analytics (round 112)**
- `OhlcvBar::wicks_to_body_ratio(bars)` — mean ratio of total wick length to body length.
- `OhlcvBar::avg_close_deviation(bars)` — mean absolute deviation of close prices from their mean.
- `OhlcvBar::open_midpoint_ratio(bars)` — mean ratio of open to `(high+low)/2`.
- `OhlcvBar::volume_weighted_close_change(bars)` — volume-weighted mean of close-to-close changes.

**`norm` module — `MinMaxNormalizer` analytics (round 112)**
- `MinMaxNormalizer::window_harmonic_mean()` — harmonic mean of window values.
- `MinMaxNormalizer::window_geometric_std()` — geometric standard deviation of window values.
- `MinMaxNormalizer::window_entropy_rate()` — mean absolute first differences as entropy rate proxy.
- `MinMaxNormalizer::window_burstiness()` — burstiness index `(std - mean) / (std + mean)`.

**`norm` module — `ZScoreNormalizer` analytics (round 112)**
- `ZScoreNormalizer::window_harmonic_mean()` — harmonic mean of window values.
- `ZScoreNormalizer::window_geometric_std()` — geometric standard deviation of window values.
- `ZScoreNormalizer::window_entropy_rate()` — mean absolute first differences as entropy rate proxy.
- `ZScoreNormalizer::window_burstiness()` — burstiness index `(std - mean) / (std + mean)`.

---

## [2.6.2] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 111)**
- `NormalizedTick::tick_price_entropy(ticks)` — Shannon entropy of price distribution across 10 buckets.
- `NormalizedTick::average_spread(ticks)` — mean absolute price change between consecutive ticks.
- `NormalizedTick::tick_sigma(ticks)` — standard deviation of prices across the slice.
- `NormalizedTick::downside_qty_fraction(ticks)` — fraction of quantity on ticks below the mean price.

**`ohlcv` module — `OhlcvBar` analytics (round 111)**
- `OhlcvBar::open_close_range(bars)` — mean absolute `|close - open|` body size across bars.
- `OhlcvBar::volume_per_bar(bars)` — mean volume per bar.
- `OhlcvBar::price_momentum_mean(bars)` — mean `(close - prev_close) / prev_close` across bars.
- `OhlcvBar::avg_intrabar_efficiency(bars)` — mean `(close - open) / (high - low)` across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 111)**
- `MinMaxNormalizer::window_trimmed_sum()` — sum of middle 80% of window values (10% trim each end).
- `MinMaxNormalizer::window_range_zscore()` — z-score of the window range relative to its mean.
- `MinMaxNormalizer::window_above_median_count()` — count of values strictly above the window median.
- `MinMaxNormalizer::window_min_run()` — maximum length of a consecutive decreasing run.

**`norm` module — `ZScoreNormalizer` analytics (round 111)**
- `ZScoreNormalizer::window_trimmed_sum()` — sum of middle 80% of window values (10% trim each end).
- `ZScoreNormalizer::window_range_zscore()` — z-score of the window range relative to its mean.
- `ZScoreNormalizer::window_above_median_count()` — count of values strictly above the window median.
- `ZScoreNormalizer::window_min_run()` — maximum length of a consecutive decreasing run.

---

## [2.6.1] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 110)**
- `NormalizedTick::first_to_last_price(ticks)` — price change from first to last tick.
- `NormalizedTick::tick_volume_profile(ticks)` — count of distinct price levels in the slice.
- `NormalizedTick::price_quartile_range(ticks)` — interquartile range (Q3 - Q1) of prices.
- `NormalizedTick::buy_pressure_index(ticks)` — buy fraction minus 0.5, scaled to [-1, 1].

**`ohlcv` module — `OhlcvBar` analytics (round 110)**
- `OhlcvBar::avg_shadow_total(bars)` — mean total shadow length `range - body` across bars.
- `OhlcvBar::open_above_prev_close(bars)` — count of bars that open above the prior close.
- `OhlcvBar::close_below_prev_open(bars)` — count of bars that close below the prior open.
- `OhlcvBar::candle_range_efficiency(bars)` — mean `|close - open| / (high - low)` body-to-range ratio.

**`norm` module — `MinMaxNormalizer` analytics (round 110)**
- `MinMaxNormalizer::window_pairwise_mean_diff()` — mean of all pairwise absolute differences.
- `MinMaxNormalizer::window_q3()` — 75th-percentile value of the window.
- `MinMaxNormalizer::window_coefficient_of_variation()` — coefficient of variation (`std / mean`).
- `MinMaxNormalizer::window_second_moment()` — second statistical moment (mean of squared values).

**`norm` module — `ZScoreNormalizer` analytics (round 110)**
- `ZScoreNormalizer::window_pairwise_mean_diff()` — mean of all pairwise absolute differences.
- `ZScoreNormalizer::window_q3()` — 75th-percentile value of the window.
- `ZScoreNormalizer::window_coefficient_of_variation()` — coefficient of variation (`std / mean`).
- `ZScoreNormalizer::window_second_moment()` — second statistical moment (mean of squared values).

---

## [2.6.0] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 109)**
- `NormalizedTick::sell_tick_count(ticks)` — count of ticks on the sell side.
- `NormalizedTick::inter_tick_range_ms(ticks)` — range (max - min) of inter-tick gaps in milliseconds.
- `NormalizedTick::net_qty_flow(ticks)` — buy total quantity minus sell total quantity.
- `NormalizedTick::qty_skew_ratio(ticks)` — ratio of maximum to minimum trade quantity.

**`ohlcv` module — `OhlcvBar` analytics (round 109)**
- `OhlcvBar::open_gap_frequency(bars)` — fraction of bars that open at a different price than the prior close.
- `OhlcvBar::avg_close_to_open(bars)` — mean intra-bar return `(close - open) / open` across bars.
- `OhlcvBar::close_cross_open_count(bars)` — count of bars where close crosses through the prior bar's open.
- `OhlcvBar::trailing_stop_distance(bars)` — mean `close - min(low over 3-bar window)` trailing stop distance.

**`norm` module — `MinMaxNormalizer` analytics (round 109)**
- `MinMaxNormalizer::window_zscore_mean()` — mean z-score of window values (always ~0, sanity check).
- `MinMaxNormalizer::window_positive_sum()` — sum of positive values in the window.
- `MinMaxNormalizer::window_negative_sum()` — sum of negative values in the window.
- `MinMaxNormalizer::window_trend_consistency()` — fraction of steps consistent with the overall trend.

**`norm` module — `ZScoreNormalizer` analytics (round 109)**
- `ZScoreNormalizer::window_zscore_mean()` — mean z-score of window values (always ~0, sanity check).
- `ZScoreNormalizer::window_positive_sum()` — sum of positive values in the window.
- `ZScoreNormalizer::window_negative_sum()` — sum of negative values in the window.
- `ZScoreNormalizer::window_trend_consistency()` — fraction of steps consistent with the overall trend.

---

## [2.5.9] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 108)**
- `NormalizedTick::last_price_change(ticks)` — price change between the last two ticks.
- `NormalizedTick::buy_tick_rate(ticks)` — buy tick count per millisecond of time span.
- `NormalizedTick::qty_median_absolute_deviation(ticks)` — median absolute deviation of trade quantities.
- `NormalizedTick::price_percentile_25(ticks)` — 25th-percentile price across the slice.

**`ohlcv` module — `OhlcvBar` analytics (round 108)**
- `OhlcvBar::close_to_prev_open(bars)` — mean `close - prev_open` across consecutive bars.
- `OhlcvBar::momentum_ratio(bars)` — mean `|close - prev_close| / prev_close` across bars.
- `OhlcvBar::volume_range_ratio(bars)` — volume range `(max - min) / mean` across bars.
- `OhlcvBar::body_upper_fraction(bars)` — mean fraction of body lying above the bar midpoint.

**`norm` module — `MinMaxNormalizer` analytics (round 108)**
- `MinMaxNormalizer::window_root_mean_square()` — root mean square of window values.
- `MinMaxNormalizer::window_first_derivative_mean()` — mean of first differences across the window.
- `MinMaxNormalizer::window_l1_norm()` — L1 norm (sum of absolute values) of the window.
- `MinMaxNormalizer::window_percentile_10()` — 10th-percentile value of the window.

**`norm` module — `ZScoreNormalizer` analytics (round 108)**
- `ZScoreNormalizer::window_root_mean_square()` — root mean square of window values.
- `ZScoreNormalizer::window_first_derivative_mean()` — mean of first differences across the window.
- `ZScoreNormalizer::window_l1_norm()` — L1 norm (sum of absolute values) of the window.
- `ZScoreNormalizer::window_percentile_10()` — 10th-percentile value of the window.

---

## [2.5.8] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 107)**
- `NormalizedTick::max_buy_price(ticks)` — maximum price among buy-side ticks.
- `NormalizedTick::min_sell_price(ticks)` — minimum price among sell-side ticks.
- `NormalizedTick::price_range_ratio(ticks)` — `(max - min) / mean` price range normalised by mean.
- `NormalizedTick::qty_weighted_price_change(ticks)` — quantity-weighted sum of absolute price changes.

**`ohlcv` module — `OhlcvBar` analytics (round 107)**
- `OhlcvBar::open_to_high_ratio(bars)` — mean `(high - open) / high` across bars.
- `OhlcvBar::close_range_position(bars)` — mean close position within the bar's high-low range.
- `OhlcvBar::up_gap_count(bars)` — count of bars that gap up from the prior bar's high.
- `OhlcvBar::high_to_prev_close(bars)` — mean `(high / prev_close - 1)` overnight gap fraction.

**`norm` module — `MinMaxNormalizer` analytics (round 107)**
- `MinMaxNormalizer::window_energy()` — sum of squared window values (signal energy).
- `MinMaxNormalizer::window_interquartile_mean()` — mean of the middle 50% of window values.
- `MinMaxNormalizer::above_mean_count()` — count of window values exceeding the mean.
- `MinMaxNormalizer::window_diff_entropy()` — approximate differential entropy via log of variance.

**`norm` module — `ZScoreNormalizer` analytics (round 107)**
- `ZScoreNormalizer::window_energy()` — sum of squared window values (signal energy).
- `ZScoreNormalizer::window_interquartile_mean()` — mean of the middle 50% of window values.
- `ZScoreNormalizer::above_mean_count()` — count of window values exceeding the mean.
- `ZScoreNormalizer::window_diff_entropy()` — approximate differential entropy via log of variance.

---

## [2.5.7] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 106)**
- `NormalizedTick::price_zscore(ticks)` — z-score of the latest tick price relative to the slice.
- `NormalizedTick::buy_side_fraction(ticks)` — fraction of total ticks that are on the buy side.
- `NormalizedTick::tick_qty_cv(ticks)` — coefficient of variation of trade quantities.
- `NormalizedTick::avg_trade_value(ticks)` — mean of `price × quantity` across all ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 106)**
- `OhlcvBar::avg_true_range_pct(bars)` — mean `(high - low) / close` normalised range fraction.
- `OhlcvBar::close_above_midpoint_count(bars)` — count of bars where close is above `(high + low) / 2`.
- `OhlcvBar::volume_weighted_high(bars)` — volume-weighted high price across all bars.
- `OhlcvBar::low_minus_close_mean(bars)` — mean lower wick `min(open, close) - low` across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 106)**
- `MinMaxNormalizer::window_median_deviation()` — mean absolute deviation from the window median.
- `MinMaxNormalizer::longest_above_mean_run()` — longest consecutive run of values above the mean.
- `MinMaxNormalizer::window_bimodality()` — bimodality coefficient `(skewness² + 1) / kurtosis`.
- `MinMaxNormalizer::window_zero_crossings()` — count of sign changes relative to zero.

**`norm` module — `ZScoreNormalizer` analytics (round 106)**
- `ZScoreNormalizer::window_median_deviation()` — mean absolute deviation from the window median.
- `ZScoreNormalizer::longest_above_mean_run()` — longest consecutive run of values above the mean.
- `ZScoreNormalizer::window_bimodality()` — bimodality coefficient `(skewness² + 1) / kurtosis`.
- `ZScoreNormalizer::window_zero_crossings()` — count of sign changes relative to zero.

---

## [2.5.6] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 105)**
- `NormalizedTick::tick_burst_count(ticks)` — count of inter-tick gaps shorter than the median gap.
- `NormalizedTick::price_trend_score(ticks)` — fraction of consecutive tick pairs where price increases.
- `NormalizedTick::sell_qty_fraction(ticks)` — fraction of total quantity traded on the sell side.
- `NormalizedTick::qty_above_median(ticks)` — count of ticks whose quantity exceeds the median.

**`ohlcv` module — `OhlcvBar` analytics (round 105)**
- `OhlcvBar::close_to_high_mean(bars)` — mean `(high - close) / (high - low)` across bars with non-zero range.
- `OhlcvBar::bar_volatility_score(bars)` — mean true range divided by mean close price.
- `OhlcvBar::bearish_close_fraction(bars)` — fraction of bars where close is below open.
- `OhlcvBar::high_minus_open_mean(bars)` — mean of `high - open` across all bars.

**`norm` module — `MinMaxNormalizer` analytics (round 105)**
- `MinMaxNormalizer::window_hurst_exponent()` — approximate Hurst exponent via rescaled range analysis.
- `MinMaxNormalizer::window_mean_crossings()` — count of times the series crosses its own mean.
- `MinMaxNormalizer::window_skewness()` — sample skewness of window values.
- `MinMaxNormalizer::window_max_run()` — maximum length of a consecutive increasing run.

**`norm` module — `ZScoreNormalizer` analytics (round 105)**
- `ZScoreNormalizer::window_hurst_exponent()` — approximate Hurst exponent via rescaled range analysis.
- `ZScoreNormalizer::window_mean_crossings()` — count of times the series crosses its own mean.
- `ZScoreNormalizer::window_skewness()` — sample skewness of window values.
- `ZScoreNormalizer::window_max_run()` — maximum length of a consecutive increasing run.

---

## [2.5.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 104)**
- `NormalizedTick::qty_percentile_75(ticks)` — 75th-percentile trade quantity across the slice.
- `NormalizedTick::large_qty_count(ticks)` — count of ticks whose quantity exceeds the mean.
- `NormalizedTick::price_rms(ticks)` — root mean square of prices across the slice.
- `NormalizedTick::weighted_tick_count(ticks)` — total quantity as a quantity-weighted tick count.

**`ohlcv` module — `OhlcvBar` analytics (round 104)**
- `OhlcvBar::gap_fill_count(bars)` — count of bars where close re-enters the prior bar's range after gapping.
- `OhlcvBar::avg_body_to_volume(bars)` — mean ratio of candle body size to bar volume.
- `OhlcvBar::price_recovery_ratio(bars)` — fraction of bullish bars that also close higher than the prior bar.
- `OhlcvBar::open_close_correlation(bars)` — Pearson correlation between open and close prices across bars.

**`norm` module — `MinMaxNormalizer` analytics (round 104)**
- `MinMaxNormalizer::window_convexity()` — mean second difference (acceleration) across the window.
- `MinMaxNormalizer::below_previous_fraction()` — fraction of values strictly below their predecessor.
- `MinMaxNormalizer::window_volatility_ratio()` — std dev of second half divided by first half of the window.
- `MinMaxNormalizer::window_gini()` — Gini coefficient measuring inequality among window values.

**`norm` module — `ZScoreNormalizer` analytics (round 104)**
- `ZScoreNormalizer::window_convexity()` — mean second difference (acceleration) across the window.
- `ZScoreNormalizer::below_previous_fraction()` — fraction of values strictly below their predecessor.
- `ZScoreNormalizer::window_volatility_ratio()` — std dev of second half divided by first half of the window.
- `ZScoreNormalizer::window_gini()` — Gini coefficient measuring inequality among window values.

---

## [2.5.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 103)**
- `NormalizedTick::qty_range(ticks)` — difference between maximum and minimum trade quantity.
- `NormalizedTick::time_weighted_qty(ticks)` — quantity weighted by inter-tick gap duration.
- `NormalizedTick::above_vwap_fraction(ticks)` — fraction of ticks whose price exceeds the slice VWAP.
- `NormalizedTick::tick_speed(ticks)` — price range divided by total time span in milliseconds.

**`ohlcv` module — `OhlcvBar` analytics (round 103)**
- `OhlcvBar::open_high_distance(bars)` — mean `(high - open) / (high - low)` across bars with non-zero range.
- `OhlcvBar::max_close_minus_open(bars)` — maximum `close - open` across all bars.
- `OhlcvBar::bullish_engulfing_count(bars)` — count of bullish engulfing candlestick patterns.
- `OhlcvBar::shadow_ratio_score(bars)` — mean upper-to-lower shadow ratio across qualifying bars.

**`norm` module — `MinMaxNormalizer` analytics (round 103)**
- `MinMaxNormalizer::window_max_drawdown()` — maximum peak-to-trough drawdown within the window.
- `MinMaxNormalizer::above_previous_fraction()` — fraction of values exceeding their immediate predecessor.
- `MinMaxNormalizer::range_efficiency()` — net move divided by total absolute step-wise movement.
- `MinMaxNormalizer::window_running_total()` — sum of all values currently in the window.

**`norm` module — `ZScoreNormalizer` analytics (round 103)**
- `ZScoreNormalizer::window_max_drawdown()` — maximum peak-to-trough drawdown within the window.
- `ZScoreNormalizer::above_previous_fraction()` — fraction of values exceeding their immediate predecessor.
- `ZScoreNormalizer::range_efficiency()` — net move divided by total absolute step-wise movement.
- `ZScoreNormalizer::window_running_total()` — sum of all values currently in the window.

---

## [2.5.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 102)**
- `NormalizedTick::price_impact_ratio(ticks)` — absolute net price move per unit of total volume.
- `NormalizedTick::consecutive_sell_streak(ticks)` — longest consecutive run of sell-side ticks.
- `NormalizedTick::avg_qty_variance(ticks)` — population variance of tick quantities.
- `NormalizedTick::price_midpoint(ticks)` — `(max_price + min_price) / 2` across the slice.

**`ohlcv` module — `OhlcvBar` analytics (round 102)**
- `OhlcvBar::high_low_ratio(bars)` — mean `high / low` ratio per bar.
- `OhlcvBar::close_change_mean(bars)` — mean signed close-to-close change.
- `OhlcvBar::down_body_fraction(bars)` — fraction of bars where `close < open`.
- `OhlcvBar::body_acceleration(bars)` — rate of change of mean body size (second half vs first half).

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 102)**
- `window_q1_q3_ratio() -> Option<Decimal>` — ratio of 25th to 75th percentile.
- `signed_momentum() -> Option<Decimal>` — sum of +1/−1/0 for each consecutive pair direction.
- `positive_run_length() -> Option<f64>` — mean length of consecutive increasing runs.
- `valley_to_peak_ratio() -> Option<f64>` — last trough value divided by last peak value.

---

## [2.5.2] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 101)**
- `NormalizedTick::avg_buy_price(ticks)` — average price of buy-side ticks.
- `NormalizedTick::avg_sell_price(ticks)` — average price of sell-side ticks.
- `NormalizedTick::price_spread_ratio(ticks)` — `(high − low) / VWAP` for the tick window.
- `NormalizedTick::trade_size_entropy(ticks)` — approximate entropy of trade size distribution (5-bin).

**`ohlcv` module — `OhlcvBar` analytics (round 101)**
- `OhlcvBar::open_range_ratio(bars)` — mean `(open − low) / (high − low)` per bar.
- `OhlcvBar::volume_normalized_range(bars)` — mean `volume / (high − low)` per bar.
- `OhlcvBar::consecutive_flat_count(bars)` — length of trailing run of near-doji bars (body/range < 5%).
- `OhlcvBar::close_vs_midpoint(bars)` — mean `(close − midpoint) / (high − low)` per bar.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 101)**
- `window_signed_area() -> Option<Decimal>` — sum of deviations from the window mean.
- `up_fraction() -> Option<f64>` — fraction of window values strictly above zero.
- `threshold_cross_count() -> Option<usize>` — number of mean crossings in the rolling window.
- `window_entropy_approx() -> Option<f64>` — approximate entropy using 4 equal-width bins.

---

## [2.5.1] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 100)**
- `NormalizedTick::consecutive_buy_streak(ticks)` — longest consecutive run of buy-side ticks.
- `NormalizedTick::qty_concentration_ratio(ticks)` — Herfindahl-like concentration of quantity shares.
- `NormalizedTick::price_level_count(ticks)` — number of distinct price levels in the slice.
- `NormalizedTick::tick_count_per_price_level(ticks)` — mean ticks per distinct price level.

**`ohlcv` module — `OhlcvBar` analytics (round 100)**
- `OhlcvBar::median_volume(bars)` — median volume across all bars.
- `OhlcvBar::bar_count_above_avg_range(bars)` — number of bars with range above the mean range.
- `OhlcvBar::price_oscillation_count(bars)` — number of close-price direction reversals.
- `OhlcvBar::vwap_deviation_mean(bars)` — mean absolute deviation of closes from the volume-weighted close.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 100)**
- `window_trough_count() -> Option<usize>` — number of local troughs in the rolling window.
- `positive_momentum_fraction() -> Option<f64>` — fraction of consecutive pairs where second > first.
- `below_percentile_10() -> Option<Decimal>` — 10th percentile of the rolling window.
- `alternation_rate() -> Option<f64>` — fraction of directional pairs that reverse direction.

---

## [2.5.0] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 99)**
- `NormalizedTick::price_change_acceleration(ticks)` — rate of change of mean inter-tick price change (second half vs first half).
- `NormalizedTick::avg_qty_per_direction(ticks)` — mean quantity per sided tick (buys and sells combined).
- `NormalizedTick::micro_price(ticks)` — volume-weighted mid-price using buy/sell quantity as proxy for bid/ask imbalance.
- `NormalizedTick::inter_tick_gap_iqr(ticks)` — interquartile range of inter-arrival gaps in milliseconds.

**`ohlcv` module — `OhlcvBar` analytics (round 99)**
- `OhlcvBar::avg_lower_shadow(bars)` — mean lower shadow as a fraction of bar range.
- `OhlcvBar::inside_bar_count(bars)` — number of bars fully contained within the previous bar's range.
- `OhlcvBar::price_channel_width(bars)` — max high minus min low across the full slice.
- `OhlcvBar::volume_trend_acceleration(bars)` — `(second_half_mean − first_half_mean) / first_half_mean` of volume.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 99)**
- `window_percentile_25() -> Option<Decimal>` — 25th percentile of the rolling window.
- `mean_reversion_score() -> Option<f64>` — distance of latest value from window mean as fraction of window range.
- `trend_strength() -> Option<f64>` — |second_half_mean − first_half_mean| / window std-dev.
- `window_peak_count() -> Option<usize>` — number of local peaks in the rolling window.

---

## [2.4.9] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 98)**
- `NormalizedTick::tick_reversal_ratio(ticks)` — fraction of consecutive direction pairs that reverse.
- `NormalizedTick::first_half_vwap(ticks)` — VWAP of the first half of the tick slice.
- `NormalizedTick::second_half_vwap(ticks)` — VWAP of the second half of the tick slice.
- `NormalizedTick::qty_momentum(ticks)` — last tick quantity minus first tick quantity.

**`ohlcv` module — `OhlcvBar` analytics (round 98)**
- `OhlcvBar::narrow_body_count(bars)` — number of bars with body-to-range ratio below 10% (doji-like).
- `OhlcvBar::bar_range_mean(bars)` — mean `high − low` across all bars.
- `OhlcvBar::close_proximity(bars)` — mean `(close − low) / (high − low)` per bar.
- `OhlcvBar::down_gap_count(bars)` — number of downward open-to-prev-close gaps.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 98)**
- `window_kurtosis() -> Option<f64>` — excess kurtosis (fourth standardized moment − 3) of the window.
- `above_percentile_90() -> Option<f64>` — fraction of window values above the 90th percentile.
- `window_lag_autocorr() -> Option<f64>` — lag-1 autocorrelation of the rolling window.
- `slope_of_mean() -> Option<f64>` — slope between first-half mean and second-half mean.

---

## [2.4.8] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 97)**
- `NormalizedTick::price_gap_count(ticks)` — number of times consecutive tick prices cross the VWAP line.
- `NormalizedTick::tick_density(ticks)` — ticks per millisecond of the total window span.
- `NormalizedTick::buy_qty_mean(ticks)` — mean quantity of buy-side ticks.
- `NormalizedTick::sell_qty_mean(ticks)` — mean quantity of sell-side ticks.
- `NormalizedTick::price_range_asymmetry(ticks)` — signed asymmetry of the high−low range around its midpoint.

**`ohlcv` module — `OhlcvBar` analytics (round 97)**
- `OhlcvBar::close_to_open_gap(bars)` — mean close-to-open gap as a fraction of prior close.
- `OhlcvBar::volume_weighted_open(bars)` — volume-weighted average open price.
- `OhlcvBar::avg_upper_shadow(bars)` — mean upper shadow as a fraction of bar range.
- `OhlcvBar::body_to_range_mean(bars)` — mean `|body| / range` per bar.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 97)**
- `window_momentum() -> Option<Decimal>` — latest value minus the oldest value in the window.
- `above_first_fraction() -> Option<f64>` — fraction of window values strictly above the oldest value.
- `window_zscore_latest() -> Option<f64>` — z-score of the latest observation within the window.
- `decay_weighted_mean(alpha) -> Option<f64>` — exponentially-decayed weighted mean (newest weight = alpha).

---

## [2.4.7] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 96)**
- `NormalizedTick::qty_weighted_spread(ticks)` — quantity-weighted average deviation of tick prices from VWAP.
- `NormalizedTick::large_tick_fraction(ticks)` — fraction of ticks with quantity above the mean quantity.
- `NormalizedTick::net_price_drift(ticks)` — mean signed price change per consecutive pair.
- `NormalizedTick::tick_arrival_entropy(ticks)` — approximate entropy of inter-arrival time distribution (5-bin).

**`ohlcv` module — `OhlcvBar` analytics (round 96)**
- `OhlcvBar::open_to_close_momentum(bars)` — mean signed `(close − open) / open` return per bar.
- `OhlcvBar::volume_dispersion(bars)` — coefficient of variation of bar volumes.
- `OhlcvBar::shadow_dominance(bars)` — mean fraction of each bar's range occupied by wicks vs body.
- `OhlcvBar::true_range_mean(bars)` — mean true range across consecutive bar pairs.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 96)**
- `window_std_dev() -> Option<f64>` — population standard deviation of the rolling window.
- `window_min_max_ratio() -> Option<Decimal>` — ratio of window minimum to window maximum.
- `recent_bias() -> Option<f64>` — mean of the second half minus mean of the first half, as fraction of overall mean.
- `window_range_pct() -> Option<f64>` — `(max − min) / min` of the rolling window.

---

## [2.4.6] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 95)**
- `NormalizedTick::buy_volume_fraction(ticks)` — fraction of total sided volume that is buy-side.
- `NormalizedTick::tick_qty_skewness(ticks)` — skewness (third standardized moment) of the quantity distribution.
- `NormalizedTick::above_median_price_fraction(ticks)` — fraction of ticks with price strictly above the median.
- `NormalizedTick::cumulative_qty_imbalance(ticks)` — net buy-minus-sell quantity as a fraction of total sided quantity.

**`ohlcv` module — `OhlcvBar` analytics (round 95)**
- `OhlcvBar::up_down_volume_ratio(bars)` — ratio of total up-bar volume to total down-bar volume.
- `OhlcvBar::longest_bearish_streak(bars)` — length of the longest consecutive run of down-close bars.
- `OhlcvBar::mean_close_to_high_ratio(bars)` — mean `(close − low) / (high − low)` per bar, excluding zero-range bars.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 95)**
- `window_trimmed_mean() -> Option<Decimal>` — mean of values between the 25th and 75th percentile.
- `window_variance() -> Option<Decimal>` — population variance of the rolling window values.

---

## [2.4.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 94)**
- `NormalizedTick::price_efficiency_ratio(ticks)` — net price displacement divided by total path length (0.0–1.0).
- `NormalizedTick::min_inter_tick_gap_ms(ticks)` — minimum gap between consecutive `received_at_ms` timestamps.
- `NormalizedTick::max_inter_tick_gap_ms(ticks)` — maximum gap between consecutive `received_at_ms` timestamps.
- `NormalizedTick::trade_count_imbalance(ticks)` — signed imbalance `(buys − sells) / total` for ticks with side info.

**`ohlcv` module — `OhlcvBar` analytics (round 94)**
- `OhlcvBar::max_gap_up(bars)` — largest upward open-to-prev-close gap as a fraction of prior close.
- `OhlcvBar::price_range_expansion(bars)` — ratio of the last bar's high−low range to the first bar's.
- `OhlcvBar::avg_volume_per_range(bars)` — mean `volume / (high − low)` per bar, excluding zero-range bars.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 94)**
- `window_mean_deviation() -> Option<Decimal>` — mean absolute deviation of window values from their mean.
- `latest_percentile() -> Option<f64>` — fraction of window values strictly below the latest observation.

---

## [2.4.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 93)**
- `NormalizedTick::buy_side_vwap(ticks)` — VWAP computed only over buy-side ticks.
- `NormalizedTick::sell_side_vwap(ticks)` — VWAP computed only over sell-side ticks.
- `NormalizedTick::inter_tick_gap_cv(ticks)` — coefficient of variation of inter-tick arrival intervals.
- `NormalizedTick::signed_tick_count(ticks)` — net count of up-ticks minus down-ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 93)**
- `OhlcvBar::avg_wick_to_body_ratio(bars)` — mean `(upper+lower shadow) / |body|` for non-doji bars.
- `OhlcvBar::close_above_open_streak(bars)` — length of the longest consecutive run of up-close bars.
- `OhlcvBar::volume_above_mean_fraction(bars)` — fraction of bars with above-average volume.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 93)**
- `window_sum_of_squares() -> Decimal` — sum of squared values in the rolling window.
- `percentile_75() -> Option<Decimal>` — 75th percentile of the rolling window.

---

## [2.4.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 92)**
- `NormalizedTick::buy_pressure_ratio(ticks)` — fraction of sided volume that is buy-side.
- `NormalizedTick::sell_pressure_ratio(ticks)` — complement of `buy_pressure_ratio`; fraction of sell-side volume.
- `NormalizedTick::trade_interval_ratio(ticks)` — ratio of first-half mean inter-tick interval to second-half.
- `NormalizedTick::weighted_price_change(ticks)` — quantity-weighted mean price change from the first tick.
- `NormalizedTick::first_last_price_ratio(ticks)` — ratio of last price to first price in the slice.
- `NormalizedTick::tick_price_variance(ticks)` — population variance of tick prices.

**`ohlcv` module — `OhlcvBar` analytics (round 92)**
- `OhlcvBar::open_gap_ratio(bars)` — mean `|open[i] − close[i-1]| / close[i-1]` across consecutive bar pairs.
- `OhlcvBar::candle_symmetry_score(bars)` — mean `1 − |body| / range`; close to 1 = doji-like bars.
- `OhlcvBar::mean_upper_shadow_pct(bars)` — mean `(high − close) / (high − low)` across valid bars.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 92)**
- `monotone_fraction() -> Option<f64>` — fraction of consecutive window pairs that are non-decreasing.
- `coeff_variation() -> Option<f64>` — coefficient of variation: `std_dev / |mean|`.

---

## [2.4.2] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 91)**
- `NormalizedTick::above_mean_qty_fraction(ticks)` — fraction of ticks where quantity exceeds the mean quantity.
- `NormalizedTick::side_alternation_rate(ticks)` — fraction of consecutive tick pairs where the trade side flips.
- `NormalizedTick::price_range_per_tick(ticks)` — `(max_price − min_price) / tick_count`; range per tick.
- `NormalizedTick::qty_weighted_price_std(ticks)` — quantity-weighted standard deviation of trade prices.

**`ohlcv` module — `OhlcvBar` analytics (round 91)**
- `OhlcvBar::gap_up_count(bars)` — count of bars whose open is strictly above the previous bar's close.
- `OhlcvBar::gap_down_count(bars)` — count of bars whose open is strictly below the previous bar's close.
- `OhlcvBar::mean_bar_efficiency(bars)` — mean of `|close − open| / (high − low)` (body-to-range efficiency).

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 91)**
- `window_iqr() -> Option<Decimal>` — interquartile range `Q3 − Q1` of the rolling window.
- `run_length_mean() -> Option<f64>` — mean length of monotone non-decreasing runs within the window.

---

## [2.4.1] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 89)**
- `NormalizedTick::max_drawdown(ticks)` — maximum peak-to-trough price decline across the slice.
- `NormalizedTick::high_to_low_ratio(ticks)` — ratio of the highest price to the lowest price in the slice.
- `NormalizedTick::tick_velocity(ticks)` — total price movement divided by elapsed time (ms).
- `NormalizedTick::notional_decay(ticks)` — ratio of second-half notional to first-half notional; < 1 means decaying activity.
- `NormalizedTick::late_price_momentum(ticks)` — mean price change in the last quarter of the slice minus the first quarter.
- `NormalizedTick::consecutive_buys_max(ticks)` — length of the longest uninterrupted run of buy-side ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 89)**
- `OhlcvBar::close_range_fraction(bars)` — mean of `(close - low) / (high - low)` across bars (CLV mean).
- `OhlcvBar::tail_symmetry(bars)` — mean absolute difference between upper and lower shadow as fraction of range.
- `OhlcvBar::bar_trend_strength(bars)` — fraction of consecutive bar pairs that continue in the same direction.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 89)**
- `cumulative_sum() -> Decimal` — sum of all values in the rolling window.
- `max_to_min_ratio() -> Option<f64>` — window maximum divided by window minimum.

---

## [2.4.0] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 88)**
- `NormalizedTick::order_flow_imbalance(ticks)` — net OFI: `(buy_qty − sell_qty) / total_qty`; range `[−1, 1]`.
- `NormalizedTick::price_qty_up_fraction(ticks)` — fraction of tick pairs where both price and quantity increased.
- `NormalizedTick::running_high_count(ticks)` — count of ticks that set a new running high within the slice.
- `NormalizedTick::running_low_count(ticks)` — count of ticks that set a new running low within the slice.
- `NormalizedTick::buy_sell_avg_qty_ratio(ticks)` — mean buy quantity / mean sell quantity ratio.
- `NormalizedTick::max_price_drop(ticks)` — largest consecutive price decline.
- `NormalizedTick::max_price_rise(ticks)` — largest consecutive price increase.
- `NormalizedTick::buy_trade_count(ticks)` — count of buy-side trades in the slice.
- `NormalizedTick::sell_trade_count(ticks)` — count of sell-side trades in the slice.
- `NormalizedTick::price_reversal_fraction(ticks)` — fraction of 3-tick windows that reverse direction.

**`ohlcv` module — `OhlcvBar` analytics (round 88)**
- `OhlcvBar::avg_range_pct_of_open(bars)` — mean of `range / open` across bars.
- `OhlcvBar::high_volume_fraction(bars)` — fraction of bars with above-average volume.
- `OhlcvBar::close_cluster_count(bars)` — count of consecutive bar pairs where closes are within 0.1% of each other.
- `OhlcvBar::mean_vwap(bars)` — mean of bar VWAP values.
- `OhlcvBar::complete_fraction(bars)` — fraction of bars where all OHLCV fields are nonzero.
- `OhlcvBar::total_body_movement(bars)` — sum of `|close − open|` across all bars.
- `OhlcvBar::open_std(bars)` — sample standard deviation of open prices.
- `OhlcvBar::mean_high_low_ratio(bars)` — mean of `high / low` ratio; always ≥ 1.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 88)**
- `new_max_count() -> usize` — number of times the window sets a new running maximum.
- `new_min_count() -> usize` — number of times the window sets a new running minimum.
- `zero_fraction() -> Option<f64>` — fraction of window values equal to zero.

---

## [2.3.9] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 87)**
- `NormalizedTick::vwap_deviation_std(ticks)` — std dev of how dispersed individual trade prices are around VWAP.
- `NormalizedTick::max_consecutive_side_run(ticks)` — length of the longest run of same-side trades.
- `NormalizedTick::inter_arrival_cv(ticks)` — coefficient of variation of inter-arrival times; measures trade burstiness.
- `NormalizedTick::volume_per_ms(ticks)` — total traded quantity per millisecond of time span.
- `NormalizedTick::notional_per_second(ticks)` — total notional (`price × quantity`) per second.

**`ohlcv` module — `OhlcvBar` analytics (round 87)**
- `OhlcvBar::avg_open_to_close(bars)` — mean of `close − open` across bars; positive = net bullish drift.
- `OhlcvBar::max_bar_volume(bars)` — maximum volume across bars.
- `OhlcvBar::min_bar_volume(bars)` — minimum volume across bars.
- `OhlcvBar::body_to_range_std(bars)` — std dev of body-to-range ratios; measures consistency of body size.
- `OhlcvBar::avg_wick_symmetry(bars)` — mean ratio of smaller to larger wick; near 1 = balanced wicks.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 87)**
- `below_mean_fraction() -> Option<f64>` — fraction of window values strictly below the mean.
- `tail_variance() -> Option<f64>` — variance of values outside the interquartile range.

---

## [2.3.8] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 86)**
- `NormalizedTick::price_mean(ticks)` — arithmetic mean of prices across the slice.
- `NormalizedTick::uptick_count(ticks)` — count of consecutive price increases.
- `NormalizedTick::downtick_count(ticks)` — count of consecutive price decreases.
- `NormalizedTick::uptick_fraction(ticks)` — fraction of tick intervals that are upticks.
- `NormalizedTick::quantity_std(ticks)` — sample std dev of quantities; requires ≥ 2 ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 86)**
- `OhlcvBar::mean_open(bars)` — arithmetic mean of open prices across bars.
- `OhlcvBar::new_high_count(bars)` — count of bars that set a new cumulative high.
- `OhlcvBar::new_low_count(bars)` — count of bars that set a new cumulative low.
- `OhlcvBar::close_std(bars)` — sample std dev of close prices; requires ≥ 2 bars.
- `OhlcvBar::zero_volume_fraction(bars)` — fraction of bars with zero volume.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 86)**
- `distinct_count() -> usize` — number of distinct values in the window.
- `max_fraction() -> Option<f64>` — fraction of values equal to the window maximum.
- `min_fraction() -> Option<f64>` — fraction of values equal to the window minimum.
- `latest_minus_mean() -> Option<f64>` — signed difference between the latest value and the mean.
- `latest_to_mean_ratio() -> Option<f64>` — ratio of the latest value to the mean.

---

## [2.3.7] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 85)**
- `NormalizedTick::neutral_count(ticks)` — count of ticks with no aggressor side (`side == None`).
- `NormalizedTick::price_dispersion(ticks)` — `max_price − min_price`; raw price spread across the slice.
- `NormalizedTick::max_notional(ticks)` — maximum per-tick notional (`price × quantity`) in the slice.
- `NormalizedTick::min_notional(ticks)` — minimum per-tick notional in the slice.
- `NormalizedTick::below_vwap_fraction(ticks)` — fraction of ticks with price below the slice VWAP.
- `NormalizedTick::trade_notional_std(ticks)` — std dev of per-tick `price × quantity`; requires ≥ 2 ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 85)**
- `OhlcvBar::total_range(bars)` — sum of `high − low` across all bars; total accumulated range.
- `OhlcvBar::close_at_high_fraction(bars)` — fraction of bars where close equals the high.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 85)**
- `interquartile_mean() -> Option<f64>` — mean of values strictly between Q1 and Q3.
- `outlier_fraction(threshold) -> Option<f64>` — fraction of window values beyond `threshold` std devs from the mean.

---

## [2.3.6] - 2026-03-21

### Added

**`tick` module — `NormalizedTick` analytics (round 84)**
- `NormalizedTick::sell_notional_fraction(ticks)` — fraction of total notional that is sell-side; complement of `buy_notional_fraction`.
- `NormalizedTick::max_price_gap(ticks)` — maximum absolute price jump between consecutive ticks.
- `NormalizedTick::price_range_velocity(ticks)` — `(high − low) / time_span_ms`; rate of price range expansion.
- `NormalizedTick::tick_count_per_ms(ticks)` — ticks per millisecond over the slice time span.

**`ohlcv` module — `OhlcvBar` analytics (round 84)**
- `OhlcvBar::avg_lower_shadow_ratio(bars)` — mean of `lower_shadow / range` per bar; excludes doji bars.
- `OhlcvBar::close_to_open_range_ratio(bars)` — mean of `(close − open) / range` per bar; signed body position.
- `OhlcvBar::max_high(bars)` — maximum high price across all bars.
- `OhlcvBar::min_low(bars)` — minimum low price across all bars.
- `OhlcvBar::avg_bar_efficiency(bars)` — mean `|close − open| / range` across non-doji bars.
- `OhlcvBar::open_range_fraction(bars)` — fraction of bars where `open` is in the upper half of `[low, high]`.

**`tick` module — `NormalizedTick` analytics (round 84, continued)**
- `NormalizedTick::buy_quantity_fraction(ticks)` — fraction of total quantity attributable to buy-side trades.
- `NormalizedTick::sell_quantity_fraction(ticks)` — fraction of total quantity attributable to sell-side trades.
- `NormalizedTick::price_mean_crossover_count(ticks)` — count of times price crosses through its window mean.
- `NormalizedTick::notional_skewness(ticks)` — skewness of per-tick notional (`price × quantity`) values.
- `NormalizedTick::volume_weighted_mid_price(ticks)` — volume-weighted midpoint of the price range (VWAP).

**`ohlcv` module — `OhlcvBar` analytics (round 84, continued)**
- `OhlcvBar::close_skewness(bars)` — skewness of close prices across bars.
- `OhlcvBar::volume_above_median_fraction(bars)` — fraction of bars with volume exceeding the median bar volume.
- `OhlcvBar::typical_price_sum(bars)` — sum of `(high + low + close) / 3` across bars.
- `OhlcvBar::max_body_size(bars)` — maximum `|close − open|` across all bars.
- `OhlcvBar::min_body_size(bars)` — minimum `|close − open|` across all bars.
- `OhlcvBar::avg_lower_wick_to_range(bars)` — mean ratio of lower wick to full bar range.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 84)**
- `exponential_weighted_mean(alpha) -> Option<f64>` — EWM with decay `alpha`; most-recent value has highest weight.
- `peak_to_trough_ratio() -> Option<f64>` — ratio of window maximum to minimum; requires non-zero minimum.
- `second_moment() -> Option<f64>` — mean of squared window values (second raw moment).
- `range_over_mean() -> Option<f64>` — coefficient of dispersion: `(max − min) / mean`.
- `above_median_fraction() -> Option<f64>` — fraction of window values strictly above the window median.

---

## [2.3.5] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 83)**
- `NormalizedTick::quantity_autocorrelation(ticks)` — lag-1 autocorrelation of trade sizes; > 0 means large trades cluster together.
- `NormalizedTick::fraction_above_vwap(ticks)` — fraction of ticks priced strictly above the VWAP.
- `NormalizedTick::max_buy_streak(ticks)` — longest consecutive run of buy-side ticks.
- `NormalizedTick::max_sell_streak(ticks)` — longest consecutive run of sell-side ticks.
- `NormalizedTick::side_entropy(ticks)` — entropy of the buy/sell/neutral distribution; higher = more mixed flow.
- `NormalizedTick::mean_inter_tick_gap_ms(ticks)` — mean time gap between consecutive ticks in milliseconds.
- `NormalizedTick::round_number_fraction(ticks, step)` — fraction of ticks whose price is divisible by `step`.
- `NormalizedTick::geometric_mean_quantity(ticks)` — geometric mean of trade quantities.
- `NormalizedTick::max_tick_return(ticks)` — best single tick-to-tick percentage gain.
- `NormalizedTick::min_tick_return(ticks)` — worst single tick-to-tick percentage drop.

**`ohlcv` module — `OhlcvBar` analytics (round 83)**
- `OhlcvBar::close_above_median_fraction(bars)` — fraction of bars where close > `(high + low) / 2`.
- `OhlcvBar::avg_range_to_open(bars)` — mean of `(high − low) / open`; intrabar range relative to open.
- `OhlcvBar::close_sum(bars)` — sum of all close prices across the slice.
- `OhlcvBar::above_avg_volume_count(bars)` — count of bars with volume above the slice average.
- `OhlcvBar::median_close(bars)` — median close price across the slice.
- `OhlcvBar::flat_bar_fraction(bars)` — fraction of bars where open == close (doji-like).
- `OhlcvBar::avg_body_to_range(bars)` — mean of `body / range` per bar.
- `OhlcvBar::max_open_gap(bars)` — largest single-bar open vs. previous-close gap.
- `OhlcvBar::volume_trend_slope(bars)` — OLS slope of bar volume over bar index.
- `OhlcvBar::up_close_fraction(bars)` — fraction of bars where close > previous close.
- `OhlcvBar::avg_upper_shadow_ratio(bars)` — mean of `upper_shadow / range` per bar.

**`norm` module — `MinMaxNormalizer` analytics (round 83)**
- `monotone_increase_fraction() -> Option<f64>` — fraction of consecutive window pairs that are increasing.
- `abs_max() -> Option<Decimal>` — maximum absolute value in the window.
- `abs_min() -> Option<Decimal>` — minimum absolute value in the window.
- `max_count() -> Option<usize>` — count of window values equal to the maximum.
- `min_count() -> Option<usize>` — count of window values equal to the minimum.
- `mean_ratio() -> Option<f64>` — ratio of the current window mean to the mean of the first half.

---

## [2.3.4] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 82)**
- `NormalizedTick::price_momentum_score(ticks)` — quantity-weighted mean of signed price changes; positive = net upward momentum.
- `NormalizedTick::vwap_std(ticks)` — std dev of prices weighted by quantity (dispersion around VWAP).
- `NormalizedTick::price_range_expansion(ticks)` — fraction of ticks that set a new running high or low.
- `NormalizedTick::sell_to_total_volume_ratio(ticks)` — fraction of total volume classified as sell-side.
- `NormalizedTick::notional_std(ticks)` — std dev of per-tick notional (`price × quantity`); requires ≥ 2 ticks.

**`ohlcv` module — `OhlcvBar` analytics (round 82)**
- `OhlcvBar::avg_bar_range(bars)` — mean of `high − low` across bars.
- `OhlcvBar::max_up_move(bars)` — largest single-bar upward body (`max(close − open, 0)`).
- `OhlcvBar::max_down_move(bars)` — largest single-bar downward body (`max(open − close, 0)`).
- `OhlcvBar::avg_close_position(bars)` — mean of `(close − low) / range` for bars with non-zero range.
- `OhlcvBar::volume_std(bars)` — std dev of volume across bars; requires ≥ 2 bars.
- `OhlcvBar::avg_wick_ratio(bars)` — mean of `total_wick / range` per bar; excludes doji bars.
- `OhlcvBar::open_gap_mean(bars)` — mean of `|open_i − close_{i-1}| / close_{i-1}`; measures gap size.
- `OhlcvBar::net_directional_move(bars)` — `(last_close − first_open) / first_open`; overall percentage move.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 82)**
- `mean_absolute_change() -> Option<f64>` — mean of `|x_i − x_{i-1}|` across consecutive window values; average absolute step size.

### Fixed
- Removed duplicate `quantity_skewness`, `price_acceleration` (tick), `autocorrelation_lag1` (norm) and `wick_ratio` (ohlcv) definitions that were introduced by parallel agent runs and caused E0592 compile errors.

---

## [2.3.3] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 81)**
- `NormalizedTick::cumulative_volume(ticks)` — running total of quantity at each tick index; returns a `Vec<Decimal>` of prefix sums.
- `NormalizedTick::price_volatility_ratio(ticks)` — `std_dev(prices) / mean(prices)`; coefficient of variation for price; requires ≥ 2 ticks.
- `NormalizedTick::notional_per_tick(ticks)` — mean of `price × quantity` per tick; average dollar value of a single trade.
- `NormalizedTick::buy_to_total_volume_ratio(ticks)` — `buy_volume / total_volume`; fraction of volume classified as buy-side.
- `NormalizedTick::avg_latency_ms(ticks)` — mean of `exchange_ts_ms − received_at_ms` for ticks that have both timestamps.
- `NormalizedTick::price_gini(ticks)` — Gini coefficient of trade prices; 0 = all identical, 1 = maximally unequal.
- `NormalizedTick::trade_velocity(ticks)` — trades per millisecond over the slice; `count / (last_ts − first_ts)`.
- `NormalizedTick::floor_price(ticks)` — minimum price seen across the slice (alias for price floor support level).

**`ohlcv` module — `OhlcvBar` analytics (round 81)**
- `OhlcvBar::close_to_high_std(bars)` — std dev of `(close − low) / range` ratio across bars; requires ≥ 2 bars.
- `OhlcvBar::avg_open_volume_ratio(bars)` — mean of `open_price / volume` per bar (price-per-unit-volume at open).
- `OhlcvBar::typical_price_std(bars)` — std dev of `(high + low + close) / 3` across bars; requires ≥ 2 bars.
- `OhlcvBar::vwap_deviation_avg(bars)` — mean absolute deviation of each bar's close from its VWAP (when set).
- `OhlcvBar::avg_high_low_ratio(bars)` — mean of `high / low` per bar; > 1.0 always; larger = wider intrabar range.
- `OhlcvBar::gap_fill_fraction(bars)` — fraction of bars (from the second onward) where the bar fills the gap from the previous close.
- `OhlcvBar::complete_bar_count(bars)` — count of bars where `is_complete == true`.
- `OhlcvBar::min_trade_count(bars)` — minimum `trade_count` seen across the slice.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 81)**
- `variance_ratio() -> Option<f64>` — ratio of the variance of the first half of the window to the second half; > 1.0 = decreasing volatility.
- `z_score_trend_slope() -> Option<f64>` — OLS slope of z-scored window values; detects directional drift in standardised space.

---

## [2.3.2] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 80)**
- `NormalizedTick::net_notional(ticks)` — `buy_notional − sell_notional`; positive = net dollar buying pressure.
- `NormalizedTick::price_reversal_count(ticks)` — count of price direction reversals (up→down or down→up) across the slice.
- `NormalizedTick::quantity_kurtosis(ticks)` — excess kurtosis of trade quantities; requires ≥ 4 ticks.
- `NormalizedTick::largest_notional_trade(ticks)` — reference to the tick with the highest `price × quantity`; ranks by dollar value rather than raw size.
- `NormalizedTick::twap(ticks)` — time-weighted average price using `received_at_ms` intervals as weights.
- `NormalizedTick::neutral_fraction(ticks)` — fraction of ticks with `side == None`; complement of `aggressor_fraction`.
- `NormalizedTick::log_return_variance(ticks)` — variance of tick-to-tick log returns; requires ≥ 3 ticks.
- `NormalizedTick::volume_at_vwap(ticks, tolerance)` — total quantity traded within `tolerance` of the VWAP.

**`ohlcv` module — `OhlcvBar` analytics (round 80)**
- `OhlcvBar::close_recovery_ratio(bars)` — mean of `(close − low) / range`; near 1.0 = closes consistently near the high.
- `OhlcvBar::median_range(bars)` — median of `high − low`; robust to outlier bars.
- `OhlcvBar::mean_typical_price(bars)` — mean of `(high + low + close) / 3` across bars.
- `OhlcvBar::directional_volume_ratio(bars)` — bullish volume / (bullish + bearish volume).
- `OhlcvBar::inside_bar_fraction(bars)` — fraction of bars (from the second onward) that are inside bars.
- `OhlcvBar::body_momentum(bars)` — net sum of signed body sizes `Σ(close − open)`; positive = net bullish drift.
- `OhlcvBar::avg_trade_count(bars)` — mean `trade_count` (ticks per bar) across the slice.
- `OhlcvBar::max_trade_count(bars)` — maximum `trade_count` seen across the slice.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 80)**
- `trimmed_mean(p) -> Option<f64>` — arithmetic mean after discarding the bottom and top `p` fraction of values; `p` clamped to `[0.0, 0.499]`.
- `linear_trend_slope() -> Option<f64>` — OLS slope of window values over insertion index; positive = upward trend.

### Fixed
- `trimmed_mean` tests: corrected trim fraction from `0.1` to `0.2` so that at least one element is removed per side with a 5-element window (floor(5 × 0.1) = 0 removes nothing).
- `OhlcvBar::mean_typical_price` test: fixed borrow-after-move by pre-computing expected value before passing the bar into the slice.

---

## [2.3.1] - 2026-03-20

### Added

**`tick` module — `NormalizedTick` analytics (round 79)**
- `NormalizedTick::aggressor_fraction(ticks)` — fraction of ticks with a known trade side; near 1.0 means the feed reliably reports aggressor direction.
- `NormalizedTick::volume_imbalance_ratio(ticks)` — `(buy_vol − sell_vol) / (buy_vol + sell_vol)`; signed `(−1, +1)` measure of net buying/selling pressure.
- `NormalizedTick::price_quantity_covariance(ticks)` — sample covariance between price and quantity; positive means larger trades cluster at higher prices.
- `NormalizedTick::large_trade_fraction(ticks, threshold)` — fraction of ticks whose quantity ≥ `threshold`; characterises institutional flow density.
- `NormalizedTick::price_level_density(ticks)` — unique price levels per unit of price range; high density = granular action, low = discrete jumps.
- `NormalizedTick::notional_buy_sell_ratio(ticks)` — buy notional / sell notional; `> 1.0` means buy-side dollar flow dominates.
- `NormalizedTick::log_return_mean(ticks)` — mean of tick-to-tick log returns `ln(p_i / p_{i-1})`.
- `NormalizedTick::log_return_std(ticks)` — standard deviation of tick-to-tick log returns; requires ≥ 3 ticks.
- `NormalizedTick::price_overshoot_ratio(ticks)` — `max_price / last_price`; > 1.0 signals the price overshot its closing level.
- `NormalizedTick::price_undershoot_ratio(ticks)` — `first_price / min_price`; > 1.0 signals the price undershot its opening level.

**`ohlcv` module — `OhlcvBar` analytics (round 79)**
- `OhlcvBar::close_to_range_position(bars)` — mean of `(close − low) / range`; near 1.0 = consistently closing near the high (bullish).
- `OhlcvBar::volume_oscillator(bars, short_n, long_n)` — `(short_avg_vol − long_avg_vol) / long_avg_vol`; positive = expanding volume.
- `OhlcvBar::direction_reversal_count(bars)` — count of consecutive bar-direction flips (bullish ↔ bearish).
- `OhlcvBar::upper_wick_dominance_fraction(bars)` — fraction of bars where upper wick > lower wick.
- `OhlcvBar::avg_open_to_high_ratio(bars)` — mean of `(high − open) / range`; how far up from the open the price moved on average.
- `OhlcvBar::volume_weighted_range(bars)` — `Σ(range_i × vol_i) / Σ(vol_i)`; volume-weighted average bar range.
- `OhlcvBar::bar_strength_index(bars)` — mean CLV `(close − low − (high − close)) / range`; +1 = all closes at high, −1 = all at low.
- `OhlcvBar::shadow_to_body_ratio(bars)` — total wick length / total body size; high = wick-dominated price action.
- `OhlcvBar::first_last_close_pct(bars)` — percentage change from first to last close.
- `OhlcvBar::open_to_close_volatility(bars)` — std dev of per-bar `(close − open) / open` returns; intrabar volatility consistency.

**`norm` module — `MinMaxNormalizer` and `ZScoreNormalizer` analytics (round 79)**
- `upper_quartile() -> Option<Decimal>` — Q3 (75th percentile) of the rolling window.
- `lower_quartile() -> Option<Decimal>` — Q1 (25th percentile) of the rolling window.
- `sign_change_rate() -> Option<f64>` — fraction of consecutive first-difference pairs whose sign flips; high = oscillating, low = trending.

### Fixed
- `MinMaxNormalizer::quantile_range()` and `ZScoreNormalizer::quantile_range()` were calling `percentile_value(75.0)` / `percentile_value(25.0)` but that method expects values in `[0.0, 1.0]`. Both arguments were clamped to `1.0`, making the IQR always return `0`. Fixed to use `percentile_value(0.75)` / `percentile_value(0.25)`.

---

## [2.2.0] - 2026-03-20

### Added

**`norm` module — `MinMaxNormalizer` analytics (rounds 42–45)**
- `MinMaxNormalizer::kurtosis() -> Option<f64>` — excess kurtosis of the rolling window; companion to the existing `skewness()`. Returns `None` for fewer than 4 observations or zero std-dev.
- `MinMaxNormalizer::count_below(threshold: Decimal) -> usize` — count of window values strictly below a threshold.
- `MinMaxNormalizer::variance() -> Option<Decimal>` — population variance of the rolling window.
- `MinMaxNormalizer::std_dev() -> Option<f64>` — population standard deviation (sqrt of variance).
- `MinMaxNormalizer::coefficient_of_variation() -> Option<f64>` — CV = std_dev / |mean|; `None` when mean is zero or window has fewer than 2 values.

**`norm` module — `ZScoreNormalizer` analytics (round 43)**
- `ZScoreNormalizer::interquartile_range() -> Option<Decimal>` — IQR (Q3 − Q1) of the rolling window. Returns `None` for fewer than 4 observations.

**`ohlcv` module — `OhlcvBar` static analytics (rounds 42–45)**
- `OhlcvBar::average_true_range(bars: &[OhlcvBar]) -> Option<Decimal>` — ATR: mean of `true_range` across consecutive bars. Returns `None` for fewer than 2 bars.
- `OhlcvBar::average_body(bars: &[OhlcvBar]) -> Option<Decimal>` — mean `|close − open|` across a slice of bars. Companion to `mean_volume`.
- `OhlcvBar::bullish_count(bars: &[OhlcvBar]) -> usize` — count of bullish bars (close > open).
- `OhlcvBar::bearish_count(bars: &[OhlcvBar]) -> usize` — count of bearish bars (close < open).
- `OhlcvBar::win_rate(bars: &[OhlcvBar]) -> Option<f64>` — fraction of bullish bars; `None` for empty slice.
- `OhlcvBar::max_drawdown(bars: &[OhlcvBar]) -> Option<f64>` — maximum peak-to-trough close drawdown. Returns `None` for fewer than 2 bars.
- `OhlcvBar::bullish_streak(bars: &[OhlcvBar]) -> usize` — consecutive bullish bars from the tail of the slice.
- `OhlcvBar::bearish_streak(bars: &[OhlcvBar]) -> usize` — consecutive bearish bars from the tail of the slice.
- `OhlcvBar::linear_regression_slope(bars: &[OhlcvBar]) -> Option<f64>` — OLS slope of close prices over bar index. Returns `None` for fewer than 2 bars.

**`tick` module — `NormalizedTick` analytics (rounds 44–45)**
- `NormalizedTick::buy_volume(ticks: &[NormalizedTick]) -> Decimal` — total quantity for buy-side ticks.
- `NormalizedTick::sell_volume(ticks: &[NormalizedTick]) -> Decimal` — total quantity for sell-side ticks.
- `NormalizedTick::price_range(ticks: &[NormalizedTick]) -> Option<Decimal>` — max price minus min price across a slice; `None` for empty slice.
- `NormalizedTick::average_price(ticks: &[NormalizedTick]) -> Option<Decimal>` — mean price across a slice; `None` for empty slice.

**`ring` module — `SpscProducer` / `SpscConsumer` (round 48)**
- `SpscProducer::fill_ratio()` and `SpscConsumer::fill_ratio()` now delegate to the inner ring's canonical `fill_ratio()`.
- `SpscProducer::available()` now delegates to `inner.remaining_capacity()`.

### Fixed
- `NormalizedTick::is_large_tick` now correctly uses strict `>` comparison (docs always said "strictly above"); it was erroneously delegating to `is_large_trade` which uses `>=`.
- `ZScoreNormalizer::is_near_mean` now explicitly returns `false` when the window has fewer than 2 observations, preventing a false-positive when std-dev is 0.
- `ZScoreNormalizer::trim_outliers`: removed redundant `.to_f64()` chain on an already-`f64` result from `std_dev()`.
- `OhlcvAggregator::feed`: replaced inline `tick.price * tick.quantity` with `tick.value()` to use the canonical method.
- Multiple methods across `ohlcv` and `norm` modules now delegate to canonical helpers (`self.range()`, `self.mean()`, `self.std_dev()`) instead of recomputing inline, eliminating silent divergence risk.

### Deprecated

**`tick` module — `NormalizedTick` alias cleanup (round 45)**

| Deprecated | Use instead |
|---|---|
| `is_above_price(p)` | `price > p` |
| `is_below_price(p)` | `price < p` |
| `is_at_price(p)` | `price == p` |
| `is_buy_side()` | `side == Some(TradeSide::Buy)` |
| `is_sell_side()` | `side == Some(TradeSide::Sell)` |
| `is_recent(ts, window)` | `age_ms(ts) <= window` |
| `notional_value()` | `value()` |
| `quote_age_ms(ts)` | `age_ms(ts)` |
| `is_high_value_tick(t)` | `is_high_value(t)` |
| `is_large_tick(t)` | `is_large_trade(t)` |
| `price_diff_from(other)` | `price_move_from(other)` |

**`session` module — alias cleanup (round 45)**

| Deprecated | Use instead |
|---|---|
| `is_pre_open(ts)` | `is_pre_market(ts)` |
| `session_progress_pct(ts)` | `progress_pct(ts)` |

**`lorentz` module — alias cleanup (round 45)**

| Deprecated | Use instead |
|---|---|
| `is_ultra_relativistic()` | `is_ultrarelativistic()` |
| `space_contraction()` | `length_contraction()` |

**`ohlcv` module — `OhlcvBar` alias cleanup (round 42)**

The following methods are exact duplicates of existing methods and are deprecated with `#[deprecated(since = "2.2.0")]`. They continue to compile and delegate to their canonical counterpart; they will be removed in a future major release.

| Deprecated | Use instead |
|---|---|
| `inside_bar(prev)` | `is_inside_bar(prev)` |
| `is_outside_bar(prev)` | `outside_bar(prev)` |
| `close_gap(prev)` | `gap_from(prev)` |
| `bar_range()` | `range()` |
| `true_range_with_prev(prev_close)` | `true_range(prev_close)` |
| `close_to_high_ratio()` | `high_close_ratio()` |
| `close_open_ratio()` | `open_close_ratio()` |
| `price_change_abs()` | `body()` |
| `body_size()` | `body()` |

---

## [2.1.0] - 2026-03-20

### Added

**`ohlcv` module — `OhlcvBar` analytics (rounds 35–41)**
- `mean_volume(bars: &[OhlcvBar]) -> Option<Decimal>` — static helper; average volume across a slice
- `vwap_deviation() -> Option<f64>` — deviation of close from VWAP
- `relative_volume(avg_volume: Decimal) -> Option<f64>` — current volume / average volume
- `intraday_reversal(prev: &OhlcvBar) -> bool` — detects close reversals against prior bar direction
- `high_close_ratio() -> Option<f64>` — (high - close) / high
- `lower_shadow_pct() -> Option<f64>` — lower shadow as fraction of total range
- `open_close_ratio() -> Option<f64>` — open / close
- `is_wide_range_bar(threshold: Decimal) -> bool` — true when high - low exceeds threshold
- `close_to_low_ratio() -> Option<f64>` — (close - low) / (high - low)
- `volume_per_trade() -> Option<Decimal>` — volume / trade_count
- `price_range_overlap(other: &OhlcvBar) -> bool` — true when two bars share a price range
- `bar_height_pct() -> Option<f64>` — (high - low) / open
- `is_bullish_engulfing(prev: &OhlcvBar) -> bool` — classic two-bar bullish engulfing pattern
- `close_gap(prev: &OhlcvBar) -> Decimal` — open - prev.close (gap between bars)
- `close_above_midpoint() -> bool` — close > (high + low) / 2
- `close_momentum(prev: &OhlcvBar) -> Decimal` — close - prev.close
- `bar_range() -> Decimal` — high - low

**`tick` module — `NormalizedTick` query methods (rounds 36–40)**
- `is_above_price(reference: Decimal) -> bool`
- `is_below_price(reference: Decimal) -> bool`
- `is_at_price(target: Decimal) -> bool`
- `price_change_from(reference: Decimal) -> Decimal`
- `quantity_above(threshold: Decimal) -> bool`
- `is_round_number(step: Decimal) -> bool`
- `is_market_open_tick(session_start_ms: u64, session_end_ms: u64) -> bool`
- `signed_quantity() -> Decimal` — positive for Buy, negative for Sell, zero for Unknown
- `as_price_level() -> (Decimal, Decimal)` — (price, quantity) tuple

**`book` module — `OrderBook` extended analytics (rounds 36–40)**
- `total_value_at_level(side, price) -> Option<Decimal>` — price × quantity at a level
- `ask_volume_above(price: Decimal) -> Decimal` — cumulative ask qty strictly above price
- `bid_volume_below(price: Decimal) -> Decimal` — cumulative bid qty strictly below price
- `total_notional_both_sides() -> Decimal` — sum of price × qty across all levels
- `price_level_exists(side, price) -> bool`
- `level_count_both_sides() -> usize`
- `ask_price_at_rank(n: usize) -> Option<Decimal>` — nth best ask (0 = best)
- `bid_price_at_rank(n: usize) -> Option<Decimal>` — nth best bid (0 = best)

**`norm` module — rolling normalizer analytics (rounds 35–41)**
- `MinMaxNormalizer::count_above(threshold: f64) -> usize`
- `MinMaxNormalizer::normalized_range(&mut self) -> Option<f64>` — (max - min) / max
- `MinMaxNormalizer::fraction_above_mid(&mut self) -> Option<f64>`
- `ZScoreNormalizer::rolling_mean_change() -> Option<f64>` — second_half_mean − first_half_mean
- `ZScoreNormalizer::count_positive_z_scores() -> usize`
- `ZScoreNormalizer::above_threshold_count(z_threshold: f64) -> usize`
- `ZScoreNormalizer::window_span_f64() -> Option<f64>` — max − min over the window
- `ZScoreNormalizer::is_mean_stable(threshold: f64) -> bool`

**`session` module — `SessionAwareness` calendar helpers (rounds 35–40)**
- `is_fomc_blackout_window(date: NaiveDate) -> bool`
- `is_market_holiday_adjacent(date: NaiveDate) -> bool`
- `seconds_until_open(utc_ms: u64) -> f64`
- `is_closing_bell_minute(utc_ms: u64) -> bool`
- `day_of_week_name(date: NaiveDate) -> &'static str`
- `is_expiry_week(date: NaiveDate) -> bool`
- `session_name() -> &'static str`

**`health` module — `HealthMonitor` batch/query helpers (rounds 35–41)**
- `register_batch(feeds: &[(impl AsRef<str>, u64)])` — register multiple feeds with custom thresholds
- `unknown_feed_ids() -> Vec<String>` — feeds registered but never heartbeated
- `feeds_needing_check() -> Vec<String>` — sorted list of non-Healthy feed IDs
- `ratio_healthy() -> f64` — healthy / total
- `total_tick_count() -> u64`
- `last_updated_feed_id() -> Option<String>`
- `is_any_stale() -> bool`

**`ring` module — `SpscRing` analytics (rounds 35–41)**
- `sum_cloned() -> T` where `T: Clone + Sum + Default`
- `average_cloned() -> Option<f64>` where `T: Clone + Into<f64>`
- `peek_nth(n: usize) -> Option<T>` where `T: Clone` — 0 = oldest
- `contains_cloned(value: &T) -> bool` where `T: Clone + PartialEq`
- `max_cloned_by<F, K>(key: F) -> Option<T>`
- `min_cloned_by<F, K>(key: F) -> Option<T>`
- `to_vec_sorted() -> Vec<T>` where `T: Clone + Ord`

**`lorentz` module — `LorentzTransform` invariants (round 41)**
- `beta_times_gamma() -> f64` — β·γ (proper velocity component)
- `energy_momentum_invariant(mass: f64) -> f64` — E² − p² = m² invariant check

### Changed
- Version bumped to `2.1.0`.
- README expanded: all new methods documented in API Reference; ZScoreNormalizer
  quickstart example added; module table updated to mention `ZScoreNormalizer`.

---

## [1.1.0] - 2026-03-18

### Added
- `[profile.release]`: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`,
  `strip = "debuginfo"`, `panic = "abort"` for maximum release performance.
- `[profile.bench]`: dedicated bench profile with `lto = "thin"`.
- `[lints.clippy]`: `pedantic = "warn"` group enabled; common false-positive
  pedantic lints explicitly `allow`ed (`module_name_repetitions`, `must_use_candidate`,
  `missing_errors_doc`, `missing_panics_doc`).
- `readme`, `authors`, and `include` fields added to `Cargo.toml`.
- CI `bench` job: separate job with `--no-run` compile check and `--sample-size 10` run.
- CI `deny` job: `cargo-deny-action` checking licenses, advisories, bans, and sources.
- CI `coverage` job: `cargo-tarpaulin` with Codecov upload.
- CI `test` job: expanded to a matrix of `ubuntu-latest`, `windows-latest`,
  `macos-latest`; adds `PROPTEST_CASES=1000`, `cargo test --release`, and
  `cargo audit` steps.
- `tests/api_coverage_stream.rs`: additional tests covering `HealthMonitor::with_circuit_breaker_threshold`,
  `SessionAwareness::session()`, `LorentzTransform::beta()`/`gamma()`, `SpacetimePoint` fields,
  `MinMaxNormalizer::window_size()`/`is_empty()`/`reset()`, `OhlcvAggregator::with_emit_empty_bars`,
  `ReconnectPolicy` backoff math, `BookDelta::with_sequence`, `TickNormalizer` unknown exchange error.
- Release workflow: `.github/workflows/release.yml` for tag-triggered crates.io publish.

### Changed
- Version bumped to `1.1.0`.
- CI restructured from a single combined job into separate `fmt`, `clippy`, `test`,
  `bench`, `doc`, `deny`, and `coverage` jobs for better parallelism and clearer failure signals.
- Production-readiness pass: doc comments, error handling, CI, tests, and README reviewed.
  All existing tests continue to pass (328 total across unit and integration suites).

---

## [0.2.0] - 2026-03-17

### Added
- `ring` module: lock-free SPSC ring buffer (`SpscRing<T, N>`) with const-generic
  capacity. `push` returns `Err(StreamError::RingBufferFull)` on overflow (never
  panics). `pop` returns `Err(StreamError::RingBufferEmpty)` on underflow.
  `split()` API yields thread-safe `SpscProducer` / `SpscConsumer` halves.
- `norm` module: rolling min-max normalizer (`MinMaxNormalizer`) mapping streaming
  observations into `[0.0, 1.0]` over a configurable sliding window. Reset,
  streaming update, and lazy recompute on eviction.
- `lorentz` module: special-relativistic Lorentz frame-boost (`LorentzTransform`)
  for financial time-series feature engineering. `transform`, `inverse_transform`,
  `transform_batch`, `dilate_time`, `contract_length`. Validates `beta in [0, 1)`.
- `StreamError` variants: `RingBufferFull { capacity }`, `RingBufferEmpty`,
  `AggregationError { reason }`, `NormalizationError { reason }`,
  `InvalidTick { reason }`, `LorentzConfigError { reason }`.
- `lib.rs` re-exports: `SpscRing`, `SpscProducer`, `SpscConsumer`,
  `MinMaxNormalizer`, `LorentzTransform`, `SpacetimePoint`.
- Property-based tests (`tests/property.rs`) using `proptest`: ring FIFO ordering,
  normalization monotonicity, normalization range invariant.
- Extended integration tests (`tests/integration.rs`): ring buffer pipeline,
  tick-to-OHLCV-to-normalized end-to-end, Lorentz transform pipeline, concurrent
  SPSC ring buffer with 50 000 ticks.
- Extended unit tests (`tests/extended.rs`): all new `StreamError` variants,
  OHLCV period boundary, multiple bars in sequence, gap detection, volume
  accumulation, all-fields correctness, Lorentz batch and round-trip tests.
- CI: added `cargo test --release` step for performance-sensitive correctness.
- `proptest = "1.4"` added to `[dev-dependencies]`.
- `documentation` and `homepage` fields in `Cargo.toml`.
- Doc comments (`///`) on every public item including complexity and throughput
  notes on hot-path methods and mathematical basis of Lorentz transforms.

### Changed
- README rewritten: architecture diagram, performance table, Lorentz math notes,
  integration-with-fin-primitives section, updated quickstart, updated module map.
- CI pipeline extended with `cargo fmt --check`, `cargo test --release`, and
  `cargo doc --no-deps` steps.
- `WsManager` and `HealthMonitor` accessors carry individual doc comments.

### Added
- `#![deny(missing_docs)]` in `lib.rs`; all public items now carry `///` doc comments.
- Doc comments on every public struct field across all modules (`book`, `health`,
  `ohlcv`, `tick`, `ws`, `session`, `lorentz`).
- Tests for feed health monitoring: mark feed unhealthy and verify health check behavior.
- Tests for tick normalization edge cases: malformed messages, missing required fields,
  null/non-string field types, all exchanges.
- Tests for order book delta application: out-of-order sequence numbers, duplicate
  sequence numbers, and sequence gap detection.
- Release CI workflow (`.github/workflows/release.yml`) that triggers on `v*` tags,
  builds in release mode, runs full test suite, and publishes a GitHub release with
  the compiled artifacts.

### Changed
- Cargo.toml version bumped from `0.1.0` to `0.2.0`.
- README "Contributing" section expanded with step-by-step guide for adding a new
  exchange adapter.
- README "Supported Exchanges" section added, listing all four adapters and their
  current status.

---

## [0.1.0] - 2026-03-17

### Added

- `tick` module: multi-exchange tick normalization pipeline.
  - `Exchange` enum: Binance, Coinbase, Alpaca, Polygon with `FromStr` / `Display`.
  - `RawTick`: raw WebSocket payload with system-clock `received_at_ms`.
  - `NormalizedTick`: canonical exchange-agnostic representation (price, quantity, side, trade_id, timestamps).
  - `TradeSide`: Buy / Sell.
  - `TickNormalizer`: stateless, `Send + Sync`, zero-cost constructor. Parses Binance, Coinbase, Alpaca, and Polygon wire formats.
- `ohlcv` module: streaming OHLCV bar aggregation.
  - `Timeframe`: Seconds / Minutes / Hours with millisecond duration and bar-start alignment.
  - `OhlcvBar`: open, high, low, close, volume, trade count, completion flag.
  - `OhlcvAggregator`: feed ticks, complete bars on boundary crossings, optional zero-volume gap-fill bars via `with_emit_empty_bars`.
- `book` module: delta-streaming order book for a single symbol.
  - `BookSide`, `PriceLevel`, `BookDelta` (optional sequence number via `with_sequence`).
  - `OrderBook`: `apply`, `reset` (full snapshot), `best_bid`, `best_ask`, `mid_price`, `spread`, `top_bids(n)`, `top_asks(n)`.
  - Crossed-book detection returns `StreamError::BookCrossed` without corrupting state.
- `session` module: trading-session classification.
  - `MarketSession`: UsEquity (NYSE/NASDAQ 9:30-16:00 ET), Crypto (24/7), Forex (24/5).
  - `TradingStatus`: Open, Extended, Closed.
  - `SessionAwareness::status(utc_ms)`: pure deterministic classification.
  - `is_tradeable` convenience function.
- `health` module: per-feed staleness monitoring with circuit breaking.
  - `HealthStatus`: Healthy, Stale, Unknown.
  - `FeedHealth`: per-feed state including `consecutive_stale` counter and `elapsed_ms`.
  - `HealthMonitor`: `DashMap`-backed concurrent monitor. `register`, `heartbeat`, `check_all`, `is_circuit_open`.
  - Circuit-breaker: configurable consecutive-stale threshold; disabled at threshold 0.
- `ws` module: WebSocket lifecycle management.
  - `ReconnectPolicy`: exponential backoff with configurable multiplier, initial delay, cap, and max attempts.
  - `ConnectionConfig`: URL, channel capacity, reconnect policy, ping interval.
  - `WsManager`: stateful connection tracker with simulated connect/disconnect for deterministic tests.
- `error` module: `StreamError` enum covering connection, parsing, order book, backpressure, SPSC, aggregation, normalization, and Lorentz-transform failures.
- Benchmark harness: `benches/tick_hot_path.rs` using Criterion.

[Unreleased]: https://github.com/Mattbusel/fin-stream/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/Mattbusel/fin-stream/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/Mattbusel/fin-stream/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/Mattbusel/fin-stream/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Mattbusel/fin-stream/releases/tag/v0.1.0
