use std::{
    io::{self, Read, Seek, SeekFrom},
    ptr,
};

use fs_err as fs;
use lz4::liblz4::{
    LZ4F_VERSION, LZ4F_createDecompressionContext, LZ4F_decompress, LZ4F_freeDecompressionContext,
    LZ4FDecompressOptions, LZ4FDecompressionContext, check_error,
};

const FRAME_MAGIC: u32 = 0x184D_2204;
const CONTENT_CHECKSUM_FLAG: u8 = 0b100;
const INPUT_SIZE: usize = 256 * 1024;

/// Decodes the first lz4 frame of a file without verifying its content checksum, which instead
/// is returned by [`FrameReader::stored_checksum`] so the caller can verify it off the decode path.
pub struct FrameReader {
    ctx: LZ4FDecompressionContext,
    file: fs::File,
    input: Box<[u8]>,
    pos: usize,
    len: usize,
    consumed: u64,
    has_checksum: bool,
    done: bool,
}

impl FrameReader {
    pub fn new(mut file: fs::File) -> io::Result<Self> {
        let mut ctx = LZ4FDecompressionContext(ptr::null_mut());
        // SAFETY: `ctx` is a valid out-pointer; ownership is released in `Drop`.
        check_error(unsafe { LZ4F_createDecompressionContext(&mut ctx, LZ4F_VERSION) })?;

        let mut input = vec![0u8; INPUT_SIZE].into_boxed_slice();
        let len = crate::read_full(&mut file, &mut input)?;
        let has_checksum = len > 4
            && u32::from_le_bytes([input[0], input[1], input[2], input[3]]) == FRAME_MAGIC
            && input[4] & CONTENT_CHECKSUM_FLAG != 0;

        Ok(Self {
            ctx,
            file,
            input,
            pos: 0,
            len,
            consumed: 0,
            has_checksum,
            done: false,
        })
    }

    /// The frame's stored content checksum, once the frame has been fully read.
    pub fn stored_checksum(&mut self) -> io::Result<Option<u32>> {
        if !self.done || !self.has_checksum {
            return Ok(None);
        }
        let mut stored = [0u8; 4];
        self.file.seek(SeekFrom::Start(self.consumed - 4))?;
        self.file.read_exact(&mut stored)?;
        Ok(Some(u32::from_le_bytes(stored)))
    }
}

impl Read for FrameReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // lz4 >= 1.9.4 reads the first reserved field as `skipChecksums`.
        let options = LZ4FDecompressOptions {
            stable_dst: 0,
            reserved: [1, 0, 0],
        };

        while !self.done && !buf.is_empty() {
            if self.pos == self.len {
                self.len = self.file.read(&mut self.input)?;
                self.pos = 0;
                if self.len == 0 {
                    // Truncated frame: end the stream, matching `lz4::Decoder`.
                    return Ok(0);
                }
            }

            let mut src_size = self.len - self.pos;
            let mut dst_size = buf.len();
            // SAFETY: both sizes describe the live lengths of the slices whose pointers are passed.
            let hint = check_error(unsafe {
                LZ4F_decompress(
                    self.ctx,
                    buf.as_mut_ptr(),
                    &mut dst_size,
                    self.input[self.pos..].as_ptr(),
                    &mut src_size,
                    &raw const options,
                )
            })?;

            self.pos += src_size;
            self.consumed += src_size as u64;
            self.done = hint == 0;

            if dst_size > 0 {
                return Ok(dst_size);
            }
        }
        Ok(0)
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        // SAFETY: `ctx` was created in `new` and is not used after this.
        unsafe {
            LZ4F_freeDecompressionContext(self.ctx);
        }
    }
}

const P1: u32 = 0x9E37_79B1;
const P2: u32 = 0x85EB_CA77;
const P3: u32 = 0xC2B2_AE3D;
const P4: u32 = 0x27D4_EB2F;
const P5: u32 = 0x1656_67B1;

/// Streaming XXH32 with seed 0, as used for lz4 frame content checksums.
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
    use super::Xxh32;

    fn xxh32(data: &[u8]) -> u32 {
        let mut h = Xxh32::default();
        h.update(data);
        h.digest()
    }

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
