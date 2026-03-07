//! Typed error hierarchy for fin-stream.

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// WebSocket connection failed.
    #[error("WebSocket connection failed to '{url}': {reason}")]
    ConnectionFailed { url: String, reason: String },

    /// WebSocket disconnected unexpectedly.
    #[error("WebSocket disconnected from '{url}'")]
    Disconnected { url: String },

    /// Reconnection attempts exhausted.
    #[error("Reconnection exhausted after {attempts} attempts to '{url}'")]
    ReconnectExhausted { url: String, attempts: u32 },

    /// Tick deserialization failed.
    #[error("Tick parse error from {exchange}: {reason}")]
    ParseError { exchange: String, reason: String },

    /// Feed is stale — no data received within staleness threshold.
    #[error("Feed '{feed_id}' is stale: last tick was {elapsed_ms}ms ago (threshold: {threshold_ms}ms)")]
    StaleFeed { feed_id: String, elapsed_ms: u64, threshold_ms: u64 },

    /// Order book reconstruction failed.
    #[error("Order book reconstruction failed for '{symbol}': {reason}")]
    BookReconstructionFailed { symbol: String, reason: String },

    /// Order book is crossed (bid >= ask).
    #[error("Order book crossed for '{symbol}': best bid {bid} >= best ask {ask}")]
    BookCrossed { symbol: String, bid: String, ask: String },

    /// Backpressure: the downstream channel is full.
    #[error("Backpressure on channel '{channel}': {depth}/{capacity} slots used")]
    Backpressure { channel: String, depth: usize, capacity: usize },

    /// Invalid exchange format.
    #[error("Unknown exchange format: '{0}'")]
    UnknownExchange(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(String),

    /// WebSocket protocol error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),
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
}
