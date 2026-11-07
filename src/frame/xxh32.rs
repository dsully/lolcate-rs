const P1: u32 = 0x9E37_79B1;
const P2: u32 = 0x85EB_CA77;
const P3: u32 = 0xC2B2_AE3D;
const P4: u32 = 0x27D4_EB2F;
const P5: u32 = 0x1656_67B1;

#[must_use]
pub fn xxh32(data: &[u8]) -> u32 {
    let mut hash = Xxh32::default();
    hash.update(data);
    hash.digest()
}

/// Streaming XXH32 with seed 0, as used for lz4 frame checksums.
pub struct Xxh32 {
    acc: [u32; 4],
    buf: [u8; 16],
    buf_len: usize,
    total: u64,
}

impl Default for Xxh32 {
    fn default() -> Self {
        Self {
            acc: [P1.wrapping_add(P2), P2, 0, 0u32.wrapping_sub(P1)],
            buf: [0; 16],
            buf_len: 0,
            total: 0,
        }
    }
}

impl Xxh32 {
    fn round(acc: u32, lane: u32) -> u32 {
        acc.wrapping_add(lane.wrapping_mul(P2))
            .rotate_left(13)
            .wrapping_mul(P1)
    }

    fn lane(bytes: &[u8]) -> u32 {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn stripe(&mut self, stripe: &[u8]) {
        for (i, acc) in self.acc.iter_mut().enumerate() {
            *acc = Self::round(*acc, Self::lane(&stripe[i * 4..]));
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;

        if self.buf_len > 0 {
            let n = (16 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len < 16 {
                return;
            }
            let buf = self.buf;
            self.stripe(&buf);
            self.buf_len = 0;
        }

        let mut stripes = data.chunks_exact(16);
        for stripe in &mut stripes {
            self.stripe(stripe);
        }
        let rest = stripes.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn digest(&self) -> u32 {
        let mut h = if self.total >= 16 {
            self.acc[0]
                .rotate_left(1)
                .wrapping_add(self.acc[1].rotate_left(7))
                .wrapping_add(self.acc[2].rotate_left(12))
                .wrapping_add(self.acc[3].rotate_left(18))
        } else {
            P5
        };
        // The spec mixes in the length modulo 2^32.
        h = h.wrapping_add(self.total as u32);

        let mut words = self.buf[..self.buf_len].chunks_exact(4);
        for word in &mut words {
            h = h
                .wrapping_add(Self::lane(word).wrapping_mul(P3))
                .rotate_left(17)
                .wrapping_mul(P4);
        }
        for &byte in words.remainder() {
            h = h
                .wrapping_add(u32::from(byte).wrapping_mul(P5))
                .rotate_left(11)
                .wrapping_mul(P1);
        }

        h ^= h >> 15;
        h = h.wrapping_mul(P2);
        h ^= h >> 13;
        h = h.wrapping_mul(P3);
        h ^ (h >> 16)
    }
}

#[cfg(test)]
mod tests {
    use super::{Xxh32, xxh32};

    #[test]
    fn known_vectors() {
        assert_eq!(xxh32(b""), 0x02CC_5D05);
        assert_eq!(xxh32(b"abc"), 0x32D1_53FF);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        for split in [0, 1, 7, 15, 16, 17, 333, 999, 1000] {
            let mut h = Xxh32::default();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.digest(), xxh32(&data), "split at {split}");
        }
    }
}
