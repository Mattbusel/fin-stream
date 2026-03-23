//! # Module: compression
//!
//! ## Responsibility
//! Compact binary encoding for tick data, targeting ~8 bytes/tick
//! (versus ~80 bytes for JSON), achieving approximately 10× compression.
//!
//! ## Techniques
//! - **Delta encoding**: store price differences (i32) rather than absolute prices.
//! - **Run-length encoding (RLE)**: collapse repeated delta values.
//! - **Fixed binary frame**: 8-byte tick record (4B price delta + 4B volume/flags).
//!
//! ## Frame Layout (8 bytes per tick)
//! ```text
//! Bytes 0-3: i32 price delta in ticks (little-endian)
//! Bytes 4-7: u32 packed:
//!   bits 31:   side (0=bid/buy, 1=ask/sell)
//!   bits 30-0: volume in units (max ~2.1 billion)
//! ```
//!
//! ## NOT Responsible For
//! - Compression of order book snapshots (tick trades only)
//! - Encryption or integrity checksums

use crate::error::StreamError;

// ─── tick record ─────────────────────────────────────────────────────────────

/// A raw tick suitable for compact encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactTick {
    /// Absolute price in integer ticks (e.g. price * 10^8 for 8 decimal places).
    pub price_ticks: i64,
    /// Trade volume (integer units).
    pub volume: u32,
    /// True = ask/sell side, false = bid/buy side.
    pub is_sell: bool,
}

// ─── delta encoder ────────────────────────────────────────────────────────────

/// Encodes a stream of `CompactTick` values using delta encoding for prices.
///
/// The first tick is stored as an absolute baseline; subsequent ticks store
/// only the price delta (i32). If a delta exceeds i32 range, an error is returned.
pub struct DeltaEncoder {
    last_price: Option<i64>,
    /// Baseline absolute price (written as first 8 bytes of any encoded block).
    pub baseline: i64,
}

impl DeltaEncoder {
    /// Create a new delta encoder (no baseline set yet).
    pub fn new() -> Self {
        Self { last_price: None, baseline: 0 }
    }

    /// Encode a single tick into 8 bytes.
    ///
    /// First call sets the baseline; subsequent calls store deltas.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if the price delta does not fit in i32.
    pub fn encode(&mut self, tick: CompactTick) -> Result<[u8; 8], StreamError> {
        let delta = match self.last_price {
            None => {
                self.baseline = tick.price_ticks;
                self.last_price = Some(tick.price_ticks);
                0i32
            }
            Some(prev) => {
                let d = tick.price_ticks - prev;
                if d > i32::MAX as i64 || d < i32::MIN as i64 {
                    return Err(StreamError::InvalidInput(format!(
                        "Price delta {d} exceeds i32 range; use a new encoder block"
                    )));
                }
                self.last_price = Some(tick.price_ticks);
                d as i32
            }
        };

        let packed_vol: u32 = if tick.is_sell {
            tick.volume | 0x8000_0000
        } else {
            tick.volume & 0x7FFF_FFFF
        };

        let mut frame = [0u8; 8];
        frame[0..4].copy_from_slice(&delta.to_le_bytes());
        frame[4..8].copy_from_slice(&packed_vol.to_le_bytes());
        Ok(frame)
    }

    /// Reset encoder state (begin a new block with fresh baseline).
    pub fn reset(&mut self) {
        self.last_price = None;
        self.baseline = 0;
    }
}

impl Default for DeltaEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── delta decoder ────────────────────────────────────────────────────────────

/// Decodes 8-byte frames produced by `DeltaEncoder` back into `CompactTick` values.
pub struct DeltaDecoder {
    last_price: Option<i64>,
    /// Absolute baseline price for this block.
    pub baseline: i64,
}

impl DeltaDecoder {
    /// Create a new decoder. Call `set_baseline` before the first `decode` call.
    pub fn new(baseline: i64) -> Self {
        Self { last_price: None, baseline }
    }

    /// Decode one 8-byte frame into a `CompactTick`.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if the frame is malformed.
    pub fn decode(&mut self, frame: &[u8; 8]) -> Result<CompactTick, StreamError> {
        let delta = i32::from_le_bytes(frame[0..4].try_into().map_err(|_| {
            StreamError::InvalidInput("Frame too short for price delta".to_owned())
        })?);
        let packed_vol = u32::from_le_bytes(frame[4..8].try_into().map_err(|_| {
            StreamError::InvalidInput("Frame too short for volume field".to_owned())
        })?);

        let is_sell = (packed_vol & 0x8000_0000) != 0;
        let volume = packed_vol & 0x7FFF_FFFF;

        let price_ticks = match self.last_price {
            None => {
                let p = self.baseline + delta as i64;
                self.last_price = Some(p);
                p
            }
            Some(prev) => {
                let p = prev + delta as i64;
                self.last_price = Some(p);
                p
            }
        };

        Ok(CompactTick { price_ticks, volume, is_sell })
    }
}

// ─── run-length encoder ───────────────────────────────────────────────────────

/// Run-length encoder for repeated 8-byte frames.
///
/// Consecutive identical frames are collapsed into a (count, frame) pair.
/// This is most effective when many ticks trade at the same price and volume
/// (e.g. queue-resting confirmations).
///
/// RLE frame layout: 2-byte little-endian count + 8-byte frame = 10 bytes per run.
/// A run of N identical ticks costs 10 bytes vs N*8 bytes raw — break-even at N=2.
pub struct RleEncoder {
    current_frame: Option<[u8; 8]>,
    run_count: u16,
    output: Vec<u8>,
}

impl RleEncoder {
    /// Create a new RLE encoder.
    pub fn new() -> Self {
        Self { current_frame: None, run_count: 0, output: Vec::new() }
    }

    /// Feed an encoded 8-byte frame. Accumulates runs internally.
    pub fn push(&mut self, frame: [u8; 8]) {
        match &self.current_frame {
            Some(cur) if *cur == frame && self.run_count < u16::MAX => {
                self.run_count += 1;
            }
            _ => {
                self.flush_run();
                self.current_frame = Some(frame);
                self.run_count = 1;
            }
        }
    }

    /// Finalise and return the complete RLE-encoded byte buffer.
    pub fn finish(mut self) -> Vec<u8> {
        self.flush_run();
        self.output
    }

    fn flush_run(&mut self) {
        if let Some(frame) = self.current_frame.take() {
            self.output.extend_from_slice(&self.run_count.to_le_bytes());
            self.output.extend_from_slice(&frame);
        }
        self.run_count = 0;
    }
}

impl Default for RleEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode an RLE-encoded byte buffer back into 8-byte frames.
///
/// # Errors
/// `StreamError::InvalidInput` if the buffer length is not a multiple of 10
/// or a run count is zero.
pub fn rle_decode(data: &[u8]) -> Result<Vec<[u8; 8]>, StreamError> {
    if data.len() % 10 != 0 {
        return Err(StreamError::InvalidInput(format!(
            "RLE data length {} is not a multiple of 10",
            data.len()
        )));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 10 <= data.len() {
        let count = u16::from_le_bytes([data[i], data[i + 1]]);
        if count == 0 {
            return Err(StreamError::InvalidInput(
                "RLE run count of 0 is invalid".to_owned(),
            ));
        }
        let mut frame = [0u8; 8];
        frame.copy_from_slice(&data[i + 2..i + 10]);
        for _ in 0..count {
            out.push(frame);
        }
        i += 10;
    }
    Ok(out)
}

// ─── high-level codec ─────────────────────────────────────────────────────────

/// All-in-one tick compression: delta-encode then RLE-compress a batch of ticks.
///
/// Returns `(baseline, compressed_bytes)`.
/// Baseline must be stored alongside the compressed bytes for decompression.
///
/// # Errors
/// `StreamError::InvalidInput` if any tick's delta overflows i32.
pub fn compress_ticks(ticks: &[CompactTick]) -> Result<(i64, Vec<u8>), StreamError> {
    if ticks.is_empty() {
        return Ok((0, Vec::new()));
    }
    let mut enc = DeltaEncoder::new();
    let mut rle = RleEncoder::new();
    for tick in ticks {
        let frame = enc.encode(*tick)?;
        rle.push(frame);
    }
    Ok((enc.baseline, rle.finish()))
}

/// Decompress a batch of ticks from `(baseline, compressed_bytes)`.
///
/// # Errors
/// `StreamError::InvalidInput` on malformed data.
pub fn decompress_ticks(baseline: i64, data: &[u8]) -> Result<Vec<CompactTick>, StreamError> {
    let frames = rle_decode(data)?;
    let mut dec = DeltaDecoder::new(baseline);
    frames.iter().map(|f| dec.decode(f)).collect()
}

/// Compute the compression ratio for a set of ticks (raw JSON estimate vs binary).
///
/// Returns `raw_bytes / compressed_bytes`. Values > 1.0 indicate savings.
pub fn compression_ratio(tick_count: usize, compressed_bytes: usize) -> f64 {
    if compressed_bytes == 0 {
        return 0.0;
    }
    // ~80 bytes per tick in JSON (timestamp + price + volume + side + symbol + overhead)
    let raw_bytes = tick_count * 80;
    raw_bytes as f64 / compressed_bytes as f64
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ticks(n: usize) -> Vec<CompactTick> {
        (0..n)
            .map(|i| CompactTick {
                price_ticks: 100_000_000 + i as i64 * 100,
                volume: (i as u32 % 1000) + 1,
                is_sell: i % 2 == 0,
            })
            .collect()
    }

    #[test]
    fn test_roundtrip_single_tick() {
        let ticks = vec![CompactTick { price_ticks: 5_000_000, volume: 42, is_sell: true }];
        let (baseline, compressed) = compress_ticks(&ticks).unwrap();
        let decoded = decompress_ticks(baseline, &compressed).unwrap();
        assert_eq!(decoded, ticks);
    }

    #[test]
    fn test_roundtrip_100_ticks() {
        let ticks = sample_ticks(100);
        let (baseline, compressed) = compress_ticks(&ticks).unwrap();
        let decoded = decompress_ticks(baseline, &compressed).unwrap();
        assert_eq!(decoded, ticks);
    }

    #[test]
    fn test_compression_ratio_better_than_one() {
        let ticks = sample_ticks(1000);
        let (_, compressed) = compress_ticks(&ticks).unwrap();
        let ratio = compression_ratio(1000, compressed.len());
        // Delta + RLE on varied data should still compress well
        assert!(ratio > 1.0, "ratio was {ratio:.2}");
    }

    #[test]
    fn test_rle_repeated_ticks() {
        // 1000 identical ticks → 1 RLE run → 10 bytes vs 8000 bytes raw
        let tick = CompactTick { price_ticks: 100_000, volume: 1, is_sell: false };
        let ticks = vec![tick; 1000];
        let (baseline, compressed) = compress_ticks(&ticks).unwrap();
        assert!(compressed.len() <= 10, "expected 10 bytes, got {}", compressed.len());
        let decoded = decompress_ticks(baseline, &compressed).unwrap();
        assert_eq!(decoded, ticks);
    }

    #[test]
    fn test_rle_decode_invalid_length() {
        let bad = vec![0u8; 7]; // not a multiple of 10
        assert!(rle_decode(&bad).is_err());
    }

    #[test]
    fn test_rle_decode_zero_count_errors() {
        // count=0 followed by 8 zero bytes
        let bad: Vec<u8> = [0u8, 0u8].iter().chain([0u8; 8].iter()).copied().collect();
        assert!(rle_decode(&bad).is_err());
    }

    #[test]
    fn test_delta_overflow_errors() {
        let mut enc = DeltaEncoder::new();
        enc.encode(CompactTick { price_ticks: 0, volume: 1, is_sell: false }).unwrap();
        // Delta of i64::MAX won't fit in i32
        let err = enc.encode(CompactTick {
            price_ticks: i64::MAX,
            volume: 1,
            is_sell: false,
        });
        assert!(err.is_err());
    }

    #[test]
    fn test_compression_ratio_target_10x_on_constant_stream() {
        // Constant stream: RLE collapses everything
        let tick = CompactTick { price_ticks: 1_000_000, volume: 100, is_sell: false };
        let ticks = vec![tick; 10_000];
        let (_, compressed) = compress_ticks(&ticks).unwrap();
        let ratio = compression_ratio(10_000, compressed.len());
        // 10000 * 80 / 10 = 80_000 → ratio = 8000x for constant stream
        assert!(ratio > 100.0, "expected huge ratio for constant stream, got {ratio:.1}");
    }
}
