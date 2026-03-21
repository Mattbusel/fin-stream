# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

**`ohlcv` module — `OhlcvBar` analytics (round 88)**
- `OhlcvBar::avg_range_pct_of_open(bars)` — mean of `range / open` across bars.
- `OhlcvBar::high_volume_fraction(bars)` — fraction of bars with above-average volume.
- `OhlcvBar::close_cluster_count(bars)` — count of consecutive bar pairs where closes are within 0.1% of each other.
- `OhlcvBar::mean_vwap(bars)` — mean of bar VWAP values.
- `OhlcvBar::complete_fraction(bars)` — fraction of bars where all OHLCV fields are nonzero.
- `OhlcvBar::total_body_movement(bars)` — sum of `|close − open|` across all bars.

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
