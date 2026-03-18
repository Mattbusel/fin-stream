//! Typed error hierarchy for fin-stream.
//!
//! All fallible operations in the pipeline return `StreamError`. Variants are
//! grouped by subsystem: connection/WebSocket, tick parsing, order book,
//! backpressure, and the streaming pipeline internals (ring buffer, aggregation,
//! normalization, transforms).

/// Unified error type for all fin-stream pipeline operations.
///
/// Each variant carries enough context to reconstruct the failure site without
/// inspecting internal state. The `Display` impl is machine-parseable: field
/// values never contain the literal substring used as a delimiter.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// WebSocket connection failed.
    #[error("WebSocket connection failed to '{url}': {reason}")]
    ConnectionFailed {
        /// The WebSocket URL that could not be reached.
        url: String,
        /// Human-readable description of the failure.
        reason: String,
    },

    /// WebSocket disconnected unexpectedly.
    #[error("WebSocket disconnected from '{url}'")]
    Disconnected {
        /// The WebSocket URL that was disconnected.
        url: String,
    },

    /// Reconnection attempts exhausted.
    #[error("Reconnection exhausted after {attempts} attempts to '{url}'")]
    ReconnectExhausted {
        /// The target URL for reconnection.
        url: String,
        /// Total number of reconnect attempts made.
        attempts: u32,
    },

    /// Tick deserialization failed.
    #[error("Tick parse error from {exchange}: {reason}")]
    ParseError {
        /// Name of the exchange that sent the unparseable tick.
        exchange: String,
        /// Description of the parse failure.
        reason: String,
    },

    /// Feed is stale -- no data received within staleness threshold.
    #[error("Feed '{feed_id}' is stale: last tick was {elapsed_ms}ms ago (threshold: {threshold_ms}ms)")]
    StaleFeed {
        /// Identifier of the stale feed.
        feed_id: String,
        /// Milliseconds since the last tick was received.
        elapsed_ms: u64,
        /// Configured staleness threshold in milliseconds.
        threshold_ms: u64,
    },

    /// Order book reconstruction failed.
    #[error("Order book reconstruction failed for '{symbol}': {reason}")]
    BookReconstructionFailed {
        /// Symbol whose order book could not be reconstructed.
        symbol: String,
        /// Description of the reconstruction failure.
        reason: String,
    },

    /// Order book is crossed (bid >= ask).
    #[error("Order book crossed for '{symbol}': best bid {bid} >= best ask {ask}")]
    BookCrossed {
        /// Symbol with the crossed book.
        symbol: String,
        /// Best bid price as a string.
        bid: String,
        /// Best ask price as a string.
        ask: String,
    },

    /// Backpressure: the downstream channel is full.
    #[error("Backpressure on channel '{channel}': {depth}/{capacity} slots used")]
    Backpressure {
        /// Name or URL of the backpressured channel.
        channel: String,
        /// Current number of items queued.
        depth: usize,
        /// Maximum capacity of the channel.
        capacity: usize,
    },

    /// Invalid exchange format.
    #[error("Unknown exchange format: '{0}'")]
    UnknownExchange(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(String),

    /// WebSocket protocol error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    // ── Pipeline-internal errors ─────────────────────────────────────────────

    /// SPSC ring buffer is full; the producer must back off or drop the item.
    ///
    /// This variant is returned by [`crate::ring::SpscRing::push`] when the
    /// buffer has no free slots. It never panics.
    #[error("SPSC ring buffer is full (capacity: {capacity})")]
    RingBufferFull {
        /// Configured usable capacity of the ring buffer (N - 1 slots).
        capacity: usize,
    },

    /// SPSC ring buffer is empty; no item is available for the consumer.
    ///
    /// This variant is returned by [`crate::ring::SpscRing::pop`] when there
    /// are no pending items. Callers should retry or park the consumer thread.
    #[error("SPSC ring buffer is empty")]
    RingBufferEmpty,

    /// An error occurred during OHLCV bar aggregation.
    ///
    /// Wraps structural errors such as receiving a tick for the wrong symbol or
    /// a timeframe with a zero-duration period.
    #[error("OHLCV aggregation error: {reason}")]
    AggregationError {
        /// Description of the aggregation failure.
        reason: String,
    },

    /// An error occurred during coordinate normalization.
    ///
    /// Typically indicates that the normalizer received a value outside the
    /// expected numeric range, or that the rolling window is not yet seeded.
    #[error("Normalization error: {reason}")]
    NormalizationError {
        /// Description of the normalization failure.
        reason: String,
    },

    /// A tick failed structural validation before entering the pipeline.
    ///
    /// Examples: negative price, zero quantity, timestamp in the past beyond
    /// the configured tolerance.
    #[error("Invalid tick: {reason}")]
    InvalidTick {
        /// Description of the validation failure.
        reason: String,
    },

    /// The Lorentz transform configuration is invalid.
    ///
    /// The relativistic velocity parameter beta (v/c) must satisfy 0 <= beta < 1.
    /// A beta of exactly 1 (or above) would produce a division by zero in the
    /// Lorentz factor gamma = 1 / sqrt(1 - beta^2).
    #[error("Lorentz config error: {reason}")]
    LorentzConfigError {
        /// Description of the configuration error.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_failed_display() {
        let e = StreamError::ConnectionFailed {
            url: "wss://example.com".into(),
            reason: "timeout".into(),
        };
        assert!(e.to_string().contains("example.com"));
        assert!(e.to_string().contains("timeout"));
    }

    #[test]
    fn test_disconnected_display() {
        let e = StreamError::Disconnected { url: "wss://feed.io".into() };
        assert!(e.to_string().contains("feed.io"));
    }

    #[test]
    fn test_reconnect_exhausted_display() {
        let e = StreamError::ReconnectExhausted { url: "wss://x.io".into(), attempts: 5 };
        assert!(e.to_string().contains("5"));
    }

    #[test]
    fn test_parse_error_display() {
        let e = StreamError::ParseError { exchange: "Binance".into(), reason: "missing field".into() };
        assert!(e.to_string().contains("Binance"));
    }

    #[test]
    fn test_stale_feed_display() {
        let e = StreamError::StaleFeed {
            feed_id: "BTC-USD".into(),
            elapsed_ms: 5000,
            threshold_ms: 2000,
        };
        assert!(e.to_string().contains("BTC-USD"));
        assert!(e.to_string().contains("5000"));
    }

    #[test]
    fn test_book_reconstruction_failed_display() {
        let e = StreamError::BookReconstructionFailed {
            symbol: "ETH-USD".into(),
            reason: "gap in sequence".into(),
        };
        assert!(e.to_string().contains("ETH-USD"));
    }

    #[test]
    fn test_book_crossed_display() {
        let e = StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: "50001".into(),
            ask: "50000".into(),
        };
        assert!(e.to_string().contains("crossed"));
    }

    #[test]
    fn test_backpressure_display() {
        let e = StreamError::Backpressure { channel: "ticks".into(), depth: 1000, capacity: 1000 };
        assert!(e.to_string().contains("1000"));
    }

    #[test]
    fn test_unknown_exchange_display() {
        let e = StreamError::UnknownExchange("Kraken".into());
        assert!(e.to_string().contains("Kraken"));
    }

    #[test]
    fn test_ring_buffer_full_display() {
        let e = StreamError::RingBufferFull { capacity: 1024 };
        assert!(e.to_string().contains("1024"));
        assert!(e.to_string().contains("full"));
    }

    #[test]
    fn test_ring_buffer_empty_display() {
        let e = StreamError::RingBufferEmpty;
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn test_aggregation_error_display() {
        let e = StreamError::AggregationError { reason: "wrong symbol".into() };
        assert!(e.to_string().contains("wrong symbol"));
    }

    #[test]
    fn test_normalization_error_display() {
        let e = StreamError::NormalizationError { reason: "window not seeded".into() };
        assert!(e.to_string().contains("window not seeded"));
    }

    #[test]
    fn test_invalid_tick_display() {
        let e = StreamError::InvalidTick { reason: "negative price".into() };
        assert!(e.to_string().contains("negative price"));
    }

    #[test]
    fn test_lorentz_config_error_display() {
        let e = StreamError::LorentzConfigError { reason: "beta >= 1".into() };
        assert!(e.to_string().contains("beta >= 1"));
    }
}
