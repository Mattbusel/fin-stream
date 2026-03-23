//! # Module: persistence
//!
//! ## Responsibility
//! Persist tick data to a packed binary format with per-symbol indexing.
//! All I/O is in-memory (Vec<u8>) so this module is fully testable without
//! touching the filesystem.
//!
//! ## Binary Format
//! Each record begins with an 8-byte magic header followed by length-prefixed
//! UTF-8 string fields and 8-byte little-endian numeric fields.
//!
//! Record layout:
//! ```text
//! [8 bytes magic]
//! [4 bytes symbol length][symbol bytes]
//! [8 bytes price   (f64 LE)]
//! [8 bytes quantity (f64 LE)]
//! [8 bytes timestamp_ms (u64 LE)]
//! [4 bytes exchange length][exchange bytes]
//! ```

// ─── Magic Bytes ─────────────────────────────────────────────────────────────

/// Magic header that identifies a valid tick record.
pub const MAGIC: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

// ─── TickRecord ───────────────────────────────────────────────────────────────

/// A single tick record that can be serialized to/from binary.
///
/// # Example
/// ```rust
/// use fin_stream::persistence::TickRecord;
///
/// let rec = TickRecord {
///     symbol: "AAPL".to_string(),
///     price: 150.25,
///     quantity: 100.0,
///     timestamp_ms: 1_700_000_000_000,
///     exchange: "NASDAQ".to_string(),
/// };
/// let bytes = rec.to_bytes();
/// let recovered = TickRecord::from_bytes(&bytes).unwrap();
/// assert_eq!(recovered.symbol, "AAPL");
/// assert!((recovered.price - 150.25).abs() < 1e-9);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct TickRecord {
    /// Ticker symbol.
    pub symbol: String,
    /// Trade price.
    pub price: f64,
    /// Trade quantity.
    pub quantity: f64,
    /// Trade timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
    /// Exchange identifier.
    pub exchange: String,
}

impl TickRecord {
    /// Serialize the record to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64);

        // Magic
        buf.extend_from_slice(&MAGIC);

        // Symbol (4-byte length prefix + bytes)
        let sym_bytes = self.symbol.as_bytes();
        buf.extend_from_slice(&(sym_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(sym_bytes);

        // Price
        buf.extend_from_slice(&self.price.to_le_bytes());

        // Quantity
        buf.extend_from_slice(&self.quantity.to_le_bytes());

        // Timestamp
        buf.extend_from_slice(&self.timestamp_ms.to_le_bytes());

        // Exchange (4-byte length prefix + bytes)
        let exch_bytes = self.exchange.as_bytes();
        buf.extend_from_slice(&(exch_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(exch_bytes);

        buf
    }

    /// Deserialize a record from bytes. Returns `None` on malformed input.
    pub fn from_bytes(bytes: &[u8]) -> Option<TickRecord> {
        let mut cursor = 0usize;

        // Check magic
        if bytes.len() < 8 {
            return None;
        }
        if &bytes[cursor..cursor + 8] != &MAGIC {
            return None;
        }
        cursor += 8;

        // Symbol
        let (symbol, advance) = read_string(bytes, cursor)?;
        cursor += advance;

        // Price (8 bytes f64 LE)
        if bytes.len() < cursor + 8 {
            return None;
        }
        let price = f64::from_le_bytes(bytes[cursor..cursor + 8].try_into().ok()?);
        cursor += 8;

        // Quantity
        if bytes.len() < cursor + 8 {
            return None;
        }
        let quantity = f64::from_le_bytes(bytes[cursor..cursor + 8].try_into().ok()?);
        cursor += 8;

        // Timestamp
        if bytes.len() < cursor + 8 {
            return None;
        }
        let timestamp_ms = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().ok()?);
        cursor += 8;

        // Exchange
        let (exchange, _) = read_string(bytes, cursor)?;

        Some(TickRecord {
            symbol,
            price,
            quantity,
            timestamp_ms,
            exchange,
        })
    }
}

/// Read a 4-byte length-prefixed UTF-8 string from `bytes` at `offset`.
/// Returns `(string, bytes_consumed)` or `None` on error.
fn read_string(bytes: &[u8], offset: usize) -> Option<(String, usize)> {
    if bytes.len() < offset + 4 {
        return None;
    }
    let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
    let start = offset + 4;
    if bytes.len() < start + len {
        return None;
    }
    let s = std::str::from_utf8(&bytes[start..start + len]).ok()?;
    Some((s.to_string(), 4 + len))
}

// ─── TickIndex ────────────────────────────────────────────────────────────────

/// Index entry for a single symbol within the stored tick data.
#[derive(Debug, Clone, PartialEq)]
pub struct TickIndex {
    /// Symbol this index entry covers.
    pub symbol: String,
    /// Number of records for this symbol.
    pub record_count: u64,
    /// Earliest timestamp among records for this symbol.
    pub min_timestamp_ms: u64,
    /// Latest timestamp among records for this symbol.
    pub max_timestamp_ms: u64,
    /// Byte offset of the first record for this symbol in the data buffer.
    pub byte_offset: u64,
}

// ─── TickWriter ───────────────────────────────────────────────────────────────

/// In-memory tick writer. Accumulates serialized records and computes an index.
///
/// # Example
/// ```rust
/// use fin_stream::persistence::{TickRecord, TickWriter};
///
/// let mut writer = TickWriter::new("test.bin");
/// let rec = TickRecord {
///     symbol: "BTC".to_string(),
///     price: 30_000.0,
///     quantity: 0.5,
///     timestamp_ms: 1_000,
///     exchange: "Binance".to_string(),
/// };
/// let offset = writer.write(&rec);
/// assert_eq!(offset, 0);
/// let _bytes_written = writer.flush();
/// let index = writer.finalize();
/// assert_eq!(index.len(), 1);
/// assert_eq!(index[0].symbol, "BTC");
/// ```
pub struct TickWriter {
    /// Internal buffer accumulating serialized records.
    buffer: Vec<u8>,
    /// (offset, record) pairs for index construction.
    records: Vec<(u64, TickRecord)>,
    #[allow(dead_code)]
    path: String,
}

impl TickWriter {
    /// Create a new writer targeting `path` (data stays in memory).
    pub fn new(path: &str) -> TickWriter {
        TickWriter {
            buffer: Vec::new(),
            records: Vec::new(),
            path: path.to_string(),
        }
    }

    /// Serialize and append a record. Returns the byte offset of this record.
    pub fn write(&mut self, record: &TickRecord) -> u64 {
        let offset = self.buffer.len() as u64;
        let bytes = record.to_bytes();
        self.buffer.extend_from_slice(&bytes);
        self.records.push((offset, record.clone()));
        offset
    }

    /// Return the number of bytes currently in the buffer (simulates a flush).
    pub fn flush(&self) -> usize {
        self.buffer.len()
    }

    /// Compute a per-symbol index from all written records.
    pub fn finalize(&self) -> Vec<TickIndex> {
        use std::collections::HashMap;

        // symbol -> (first_offset, count, min_ts, max_ts)
        let mut map: HashMap<&str, (u64, u64, u64, u64)> = HashMap::new();

        for (offset, rec) in &self.records {
            let entry = map.entry(rec.symbol.as_str()).or_insert((*offset, 0, u64::MAX, 0));
            entry.1 += 1; // count
            if rec.timestamp_ms < entry.2 {
                entry.2 = rec.timestamp_ms;
            }
            if rec.timestamp_ms > entry.3 {
                entry.3 = rec.timestamp_ms;
            }
        }

        let mut index: Vec<TickIndex> = map
            .into_iter()
            .map(|(sym, (offset, count, min_ts, max_ts))| TickIndex {
                symbol: sym.to_string(),
                record_count: count,
                min_timestamp_ms: min_ts,
                max_timestamp_ms: max_ts,
                byte_offset: offset,
            })
            .collect();

        index.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        index
    }

    /// Consume the writer and return the internal byte buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }
}

// ─── TickReader ───────────────────────────────────────────────────────────────

/// In-memory tick reader. Deserializes records from a byte buffer.
///
/// # Example
/// ```rust
/// use fin_stream::persistence::{TickRecord, TickWriter, TickReader};
///
/// let mut writer = TickWriter::new("test.bin");
/// writer.write(&TickRecord {
///     symbol: "ETH".to_string(), price: 2000.0, quantity: 1.0,
///     timestamp_ms: 5000, exchange: "Kraken".to_string(),
/// });
/// let data = writer.into_bytes();
/// let mut reader = TickReader::new(data);
/// let all = reader.read_all();
/// assert_eq!(all.len(), 1);
/// assert_eq!(all[0].symbol, "ETH");
/// ```
pub struct TickReader {
    data: Vec<u8>,
}

impl TickReader {
    /// Create a reader over an in-memory byte buffer.
    pub fn new(data: Vec<u8>) -> TickReader {
        TickReader { data }
    }

    /// Deserialize all records from the buffer.
    pub fn read_all(&self) -> Vec<TickRecord> {
        self.parse_records(|_| true)
    }

    /// Deserialize records whose timestamp falls within `[start_ms, end_ms]`.
    pub fn read_range(&self, start_ms: u64, end_ms: u64) -> Vec<TickRecord> {
        self.parse_records(|r| r.timestamp_ms >= start_ms && r.timestamp_ms <= end_ms)
    }

    /// Return the distinct symbols present in the buffer.
    pub fn symbols(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for rec in self.read_all() {
            if !seen.contains(&rec.symbol) {
                seen.push(rec.symbol);
            }
        }
        seen
    }

    /// Internal: scan the buffer sequentially, applying `predicate` to each record.
    fn parse_records<F>(&self, predicate: F) -> Vec<TickRecord>
    where
        F: Fn(&TickRecord) -> bool,
    {
        let mut out = Vec::new();
        let mut cursor = 0usize;

        while cursor < self.data.len() {
            // Find the next magic header
            if self.data.len() < cursor + 8 {
                break;
            }
            if &self.data[cursor..cursor + 8] != &MAGIC {
                cursor += 1;
                continue;
            }

            // Try to parse from here
            match TickRecord::from_bytes(&self.data[cursor..]) {
                Some(rec) => {
                    let record_len = rec.to_bytes().len();
                    if predicate(&rec) {
                        out.push(rec);
                    }
                    cursor += record_len;
                }
                None => {
                    // Skip one byte and keep scanning
                    cursor += 1;
                }
            }
        }

        out
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(symbol: &str, price: f64, ts: u64) -> TickRecord {
        TickRecord {
            symbol: symbol.to_string(),
            price,
            quantity: 10.0,
            timestamp_ms: ts,
            exchange: "TEST".to_string(),
        }
    }

    // ── TickRecord serialization ──

    #[test]
    fn test_round_trip() {
        let rec = TickRecord {
            symbol: "AAPL".to_string(),
            price: 150.25,
            quantity: 100.0,
            timestamp_ms: 1_700_000_000_000,
            exchange: "NASDAQ".to_string(),
        };
        let bytes = rec.to_bytes();
        let recovered = TickRecord::from_bytes(&bytes).unwrap();
        assert_eq!(recovered.symbol, rec.symbol);
        assert!((recovered.price - rec.price).abs() < 1e-9);
        assert!((recovered.quantity - rec.quantity).abs() < 1e-9);
        assert_eq!(recovered.timestamp_ms, rec.timestamp_ms);
        assert_eq!(recovered.exchange, rec.exchange);
    }

    #[test]
    fn test_from_bytes_bad_magic() {
        let mut bytes = sample_record("X", 1.0, 0).to_bytes();
        bytes[0] = 0x00; // corrupt magic
        assert!(TickRecord::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_from_bytes_too_short() {
        assert!(TickRecord::from_bytes(&[0xDE, 0xAD]).is_none());
    }

    #[test]
    fn test_magic_constant() {
        assert_eq!(MAGIC, [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04]);
    }

    // ── TickWriter ──

    #[test]
    fn test_writer_offsets() {
        let mut w = TickWriter::new("test.bin");
        let r1 = sample_record("A", 1.0, 1000);
        let r2 = sample_record("B", 2.0, 2000);
        let off1 = w.write(&r1);
        let off2 = w.write(&r2);
        assert_eq!(off1, 0);
        assert_eq!(off2, r1.to_bytes().len() as u64);
    }

    #[test]
    fn test_writer_flush_returns_byte_count() {
        let mut w = TickWriter::new("test.bin");
        w.write(&sample_record("A", 1.0, 0));
        let flushed = w.flush();
        assert!(flushed > 0);
    }

    #[test]
    fn test_writer_finalize_single_symbol() {
        let mut w = TickWriter::new("test.bin");
        w.write(&sample_record("BTC", 30_000.0, 1000));
        w.write(&sample_record("BTC", 30_100.0, 2000));
        w.write(&sample_record("BTC", 29_900.0, 500));
        let index = w.finalize();
        assert_eq!(index.len(), 1);
        let idx = &index[0];
        assert_eq!(idx.symbol, "BTC");
        assert_eq!(idx.record_count, 3);
        assert_eq!(idx.min_timestamp_ms, 500);
        assert_eq!(idx.max_timestamp_ms, 2000);
        assert_eq!(idx.byte_offset, 0);
    }

    #[test]
    fn test_writer_finalize_multi_symbol() {
        let mut w = TickWriter::new("test.bin");
        w.write(&sample_record("AAPL", 150.0, 1000));
        w.write(&sample_record("GOOG", 2800.0, 2000));
        w.write(&sample_record("AAPL", 151.0, 3000));
        let index = w.finalize();
        assert_eq!(index.len(), 2);
        let aapl = index.iter().find(|i| i.symbol == "AAPL").unwrap();
        assert_eq!(aapl.record_count, 2);
        assert_eq!(aapl.min_timestamp_ms, 1000);
        assert_eq!(aapl.max_timestamp_ms, 3000);
        let goog = index.iter().find(|i| i.symbol == "GOOG").unwrap();
        assert_eq!(goog.record_count, 1);
    }

    // ── TickReader ──

    fn build_data(records: &[TickRecord]) -> Vec<u8> {
        let mut w = TickWriter::new("mem");
        for r in records {
            w.write(r);
        }
        w.into_bytes()
    }

    #[test]
    fn test_read_all() {
        let recs = vec![
            sample_record("A", 1.0, 100),
            sample_record("B", 2.0, 200),
            sample_record("A", 1.5, 300),
        ];
        let data = build_data(&recs);
        let reader = TickReader::new(data);
        let all = reader.read_all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].symbol, "A");
        assert_eq!(all[1].symbol, "B");
        assert_eq!(all[2].symbol, "A");
    }

    #[test]
    fn test_read_range_inclusive() {
        let recs = vec![
            sample_record("X", 1.0, 100),
            sample_record("X", 1.0, 200),
            sample_record("X", 1.0, 300),
            sample_record("X", 1.0, 400),
        ];
        let data = build_data(&recs);
        let reader = TickReader::new(data);
        let range = reader.read_range(150, 350);
        assert_eq!(range.len(), 2);
        assert_eq!(range[0].timestamp_ms, 200);
        assert_eq!(range[1].timestamp_ms, 300);
    }

    #[test]
    fn test_read_range_empty_result() {
        let recs = vec![sample_record("X", 1.0, 1000)];
        let data = build_data(&recs);
        let reader = TickReader::new(data);
        let range = reader.read_range(0, 500);
        assert!(range.is_empty());
    }

    #[test]
    fn test_symbols() {
        let recs = vec![
            sample_record("AAPL", 1.0, 100),
            sample_record("GOOG", 2.0, 200),
            sample_record("AAPL", 1.1, 300),
        ];
        let data = build_data(&recs);
        let reader = TickReader::new(data);
        let mut syms = reader.symbols();
        syms.sort();
        assert_eq!(syms, vec!["AAPL", "GOOG"]);
    }

    #[test]
    fn test_empty_buffer() {
        let reader = TickReader::new(Vec::new());
        assert!(reader.read_all().is_empty());
        assert!(reader.symbols().is_empty());
    }

    #[test]
    fn test_unicode_symbol_and_exchange() {
        let rec = TickRecord {
            symbol: "BTC-USDT".to_string(),
            price: 30_000.0,
            quantity: 0.001,
            timestamp_ms: 99999,
            exchange: "Binance-Spot".to_string(),
        };
        let bytes = rec.to_bytes();
        let recovered = TickRecord::from_bytes(&bytes).unwrap();
        assert_eq!(recovered.symbol, "BTC-USDT");
        assert_eq!(recovered.exchange, "Binance-Spot");
    }
}
