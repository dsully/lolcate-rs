use std::{
    ffi::{c_char, c_int},
    io::{self, BufReader, Read},
    ops::{Deref, Range},
    sync::Arc,
    thread,
};

use crossbeam_channel as channel;

// Built and linked by lz4-sys, which doesn't bind it.
unsafe extern "C" {
    fn LZ4_decompress_safe_usingDict(
        src: *const c_char,
        dst: *mut c_char,
        compressed_size: c_int,
        dst_capacity: c_int,
        dict_start: *const c_char,
        dict_size: c_int,
    ) -> c_int;
}

const FRAME_MAGIC: u32 = 0x184D_2204;
const VERSION_MASK: u8 = 0b1100_0000;
const VERSION_01: u8 = 0b0100_0000;
const INDEPENDENT_BLOCKS_FLAG: u8 = 0b10_0000;
const BLOCK_CHECKSUM_FLAG: u8 = 0b1_0000;
const CONTENT_SIZE_FLAG: u8 = 0b1000;
const CONTENT_CHECKSUM_FLAG: u8 = 0b100;
const DICT_ID_FLAG: u8 = 0b1;
const UNCOMPRESSED_BLOCK: u32 = 0x8000_0000;
/// Linked blocks may reference this much previously decoded data.
const HISTORY: usize = 64 * 1024;
const INPUT_SIZE: usize = 1 << 20;
const CHECK_QUEUE: usize = 64;
const CHUNK_QUEUE: usize = 16;
const WORK_QUEUE: usize = 256;

/// A line-aligned piece of the decompressed stream.
struct Chunk {
    buf: Vec<u8>,
    range: Range<usize>,
}

impl Deref for Chunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[self.range.clone()]
    }
}

struct Frame {
    block_max: usize,
    independent: bool,
    block_checksum: bool,
    content_checksum: bool,
}

fn lz4_error(name: &str) -> io::Error {
    io::Error::other(format!("LZ4 error: {name}"))
}

/// Like `read_exact`, but reports EOF as `false`.
fn read_or_eof(input: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    match input.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// Reads until `buf` is full or the input ends, returning the number of bytes read.
fn read_up_to(input: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn read_u32(input: &mut impl Read) -> io::Result<Option<u32>> {
    let mut bytes = [0u8; 4];
    Ok(read_or_eof(input, &mut bytes)?.then(|| u32::from_le_bytes(bytes)))
}

fn read_header(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut fixed = [0u8; 6];
    if !read_or_eof(input, &mut fixed)? {
        return Ok(None);
    }
    if u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]) != FRAME_MAGIC {
        return Err(lz4_error("ERROR_frameType_unknown"));
    }
    let (flags, block_descriptor) = (fixed[4], fixed[5]);
    if flags & VERSION_MASK != VERSION_01 {
        return Err(lz4_error("ERROR_headerVersion_wrong"));
    }
    if flags & DICT_ID_FLAG != 0 {
        return Err(lz4_error("ERROR_dictionary_unsupported"));
    }

    let mut descriptor = vec![flags, block_descriptor];
    if flags & CONTENT_SIZE_FLAG != 0 {
        let mut size = [0u8; 8];
        if !read_or_eof(input, &mut size)? {
            return Ok(None);
        }
        descriptor.extend_from_slice(&size);
    }
    let mut header_checksum = [0u8; 1];
    if !read_or_eof(input, &mut header_checksum)? {
        return Ok(None);
    }
    let mut hash = Xxh32::default();
    hash.update(&descriptor);
    if (hash.digest() >> 8).to_le_bytes()[0] != header_checksum[0] {
        return Err(lz4_error("ERROR_headerChecksum_invalid"));
    }

    let block_max = match (block_descriptor >> 4) & 0b111 {
        4 => 64 * 1024,
        5 => 256 * 1024,
        6 => 1024 * 1024,
        7 => 4 * 1024 * 1024,
        _ => return Err(lz4_error("ERROR_maxBlockSize_invalid")),
    };

    Ok(Some(Frame {
        block_max,
        independent: flags & INDEPENDENT_BLOCKS_FLAG != 0,
        block_checksum: flags & BLOCK_CHECKSUM_FLAG != 0,
        content_checksum: flags & CONTENT_CHECKSUM_FLAG != 0,
    }))
}

/// Raw block: compressed `data` unless `stored`, with its stored block checksum if the frame has them.
struct Block {
    data: Vec<u8>,
    stored: bool,
    checksum: Option<u32>,
}

enum Next {
    Block(Block),
    /// The frame is truncated after this block.
    Last(Block),
    End,
    Truncated,
}

impl Block {
    fn read(input: &mut impl Read, frame: &Frame) -> io::Result<Next> {
        let Some(size) = read_u32(input)? else {
            return Ok(Next::Truncated);
        };
        if size == 0 {
            return Ok(Next::End);
        }
        let len = (size & !UNCOMPRESSED_BLOCK) as usize;
        if len > frame.block_max {
            return Err(lz4_error("ERROR_maxBlockSize_invalid"));
        }
        let stored = size & UNCOMPRESSED_BLOCK != 0;
        let mut data = vec![0u8; len];
        let read = read_up_to(input, &mut data)?;
        if read < len {
            // `lz4::Decoder` passes on what there is of a truncated uncompressed block.
            data.truncate(if stored { read } else { 0 });
            return Ok(Next::Last(Self {
                data,
                stored: true,
                checksum: None,
            }));
        }
        let checksum = if frame.block_checksum {
            let Some(checksum) = read_u32(input)? else {
                return Ok(Next::Truncated);
            };
            Some(checksum)
        } else {
            None
        };
        Ok(Next::Block(Self {
            data,
            stored,
            checksum,
        }))
    }

    fn verify(&self) -> io::Result<()> {
        match self.checksum {
            Some(stored) if xxh32(&self.data) != stored => {
                Err(lz4_error("ERROR_blockChecksum_invalid"))
            }
            _ => Ok(()),
        }
    }

    /// Decodes into the first `block_max` bytes of `dst`, returning the decoded length.
    fn decode_into(&self, dst: &mut [u8], dict: &[u8], block_max: usize) -> io::Result<usize> {
        let dst = &mut dst[..block_max];
        if self.stored {
            dst[..self.data.len()].copy_from_slice(&self.data);
            return Ok(self.data.len());
        }
        let to_c_int = |n: usize| c_int::try_from(n).expect("block sizes fit in c_int");
        // SAFETY: every pointer/length pair describes a live slice.
        let written = unsafe {
            LZ4_decompress_safe_usingDict(
                self.data.as_ptr().cast(),
                dst.as_mut_ptr().cast(),
                to_c_int(self.data.len()),
                to_c_int(dst.len()),
                dict.as_ptr().cast(),
                to_c_int(dict.len()),
            )
        };
        usize::try_from(written).map_err(|_| lz4_error("ERROR_decompressionFailed"))
    }
}

/// Receives decoded lines on one thread; `decode` makes one per thread.
pub trait Consume: Send {
    /// Takes whole lines, each `\n`-terminated except possibly the stream's last.
    fn consume(&mut self, lines: &[u8]);

    /// Runs on the consumer's thread once its input is done.
    fn finish(&mut self) {}
}

/// Decodes the first lz4 frame of `input`, feeding its lines, in no particular order, to
/// consumers made by `new_consumer`, and returns them.
///
/// Block checksums, when present, are verified instead of the content checksum, so independent
/// blocks can be decoded, verified and consumed in parallel. A truncated frame ends the stream
/// early, as `lz4::Decoder` does.
pub fn decode<C: Consume>(
    input: impl Read,
    chunk_size: usize,
    new_consumer: &(impl Fn() -> C + Sync),
) -> io::Result<Vec<C>> {
    let mut input = BufReader::with_capacity(INPUT_SIZE, input);
    let Some(frame) = read_header(&mut input)? else {
        return Ok(Vec::new());
    };
    if frame.independent && (frame.block_checksum || !frame.content_checksum) {
        decode_parallel(&mut input, &frame, new_consumer)
    } else {
        decode_sequential(&mut input, &frame, chunk_size, new_consumer)
    }
}

enum Check {
    Block(Block),
    Content(Arc<Chunk>),
}

fn run_checks(checks: channel::Receiver<Check>) -> io::Result<u32> {
    let mut content = Xxh32::default();
    for check in checks {
        match check {
            Check::Block(block) => block.verify()?,
            Check::Content(chunk) => content.update(&chunk),
        }
    }
    Ok(content.digest())
}

/// Decodes in stream order (required for linked blocks and for the content checksum), with
/// checksums verified on another thread.
fn decode_sequential<C: Consume>(
    input: &mut impl Read,
    frame: &Frame,
    chunk_size: usize,
    new_consumer: &(impl Fn() -> C + Sync),
) -> io::Result<Vec<C>> {
    let hash_content = frame.content_checksum && !frame.block_checksum;
    let (check_tx, check_rx) = channel::bounded(CHECK_QUEUE);
    let (tx, rx) = channel::bounded::<Arc<Chunk>>(CHUNK_QUEUE);
    thread::scope(|s| {
        let checker = s.spawn(move || run_checks(check_rx));
        let consumers: Vec<_> = (0..num_cpus::get())
            .map(|_| {
                let rx = rx.clone();
                s.spawn(move || {
                    let mut consumer = new_consumer();
                    for chunk in rx {
                        consumer.consume(&chunk);
                    }
                    consumer.finish();
                    consumer
                })
            })
            .collect();
        drop(rx);

        let stored = decode_in_order(input, frame, chunk_size, &tx, &check_tx, hash_content);
        drop(tx);
        drop(check_tx);
        let consumers = consumers
            .into_iter()
            .map(|consumer| consumer.join().expect("Unable to join threads"))
            .collect();
        let digest = checker.join().expect("Unable to join threads")?;
        if stored?.is_some_and(|stored| hash_content && stored != digest) {
            return Err(lz4_error("ERROR_contentChecksum_invalid"));
        }
        Ok(consumers)
    })
}

/// Returns the stored content checksum, if the frame was read to its end and has one.
fn decode_in_order(
    input: &mut impl Read,
    frame: &Frame,
    chunk_size: usize,
    tx: &channel::Sender<Arc<Chunk>>,
    checks: &channel::Sender<Check>,
    hash_content: bool,
) -> io::Result<Option<u32>> {
    let send = |chunk: Chunk| {
        let chunk = Arc::new(chunk);
        (!hash_content || checks.send(Check::Content(chunk.clone())).is_ok())
            && tx.send(chunk).is_ok()
    };

    // Blocks decode straight into `buf` after the history they may reference, which is carried
    // over into each new buffer.
    let mut buf = vec![0u8; chunk_size.max(2 * frame.block_max)];
    let mut start = 0;
    let mut pos = 0;

    let complete = loop {
        let (block, last) = match Block::read(input, frame)? {
            Next::Block(block) => (block, false),
            Next::Last(block) => (block, true),
            Next::End => break true,
            Next::Truncated => break false,
        };

        while buf.len() - pos < frame.block_max {
            let Some(i) = memchr::memrchr(b'\n', &buf[start..pos]) else {
                buf.resize(buf.len() * 2, 0);
                continue;
            };
            let end = start + i + 1;
            let keep_from = end.min(pos.saturating_sub(HISTORY));
            let mut next = vec![0u8; buf.len()];
            next[..pos - keep_from].copy_from_slice(&buf[keep_from..pos]);
            let chunk = Chunk {
                buf: std::mem::replace(&mut buf, next),
                range: start..end,
            };
            start = end - keep_from;
            pos -= keep_from;
            if !send(chunk) {
                return Ok(None);
            }
        }

        let dict_len = if frame.independent {
            0
        } else {
            HISTORY.min(pos)
        };
        let (history, dst) = buf.split_at_mut(pos);
        pos += block.decode_into(dst, &history[pos - dict_len..], frame.block_max)?;
        if block.checksum.is_some() && checks.send(Check::Block(block)).is_err() {
            return Ok(None);
        }
        if last {
            break false;
        }
    };

    if pos > start
        && !send(Chunk {
            buf,
            range: start..pos,
        })
    {
        return Ok(None);
    }

    if complete && frame.content_checksum {
        read_u32(input)
    } else {
        Ok(None)
    }
}

/// The partial lines at the edges of a decoded independent block.
struct Edges {
    index: usize,
    /// Up to and including the block's first `\n`, or the whole block if it has none.
    head: Vec<u8>,
    /// After the block's last `\n`; `None` if it has none.
    tail: Option<Vec<u8>>,
}

/// Decodes independent blocks on worker threads, each consuming the lines of the blocks it decodes
/// while they are in cache.
fn decode_parallel<C: Consume>(
    input: &mut impl Read,
    frame: &Frame,
    new_consumer: &(impl Fn() -> C + Sync),
) -> io::Result<Vec<C>> {
    let (work_tx, work_rx) = channel::bounded::<(usize, Block)>(WORK_QUEUE);

    let (mut consumers, edges) = thread::scope(|s| {
        let workers: Vec<_> = (0..num_cpus::get())
            .map(|_| {
                let work_rx = work_rx.clone();
                s.spawn(move || -> io::Result<(C, Vec<Edges>)> {
                    let mut consumer = new_consumer();
                    let mut buf = Vec::new();
                    let mut edges = Vec::new();
                    for (index, block) in work_rx {
                        edges.push(decode_independent(
                            index,
                            &block,
                            frame.block_max,
                            &mut buf,
                            &mut consumer,
                        )?);
                    }
                    consumer.finish();
                    Ok((consumer, edges))
                })
            })
            .collect();
        drop(work_rx);

        let mut index = 0;
        let read = loop {
            match Block::read(input, frame) {
                Ok(Next::Block(block)) => {
                    if work_tx.send((index, block)).is_err() {
                        break Ok(());
                    }
                    index += 1;
                }
                Ok(Next::Last(block)) => {
                    let _ = work_tx.send((index, block));
                    index += 1;
                    break Ok(());
                }
                Ok(Next::End | Next::Truncated) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        drop(work_tx);

        let mut consumers = Vec::with_capacity(workers.len() + 1);
        let mut edges = Vec::with_capacity(index);
        for worker in workers {
            let (consumer, worker_edges) = worker.join().expect("Unable to join threads")?;
            consumers.push(consumer);
            edges.extend(worker_edges);
        }
        read.map(|()| (consumers, edges))
    })?;

    let stitched = stitch(edges);
    if !stitched.is_empty() {
        let mut consumer = new_consumer();
        consumer.consume(&stitched);
        consumer.finish();
        consumers.push(consumer);
    }
    Ok(consumers)
}

/// Decodes the block into `buf`, consumes its complete lines and returns the partial lines at
/// its edges.
fn decode_independent(
    index: usize,
    block: &Block,
    block_max: usize,
    buf: &mut Vec<u8>,
    consumer: &mut impl Consume,
) -> io::Result<Edges> {
    block.verify()?;
    buf.resize(block_max, 0);
    let len = block.decode_into(buf, &[], block_max)?;
    buf.truncate(len);

    let (Some(first), Some(last)) = (memchr::memchr(b'\n', buf), memchr::memrchr(b'\n', buf))
    else {
        return Ok(Edges {
            index,
            head: buf.clone(),
            tail: None,
        });
    };
    if first < last {
        consumer.consume(&buf[first + 1..=last]);
    }
    Ok(Edges {
        index,
        head: buf[..=first].to_vec(),
        tail: Some(buf[last + 1..].to_vec()),
    })
}

/// Joins the blocks' edges, in stream order, into the lines that span block boundaries.
fn stitch(mut edges: Vec<Edges>) -> Vec<u8> {
    edges.sort_unstable_by_key(|e| e.index);
    let mut lines = Vec::new();
    let mut carry = Vec::new();
    for edge in edges {
        match edge.tail {
            None => carry.extend_from_slice(&edge.head),
            Some(tail) => {
                lines.append(&mut carry);
                lines.extend_from_slice(&edge.head);
                carry = tail;
            }
        }
    }
    lines.append(&mut carry);
    lines
}

const P1: u32 = 0x9E37_79B1;
const P2: u32 = 0x85EB_CA77;
const P3: u32 = 0xC2B2_AE3D;
const P4: u32 = 0x27D4_EB2F;
const P5: u32 = 0x1656_67B1;

fn xxh32(data: &[u8]) -> u32 {
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
    use std::io::Write;

    use lz4::{BlockMode, BlockSize, ContentChecksum, EncoderBuilder, liblz4::BlockChecksum};

    use std::collections::HashSet;

    use super::{Consume, Xxh32, decode, xxh32};

    /// Path-like lines, a stretch of incompressible lines (stored as uncompressed blocks), and a
    /// line longer than any chunk or block.
    fn sample() -> Vec<u8> {
        let mut seed = 0x2545_F491_u32;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut data = Vec::new();
        for i in 0..40_000 {
            writeln!(
                data,
                "/Users/someone/dev/project{}/src/module{}/file{i}.rs",
                i % 37,
                i % 11
            )
            .expect("write to Vec");
        }
        for _ in 0..4_000 {
            data.extend((0..100).map(|_| b' ' + (rand() % 95) as u8));
            data.push(b'\n');
        }
        data.extend(std::iter::repeat_n(b'x', 9 << 20));
        data.push(b'\n');
        for i in 0..10_000 {
            writeln!(data, "/opt/tail/{i}").expect("write to Vec");
        }
        data
    }

    fn encode(data: &[u8], configure: impl Fn(&mut EncoderBuilder)) -> Vec<u8> {
        let mut builder = EncoderBuilder::new();
        configure(&mut builder);
        let mut encoder = builder.build(Vec::new()).expect("encoder");
        for piece in data.chunks(10_000) {
            encoder.write_all(piece).expect("encode");
        }
        let (out, result) = encoder.finish();
        result.expect("finish");
        out
    }

    fn configure(
        size: &BlockSize,
        mode: &BlockMode,
        block_checksum: bool,
        content_checksum: bool,
    ) -> impl Fn(&mut EncoderBuilder) {
        move |b| {
            b.block_size(size.clone())
                .block_mode(mode.clone())
                .block_checksum(if block_checksum {
                    BlockChecksum::BlockChecksumEnabled
                } else {
                    BlockChecksum::NoBlockChecksum
                })
                .checksum(if content_checksum {
                    ContentChecksum::ChecksumEnabled
                } else {
                    ContentChecksum::NoChecksum
                });
        }
    }

    struct Collect(Vec<Vec<u8>>);

    impl Consume for Collect {
        fn consume(&mut self, lines: &[u8]) {
            self.0.push(lines.to_vec());
        }
    }

    fn decode_all(compressed: &[u8]) -> (Vec<Vec<u8>>, std::io::Result<()>) {
        match decode(compressed, 1 << 20, &|| Collect(Vec::new())) {
            Ok(consumers) => (consumers.into_iter().flat_map(|c| c.0).collect(), Ok(())),
            Err(e) => (Vec::new(), Err(e)),
        }
    }

    fn lines(data: &[u8]) -> Vec<&[u8]> {
        data.split_inclusive(|&b| b == b'\n').collect()
    }

    const LAYOUTS: [(BlockSize, BlockMode, bool, bool); 7] = [
        (BlockSize::Max256KB, BlockMode::Linked, true, true),
        (BlockSize::Max64KB, BlockMode::Linked, false, true),
        (BlockSize::Max1MB, BlockMode::Linked, false, false),
        (BlockSize::Max64KB, BlockMode::Independent, true, false),
        (BlockSize::Max4MB, BlockMode::Independent, true, true),
        (BlockSize::Max256KB, BlockMode::Independent, false, true),
        (BlockSize::Max1MB, BlockMode::Independent, false, false),
    ];

    #[test]
    fn decodes_every_frame_layout() {
        let data = sample();
        let mut expected = lines(&data);
        expected.sort_unstable();
        for (size, mode, block_checksum, content_checksum) in LAYOUTS {
            let layout = format!("{size:?} {mode:?} {block_checksum} {content_checksum}");
            let compressed = encode(
                &data,
                configure(&size, &mode, block_checksum, content_checksum),
            );
            let (chunks, result) = decode_all(&compressed);
            result.expect("decode");

            assert!(chunks.iter().all(|c| c.ends_with(b"\n")), "{layout}");
            let mut decoded: Vec<_> = chunks.iter().flat_map(|c| lines(c)).collect();
            decoded.sort_unstable();
            assert!(decoded == expected, "{layout}");
        }
    }

    #[test]
    fn truncated_frame_yields_complete_lines_of_a_prefix() {
        let data = sample();
        let known: HashSet<_> = lines(&data).into_iter().collect();
        for (size, mode, block_checksum, content_checksum) in LAYOUTS {
            let compressed = encode(
                &data,
                configure(&size, &mode, block_checksum, content_checksum),
            );
            for cut in [0, 3, 7, 100, compressed.len() / 3, compressed.len() - 1] {
                let case = format!("{size:?} {mode:?} {block_checksum} cut at {cut}");
                let (chunks, result) = decode_all(&compressed[..cut]);
                result.expect(&case);
                let decoded: Vec<_> = chunks.iter().flat_map(|c| lines(c)).collect();
                assert!(
                    decoded.iter().map(|l| l.len()).sum::<usize>() <= data.len(),
                    "{case}"
                );
                let partial = decoded.iter().filter(|l| !known.contains(*l)).count();
                assert!(partial <= 1, "{case}");
            }
        }
    }

    #[test]
    fn rejects_corrupt_input() {
        assert!(decode_all(b"not an lz4 frame").1.is_err());

        let data = sample();
        for (size, mode, block_checksum, content_checksum) in LAYOUTS {
            let case = format!("{size:?} {mode:?} {block_checksum} {content_checksum}");
            let compressed = encode(
                &data,
                configure(&size, &mode, block_checksum, content_checksum),
            );

            let mut header = compressed.clone();
            header[6] ^= 0xff;
            assert!(decode_all(&header).1.is_err(), "header: {case}");

            if block_checksum {
                let mut block = compressed.clone();
                block[40] ^= 0x01;
                assert!(decode_all(&block).1.is_err(), "block: {case}");
            } else if content_checksum {
                let mut content = compressed.clone();
                let last = content.len() - 1;
                content[last] ^= 0x01;
                assert!(decode_all(&content).1.is_err(), "content: {case}");
            }
        }
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
