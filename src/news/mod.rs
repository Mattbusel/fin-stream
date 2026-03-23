//! News sentiment feed integration.
//!
//! Ingest scored [`NewsItem`]s, aggregate per-symbol sentiment with recency
//! decay, compute trend buckets, and fire [`SentimentAlert`]s when sentiment
//! shifts sharply or reverses.

use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Core data types
// ─────────────────────────────────────────────────────────────────────────────

/// A single news article with pre-scored sentiment.
#[derive(Debug, Clone)]
pub struct NewsItem {
    /// Unique identifier for the article.
    pub id: String,
    /// Article headline.
    pub headline: String,
    /// Publisher / data vendor name.
    pub source: String,
    /// Article URL.
    pub url: String,
    /// Publication time as milliseconds since the Unix epoch.
    pub published_at: u64,
    /// Ticker symbols mentioned in the article.
    pub symbols: Vec<String>,
    /// Pre-computed sentiment score in \[−1.0, 1.0\] (−1 = very negative, +1 = very positive).
    pub raw_sentiment: f64,
    /// Relevance of this article to the listed symbols in \[0.0, 1.0\].
    pub relevance: f64,
}

/// Aggregated sentiment for a symbol over a time window.
#[derive(Debug, Clone)]
pub struct SentimentScore {
    /// The symbol this score relates to.
    pub symbol: String,
    /// Weighted average sentiment in \[−1.0, 1.0\].
    pub score: f64,
    /// Confidence in \[0.0, 1.0\] — higher when more items contribute.
    pub confidence: f64,
    /// Number of items that contributed to the score.
    pub item_count: u32,
    /// Timestamp (ms) at which the score was computed.
    pub timestamp: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// NewsStore
// ─────────────────────────────────────────────────────────────────────────────

const MAX_ITEMS_PER_SYMBOL: usize = 200;

/// Concurrent, symbol-keyed store of recent news items.
///
/// At most [`MAX_ITEMS_PER_SYMBOL`] (200) items are retained per symbol;
/// older items are evicted from the front of the queue.
pub struct NewsStore {
    data: DashMap<String, VecDeque<NewsItem>>,
}

impl NewsStore {
    /// Create a new, empty store.
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    /// Ingest a news item, adding it to the queue for every mentioned symbol.
    pub fn ingest(&self, item: NewsItem) {
        for symbol in &item.symbols {
            let mut queue = self
                .data
                .entry(symbol.clone())
                .or_insert_with(VecDeque::new);
            if queue.len() == MAX_ITEMS_PER_SYMBOL {
                queue.pop_front();
            }
            queue.push_back(item.clone());
        }
    }

    /// Return all items for `symbol` published at or after `since_ms`.
    pub fn recent_for(&self, symbol: &str, since_ms: u64) -> Vec<NewsItem> {
        self.data
            .get(symbol)
            .map(|q| {
                q.iter()
                    .filter(|item| item.published_at >= since_ms)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Weighted-average sentiment for `symbol` over the past `window_ms` milliseconds.
    ///
    /// Each item's weight = relevance × exp(−age_minutes / 60).
    /// Returns `None` when there are no items in the window.
    pub fn aggregate_sentiment(&self, symbol: &str, window_ms: u64) -> Option<SentimentScore> {
        let now_ms = current_time_ms();
        let cutoff = now_ms.saturating_sub(window_ms);
        let items = self.recent_for(symbol, cutoff);
        if items.is_empty() {
            return None;
        }

        let mut weighted_sum = 0.0_f64;
        let mut weight_total = 0.0_f64;

        for item in &items {
            let age_ms = now_ms.saturating_sub(item.published_at);
            let age_minutes = age_ms as f64 / 60_000.0;
            let recency_decay = (-age_minutes / 60.0).exp();
            let weight = item.relevance * recency_decay;
            weighted_sum += item.raw_sentiment * weight;
            weight_total += weight;
        }

        if weight_total == 0.0 {
            return None;
        }

        let score = weighted_sum / weight_total;
        // Confidence grows with item count, saturating at ~50 items.
        let confidence = 1.0 - (-(items.len() as f64) / 50.0).exp();

        Some(SentimentScore {
            symbol: symbol.to_string(),
            score,
            confidence,
            item_count: items.len() as u32,
            timestamp: now_ms,
        })
    }

    /// Average sentiment per equal-width time bucket, newest first.
    ///
    /// Returns a `Vec` of `(bucket_start_ms, avg_sentiment)` pairs.  Empty
    /// buckets are included with a score of 0.0.
    pub fn sentiment_trend(
        &self,
        symbol: &str,
        buckets: usize,
        bucket_ms: u64,
    ) -> Vec<(u64, f64)> {
        if buckets == 0 || bucket_ms == 0 {
            return Vec::new();
        }
        let now_ms = current_time_ms();
        let total_window = buckets as u64 * bucket_ms;
        let window_start = now_ms.saturating_sub(total_window);
        let items = self.recent_for(symbol, window_start);

        let mut bucket_sums = vec![0.0_f64; buckets];
        let mut bucket_counts = vec![0_u32; buckets];

        for item in &items {
            if item.published_at < window_start {
                continue;
            }
            let age = now_ms - item.published_at;
            let bucket_from_now = (age / bucket_ms) as usize;
            let bucket_idx = buckets.saturating_sub(1).min(bucket_from_now);
            // Invert so index 0 is the most recent bucket.
            let bucket_idx = buckets - 1 - bucket_idx;
            bucket_sums[bucket_idx] += item.raw_sentiment;
            bucket_counts[bucket_idx] += 1;
        }

        (0..buckets)
            .map(|i| {
                let bucket_start = window_start + i as u64 * bucket_ms;
                let avg = if bucket_counts[i] > 0 {
                    bucket_sums[i] / bucket_counts[i] as f64
                } else {
                    0.0
                };
                (bucket_start, avg)
            })
            .collect()
    }
}

impl Default for NewsStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// NewsSource trait + MockNewsSource
// ─────────────────────────────────────────────────────────────────────────────

/// Synchronous news source adapter.
pub trait NewsSource {
    /// Human-readable name of this source.
    fn name(&self) -> &str;
    /// Fetch the latest available news items from this source.
    fn fetch_latest(&self) -> Vec<NewsItem>;
}

/// Deterministic mock news source for testing.
pub struct MockNewsSource {
    /// The name to return from [`NewsSource::name`].
    pub source_name: String,
    /// Pre-configured items returned verbatim by [`NewsSource::fetch_latest`].
    pub items: Vec<NewsItem>,
}

impl MockNewsSource {
    /// Create a new mock source with a given name and item list.
    pub fn new(name: impl Into<String>, items: Vec<NewsItem>) -> Self {
        Self {
            source_name: name.into(),
            items,
        }
    }
}

impl NewsSource for MockNewsSource {
    fn name(&self) -> &str {
        &self.source_name
    }

    fn fetch_latest(&self) -> Vec<NewsItem> {
        self.items.clone()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SentimentAlert
// ─────────────────────────────────────────────────────────────────────────────

/// Direction of a sentiment alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentimentDirection {
    /// Sentiment jumped sharply positive.
    SharpPositive,
    /// Sentiment fell sharply negative.
    SharpNegative,
    /// Sentiment reversed direction by a significant amount.
    Reversal,
}

/// A triggered sentiment alert for a symbol.
#[derive(Debug, Clone)]
pub struct SentimentAlert {
    /// Symbol that triggered the alert.
    pub symbol: String,
    /// The direction of the sentiment change.
    pub direction: SentimentDirection,
    /// Magnitude of the change (always positive).
    pub magnitude: f64,
    /// Time at which the alert was generated (ms since Unix epoch).
    pub triggered_at: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// NewsMonitor
// ─────────────────────────────────────────────────────────────────────────────

/// High-level monitor that ingests news, maintains the store, and fires alerts.
pub struct NewsMonitor {
    /// Shared news store.
    pub store: Arc<NewsStore>,
    /// Minimum absolute score change required to trigger an alert.
    pub alert_threshold: f64,
}

impl NewsMonitor {
    /// Create a new monitor wrapping the provided store.
    pub fn new(store: Arc<NewsStore>, alert_threshold: f64) -> Self {
        Self { store, alert_threshold }
    }

    /// Compare sentiment over the most recent half-window vs the prior half-window
    /// and return any triggered alerts for `symbol`.
    ///
    /// Uses a 2-hour comparison window split into two 1-hour halves.
    pub fn check_alerts(&self, symbol: &str) -> Vec<SentimentAlert> {
        let now_ms = current_time_ms();
        let hour_ms: u64 = 3_600_000;

        // Recent window: last hour.
        let recent = self
            .store
            .aggregate_sentiment_at(symbol, now_ms.saturating_sub(hour_ms), now_ms);
        // Prior window: the hour before that.
        let prior = self.store.aggregate_sentiment_at(
            symbol,
            now_ms.saturating_sub(2 * hour_ms),
            now_ms.saturating_sub(hour_ms),
        );

        let mut alerts = Vec::new();
        if let (Some(recent_score), Some(prior_score)) = (recent, prior) {
            let delta = recent_score - prior_score;
            if delta.abs() >= self.alert_threshold {
                let direction = if recent_score > 0.0 && prior_score < 0.0 {
                    SentimentDirection::Reversal
                } else if delta > 0.0 {
                    SentimentDirection::SharpPositive
                } else {
                    SentimentDirection::SharpNegative
                };
                alerts.push(SentimentAlert {
                    symbol: symbol.to_string(),
                    direction,
                    magnitude: delta.abs(),
                    triggered_at: now_ms,
                });
            }
        }
        alerts
    }
}

// Internal helper: bounded-window raw sentiment average (not exported).
impl NewsStore {
    fn aggregate_sentiment_at(
        &self,
        symbol: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Option<f64> {
        let items = self.recent_for(symbol, from_ms);
        let window: Vec<&NewsItem> = items
            .iter()
            .filter(|i| i.published_at <= to_ms)
            .collect();
        if window.is_empty() {
            return None;
        }
        let sum: f64 = window.iter().map(|i| i.raw_sentiment).sum();
        Some(sum / window.len() as f64)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Falls back to 0 if the system clock is unavailable (should never happen in practice).
fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(symbol: &str, sentiment: f64, age_ms: u64) -> NewsItem {
        let now = current_time_ms();
        NewsItem {
            id: uuid_stub(),
            headline: format!("{symbol} news"),
            source: "test".to_string(),
            url: "http://example.com".to_string(),
            published_at: now.saturating_sub(age_ms),
            symbols: vec![symbol.to_string()],
            raw_sentiment: sentiment,
            relevance: 1.0,
        }
    }

    fn uuid_stub() -> String {
        // Minimal unique string without pulling in uuid crate.
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
    }

    #[test]
    fn ingest_and_retrieve() {
        let store = NewsStore::new();
        store.ingest(make_item("AAPL", 0.5, 60_000));
        let items = store.recent_for("AAPL", 0);
        assert_eq!(items.len(), 1);
        assert!((items[0].raw_sentiment - 0.5).abs() < 1e-10);
    }

    #[test]
    fn aggregate_sentiment_positive() {
        let store = NewsStore::new();
        for _ in 0..5 {
            store.ingest(make_item("TSLA", 0.8, 30_000)); // 30 s ago
        }
        let score = store.aggregate_sentiment("TSLA", 3_600_000);
        assert!(score.is_some());
        let s = score.unwrap();
        assert!(s.score > 0.0, "expected positive sentiment, got {}", s.score);
        assert!(s.confidence > 0.0);
        assert_eq!(s.item_count, 5);
    }

    #[test]
    fn aggregate_sentiment_empty_window() {
        let store = NewsStore::new();
        // Items are 3 hours old, window is only 1 hour.
        store.ingest(make_item("MSFT", -0.4, 3 * 3_600_000));
        let score = store.aggregate_sentiment("MSFT", 3_600_000);
        assert!(score.is_none(), "expected None for items outside window");
    }

    #[test]
    fn max_items_per_symbol_cap() {
        let store = NewsStore::new();
        for _ in 0..250 {
            store.ingest(make_item("NVDA", 0.1, 1000));
        }
        let items = store.recent_for("NVDA", 0);
        assert!(
            items.len() <= super::MAX_ITEMS_PER_SYMBOL,
            "store exceeded max items: {}",
            items.len()
        );
    }

    #[test]
    fn sentiment_trend_returns_buckets() {
        let store = NewsStore::new();
        store.ingest(make_item("AMZN", 0.3, 10_000));
        store.ingest(make_item("AMZN", -0.2, 3_700_000));
        let trend = store.sentiment_trend("AMZN", 4, 3_600_000);
        assert_eq!(trend.len(), 4);
    }

    #[test]
    fn mock_news_source_returns_items() {
        let items = vec![make_item("GOOG", 0.6, 0)];
        let source = MockNewsSource::new("test-source", items);
        assert_eq!(source.name(), "test-source");
        let fetched = source.fetch_latest();
        assert_eq!(fetched.len(), 1);
    }

    #[test]
    fn news_monitor_check_alerts_no_crash() {
        let store = Arc::new(NewsStore::new());
        let monitor = NewsMonitor::new(Arc::clone(&store), 0.3);
        // Should not panic on an empty store.
        let alerts = monitor.check_alerts("UNKNOWN");
        assert!(alerts.is_empty());
    }

    #[test]
    fn news_monitor_fires_alert_on_sharp_change() {
        let store = Arc::new(NewsStore::new());

        // Recent: very positive (within last 30 min).
        for _ in 0..10 {
            store.ingest(make_item("BTC", 0.9, 10 * 60_000)); // 10 min ago
        }
        // Prior: very negative (between 61 min and 90 min ago).
        for _ in 0..10 {
            store.ingest(make_item("BTC", -0.9, 75 * 60_000)); // 75 min ago
        }

        let monitor = NewsMonitor::new(Arc::clone(&store), 0.3);
        let alerts = monitor.check_alerts("BTC");
        assert!(!alerts.is_empty(), "expected at least one alert");
        let alert = &alerts[0];
        assert!(alert.magnitude >= 0.3);
    }
}
