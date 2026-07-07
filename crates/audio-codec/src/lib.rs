//! Audio codecs for AirPlay streams.
//!
//! Step 4: minimal ALAC encoder producing *uncompressed* ("verbatim") frames —
//! the same approach OwnTone's RAOP output uses in production. A real ALAC
//! compressor (Apple reference via `cc`, or pure-Rust) and FDK-AAC arrive in
//! Step 5; the bitstream here is already valid ALAC that every receiver plays.
//!
//! Frame layout for an uncompressed stereo frame (bit-packed, MSB first):
//! ```text
//! 3 bits  channels - 1            (1 = stereo)
//! 4 bits  unknown/reserved        (0)
//! 8 bits  unknown/reserved        (0)
//! 4 bits  unknown/reserved        (0)
//! 1 bit   has-size flag           (0)
//! 2 bits  wasted bytes            (0)
//! 1 bit   is-not-compressed       (1)
//! then    16-bit big-endian interleaved PCM samples
//! ```

/// Samples per ALAC frame for realtime AirPlay (both AP1 and AP2 use 352).
pub const FRAMES_PER_PACKET: usize = 352;

/// MSB-first bit writer.
struct BitWriter {
    out: Vec<u8>,
    /// Bits already used in the last byte (0..8).
    used: u32,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        BitWriter { out: Vec::with_capacity(capacity), used: 8 }
    }

    /// Write the lowest `bits` bits of `value`, MSB first.
    fn write(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32);
        let mut remaining = bits;
        while remaining > 0 {
            if self.used == 8 {
                self.out.push(0);
                self.used = 0;
            }
            let space = 8 - self.used;
            let take = remaining.min(space);
            let shift = remaining - take;
            let chunk = ((value >> shift) & ((1u32 << take) - 1)) as u8;
            let idx = self.out.len() - 1;
            self.out[idx] |= chunk << (space - take);
            self.used += take;
            remaining -= take;
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Encode interleaved stereo S16 samples into one uncompressed ALAC frame.
///
/// `samples` must contain exactly `FRAMES_PER_PACKET * 2` values (L/R pairs);
/// shorter input is zero-padded (last packet of a stream).
pub fn alac_encode_verbatim(samples: &[i16]) -> Vec<u8> {
    let total = FRAMES_PER_PACKET * 2;
    // 23 header bits + 16 bits per sample
    let mut w = BitWriter::new(3 + total * 2);
    w.write(1, 3); // channels - 1 (stereo)
    w.write(0, 4);
    w.write(0, 8);
    w.write(0, 4);
    w.write(0, 1); // has-size
    w.write(0, 2); // wasted bytes
    w.write(1, 1); // is-not-compressed
    for i in 0..total {
        let s = samples.get(i).copied().unwrap_or(0) as u16;
        w.write(u32::from(s), 16);
    }
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_length_is_exact() {
        let samples = vec![0i16; FRAMES_PER_PACKET * 2];
        let frame = alac_encode_verbatim(&samples);
        // ceil((23 + 704*16) / 8) = ceil(11287/8) = 1411
        assert_eq!(frame.len(), 1411);
    }

    #[test]
    fn header_bits_and_sample_alignment() {
        // All-zero samples: header = 0b001_0000_00000000_0000_0_00_1 then zeros.
        let samples = vec![0i16; FRAMES_PER_PACKET * 2];
        let frame = alac_encode_verbatim(&samples);
        assert_eq!(frame[0], 0b0010_0000); // 3 bits ch=1, then zeros
        assert_eq!(frame[1], 0);
        // 23rd bit (is-not-compressed) = 1 → third byte = 0b0000_0010
        assert_eq!(frame[2], 0b0000_0010);
        assert!(frame[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn sample_bits_survive_offset() {
        // One sample of 0x8001 must appear split across the 1-bit offset.
        let mut samples = vec![0i16; FRAMES_PER_PACKET * 2];
        samples[0] = i16::from_be_bytes([0x80, 0x01]);
        let frame = alac_encode_verbatim(&samples);
        // After 23 header bits, sample bits start at bit 23:
        // byte2 gets is-not-compressed(1) + top 6 bits of 0x8001 (100000) → 0b0000_0011, wait:
        // byte2 bits: [23 header bit = 1][sample bits 15..9 = 1000000] → 0b0000_0011_0...
        // Simply verify roundtrip by re-reading bits instead:
        let mut bits: Vec<bool> = Vec::new();
        for b in &frame {
            for i in (0..8).rev() {
                bits.push(b >> i & 1 == 1);
            }
        }
        let sample_bits = &bits[23..23 + 16];
        let mut v = 0u16;
        for &bit in sample_bits {
            v = (v << 1) | u16::from(bit);
        }
        assert_eq!(v, 0x8001);
    }

    #[test]
    fn short_input_zero_padded() {
        let samples = vec![1i16; 10];
        let frame = alac_encode_verbatim(&samples);
        assert_eq!(frame.len(), 1411);
    }
}
