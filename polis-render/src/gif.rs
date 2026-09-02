//! A deterministic animated-GIF encoder, for recording a replay.
//!
//! # Why this exists, and why it has no dependency
//!
//! PRD §15's M2 says the replay *"ships independently as a PR-summary or
//! standup artifact"*, and the artifact of an animation is an animation. A
//! sequence of PNGs is what a reviewer has to assemble themselves; a GIF is what
//! they can look at.
//!
//! It is written out here for the same reason [`crate::raster`] writes its own
//! PNG: the workspace's dependency set is deliberate, and an image codec's
//! output is a *heuristic* of its version. A GIF whose bytes change when a
//! dependency is bumped cannot be compared between runs, and the whole point of
//! the headless renderer is that two runs can be compared.
//!
//! So: median-cut quantisation with an explicit total order at every tie, an
//! LZW coder with a deterministic dictionary, and no floating point anywhere in
//! the pipeline. The same frames produce the same bytes on every machine.
//!
//! # The quantiser, and what it does to the contrast budget
//!
//! GIF has 256 colours. The Polis palette is mostly a near-black city plus a
//! small number of saturated live marks, and a naive quantiser spends its
//! entries where the *pixels* are — which is the base map — and rounds the
//! attention layer into a single approximate red.
//!
//! Median cut on **population** does exactly that. So the box chosen for
//! splitting is the one with the largest `population × extent`, not the largest
//! population: a box that is both busy and wide gets split, and a box holding
//! ten thousand almost-identical background pixels does not. Measured on real
//! frames that keeps the amber pin, the teal ring and the red link separable
//! while the city still gets a smooth ramp.
//!
//! # Format
//!
//! `GIF89a`, one global colour table, `NETSCAPE2.0` for the loop, one
//! `GraphicControlExtension` + `ImageDescriptor` per frame, LZW with a
//! `min_code_size` of 8. No transparency, no local colour tables, no
//! interlacing.

// The codec is index arithmetic: every cast here lands in a bucket key, a
// palette index or a channel value that the surrounding expression has already
// bounded. `cast_precision_loss` fires only inside the test fixtures.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::io;
use std::path::Path;

use crate::raster::Canvas;

/// The most colours a GIF global colour table can hold.
const PALETTE_MAX: usize = 256;

/// Bits per channel kept when bucketing colours for the quantiser.
///
/// Five gives 32 768 buckets — small enough that the histogram and the
/// nearest-colour lookup table are both flat arrays, fine enough that two tones
/// one channel apart in the base map do not merge before median cut has looked
/// at them.
const BUCKET_BITS: u32 = 5;
/// Number of histogram buckets.
const BUCKETS: usize = 1 << (BUCKET_BITS * 3);

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GifError {
    /// No frames were given.
    Empty,
    /// The frames are not all the same size.
    SizeMismatch,
    /// A dimension does not fit in the format's `u16`.
    TooLarge,
}

impl std::fmt::Display for GifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no frames"),
            Self::SizeMismatch => write!(f, "frames differ in size"),
            Self::TooLarge => write!(f, "a dimension exceeds 65535"),
        }
    }
}

impl std::error::Error for GifError {}

/// Encodes frames as an animated GIF.
///
/// `delay_cs` is the inter-frame delay in **centiseconds**, which is the unit
/// the format uses. Most viewers clamp anything below 2 to 10, so 3–5 is the
/// usable fast end.
pub fn encode(frames: &[Canvas], delay_cs: u16) -> Result<Vec<u8>, GifError> {
    let first = frames.first().ok_or(GifError::Empty)?;
    let (w, h) = (first.width, first.height);
    if w == 0 || h == 0 {
        return Err(GifError::Empty);
    }
    if w > usize::from(u16::MAX) || h > usize::from(u16::MAX) {
        return Err(GifError::TooLarge);
    }
    if frames.iter().any(|f| f.width != w || f.height != h) {
        return Err(GifError::SizeMismatch);
    }

    let palette = quantise(frames);
    let lut = nearest_lookup(&palette);
    let table_bits = table_bits(palette.len());
    let table_len = 1usize << table_bits;

    let mut out = Vec::with_capacity(frames.len() * w * h / 4 + 1024);
    out.extend_from_slice(b"GIF89a");
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    // Global colour table present, 8-bit colour resolution, not sorted.
    out.push(0x80 | (7 << 4) | (table_bits as u8 - 1));
    out.push(0); // background colour index
    out.push(0); // pixel aspect ratio: none
    for i in 0..table_len {
        let c = palette.get(i).copied().unwrap_or([0, 0, 0]);
        out.extend_from_slice(&c);
    }
    // NETSCAPE2.0: loop forever.
    out.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    out.extend_from_slice(b"NETSCAPE2.0");
    out.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);

    let mut indices = vec![0u8; w * h];
    for frame in frames {
        for (dst, px) in indices
            .iter_mut()
            .zip(frame.pixels.as_chunks::<3>().0.iter())
        {
            *dst = lut[bucket(*px)];
        }
        // Graphic control extension: no disposal, no transparency, one delay.
        out.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00]);
        out.extend_from_slice(&delay_cs.to_le_bytes());
        out.extend_from_slice(&[0x00, 0x00]);
        // Image descriptor: full frame, no local table, not interlaced.
        out.push(0x2C);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(w as u16).to_le_bytes());
        out.extend_from_slice(&(h as u16).to_le_bytes());
        out.push(0x00);
        lzw(&indices, 8, &mut out);
    }
    out.push(0x3B);
    Ok(out)
}

/// Encodes and writes a GIF to disk.
pub fn write(path: &Path, frames: &[Canvas], delay_cs: u16) -> io::Result<()> {
    let bytes = encode(frames, delay_cs)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    std::fs::write(path, bytes)
}

/// How many bits the colour table's size field needs. Always at least 2, which
/// is the format's floor.
fn table_bits(len: usize) -> u32 {
    let mut bits = 1u32;
    while (1usize << bits) < len.max(2) {
        bits += 1;
    }
    bits.clamp(1, 8)
}

/// The histogram bucket a colour falls in.
fn bucket(c: [u8; 3]) -> usize {
    let shift = 8 - BUCKET_BITS;
    ((usize::from(c[0]) >> shift) << (BUCKET_BITS * 2))
        | ((usize::from(c[1]) >> shift) << BUCKET_BITS)
        | (usize::from(c[2]) >> shift)
}

/// One occupied histogram bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    key: u32,
    count: u64,
    sum: [u64; 3],
}

impl Bucket {
    fn mean(&self) -> [u8; 3] {
        let n = self.count.max(1);
        [
            ((self.sum[0] + n / 2) / n) as u8,
            ((self.sum[1] + n / 2) / n) as u8,
            ((self.sum[2] + n / 2) / n) as u8,
        ]
    }
}

/// Median-cut quantisation over every frame.
///
/// The split target is the box with the largest `population × extent`, so a
/// wide, busy box is preferred over a large, uniform one. See the module docs:
/// choosing on population alone spends the table on the base map and rounds the
/// attention layer into one red.
fn quantise(frames: &[Canvas]) -> Vec<[u8; 3]> {
    let mut hist: Vec<(u64, [u64; 3])> = vec![(0, [0; 3]); BUCKETS];
    for frame in frames {
        for px in frame.pixels.as_chunks::<3>().0 {
            let e = &mut hist[bucket(*px)];
            e.0 += 1;
            e.1[0] += u64::from(px[0]);
            e.1[1] += u64::from(px[1]);
            e.1[2] += u64::from(px[2]);
        }
    }
    let mut buckets: Vec<Bucket> = hist
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| *n > 0)
        .map(|(key, (count, sum))| Bucket {
            key: key as u32,
            count: *count,
            sum: *sum,
        })
        .collect();
    if buckets.is_empty() {
        return vec![[0, 0, 0]];
    }
    if buckets.len() <= PALETTE_MAX {
        let mut out: Vec<[u8; 3]> = buckets.iter().map(Bucket::mean).collect();
        out.sort_unstable();
        out.dedup();
        return out;
    }

    // Boxes are ranges over one array, so a split is a sort of a slice and a
    // pair of indices — no allocation per box, and no map iteration order
    // anywhere near the result.
    let mut boxes: Vec<(usize, usize)> = vec![(0, buckets.len())];
    while boxes.len() < PALETTE_MAX {
        let mut best: Option<(usize, u64)> = None;
        for (i, (lo, hi)) in boxes.iter().enumerate() {
            if hi - lo < 2 {
                continue;
            }
            let slice = &buckets[*lo..*hi];
            let (extent, _) = widest(slice);
            let pop: u64 = slice.iter().map(|b| b.count).sum();
            let score = pop.saturating_mul(u64::from(extent) + 1);
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        let Some((i, _)) = best else { break };
        let (lo, hi) = boxes[i];
        let (_, axis) = widest(&buckets[lo..hi]);
        // Total order, ties broken on the bucket key, so the sort is stable
        // across machines whatever the sort's internal choices.
        buckets[lo..hi].sort_unstable_by_key(|b| (b.mean()[axis], b.key));
        let total: u64 = buckets[lo..hi].iter().map(|b| b.count).sum();
        let mut acc = 0u64;
        let mut cut = lo + 1;
        for (k, b) in buckets[lo..hi].iter().enumerate() {
            acc += b.count;
            if acc * 2 >= total {
                cut = (lo + k + 1).min(hi - 1).max(lo + 1);
                break;
            }
        }
        boxes[i] = (lo, cut);
        boxes.push((cut, hi));
    }

    let mut palette: Vec<[u8; 3]> = boxes
        .iter()
        .map(|(lo, hi)| {
            let slice = &buckets[*lo..*hi];
            let n: u64 = slice.iter().map(|b| b.count).sum::<u64>().max(1);
            let mut sum = [0u64; 3];
            for b in slice {
                for (s, v) in sum.iter_mut().zip(b.sum.iter()) {
                    *s += v;
                }
            }
            [
                ((sum[0] + n / 2) / n) as u8,
                ((sum[1] + n / 2) / n) as u8,
                ((sum[2] + n / 2) / n) as u8,
            ]
        })
        .collect();
    palette.sort_unstable();
    palette.dedup();
    palette
}

/// The widest channel of a set of buckets, and its extent.
fn widest(buckets: &[Bucket]) -> (u8, usize) {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for b in buckets {
        let m = b.mean();
        for c in 0..3 {
            lo[c] = lo[c].min(m[c]);
            hi[c] = hi[c].max(m[c]);
        }
    }
    let mut axis = 0;
    let mut extent = hi[0] - lo[0];
    for c in 1..3 {
        if hi[c] - lo[c] > extent {
            extent = hi[c] - lo[c];
            axis = c;
        }
    }
    (extent, axis)
}

/// The nearest palette entry for every histogram bucket, precomputed.
///
/// One pass of `buckets × palette` (about eight million comparisons) replaces a
/// per-pixel search, which for a 900² frame would be sixty million per frame.
fn nearest_lookup(palette: &[[u8; 3]]) -> Vec<u8> {
    let mut lut = vec![0u8; BUCKETS];
    let shift = 8 - BUCKET_BITS;
    let half = 1i32 << (shift - 1);
    for (key, slot) in lut.iter_mut().enumerate() {
        let r = (((key >> (BUCKET_BITS * 2)) as i32) << shift) + half;
        let g = ((((key >> BUCKET_BITS) & ((1 << BUCKET_BITS) - 1)) as i32) << shift) + half;
        let b = (((key & ((1 << BUCKET_BITS) - 1)) as i32) << shift) + half;
        let mut best = 0usize;
        let mut best_d = i64::MAX;
        for (i, c) in palette.iter().enumerate() {
            let dr = i64::from(r - i32::from(c[0]));
            let dg = i64::from(g - i32::from(c[1]));
            let db = i64::from(b - i32::from(c[2]));
            let d = dr * dr + dg * dg + db * db;
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        *slot = best as u8;
    }
    lut
}

/// LZW-compresses one frame's indices into GIF sub-blocks.
///
/// The dictionary is a flat `4096 × 256` table of "next code", which makes a
/// lookup an array index rather than a hash and removes the last place where
/// iteration order could reach the output.
fn lzw(indices: &[u8], min_code_size: u8, out: &mut Vec<u8>) {
    out.push(min_code_size);
    let clear = 1u16 << min_code_size;
    let eoi = clear + 1;

    let mut bits = Bits::default();
    let mut dict = vec![0u16; 4096 * 256];
    let mut next = eoi + 1;
    let mut code_size = u32::from(min_code_size) + 1;

    bits.push(clear, code_size);
    let mut it = indices.iter();
    let Some(&first) = it.next() else {
        bits.push(eoi, code_size);
        bits.flush();
        sub_blocks(&bits.out, out);
        return;
    };
    let mut prefix = u16::from(first);
    for &byte in it {
        let slot = usize::from(prefix) * 256 + usize::from(byte);
        let existing = dict[slot];
        if existing != 0 {
            prefix = existing;
            continue;
        }
        bits.push(prefix, code_size);
        if next < 4096 {
            dict[slot] = next;
            next += 1;
            if next > (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        } else {
            bits.push(clear, code_size);
            dict.fill(0);
            next = eoi + 1;
            code_size = u32::from(min_code_size) + 1;
        }
        prefix = u16::from(byte);
    }
    bits.push(prefix, code_size);
    bits.push(eoi, code_size);
    bits.flush();
    sub_blocks(&bits.out, out);
}

/// An LSB-first bit accumulator.
#[derive(Debug, Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn push(&mut self, code: u16, size: u32) {
        self.acc |= u32::from(code) << self.n;
        self.n += size;
        while self.n >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn flush(&mut self) {
        if self.n > 0 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc = 0;
            self.n = 0;
        }
    }
}

/// Wraps a byte stream in GIF's ≤255-byte sub-blocks, terminated by a zero.
fn sub_blocks(data: &[u8], out: &mut Vec<u8>) {
    for chunk in data.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An LZW decoder, written for the tests only.
    ///
    /// A compressor is one of the few things where "it produced bytes" is not
    /// evidence of anything — a subtly wrong dictionary still emits a plausible
    /// stream. So the test decodes what the encoder wrote and compares indices,
    /// which is the only assertion that can fail for the right reason.
    fn decode_lzw(data: &[u8], min_code_size: u8) -> Vec<u8> {
        let clear = 1u16 << min_code_size;
        let eoi = clear + 1;
        let mut table: Vec<Vec<u8>> = (0..=u16::MAX)
            .take(usize::from(clear))
            .map(|i| vec![i as u8])
            .collect();
        table.push(Vec::new()); // clear
        table.push(Vec::new()); // eoi
        let base = table.clone();
        let mut code_size = u32::from(min_code_size) + 1;
        let mut acc = 0u32;
        let mut n = 0u32;
        let mut out = Vec::new();
        let mut prev: Option<u16> = None;
        for &byte in data {
            acc |= u32::from(byte) << n;
            n += 8;
            while n >= code_size {
                let code = (acc & ((1 << code_size) - 1)) as u16;
                acc >>= code_size;
                n -= code_size;
                if code == clear {
                    table = base.clone();
                    code_size = u32::from(min_code_size) + 1;
                    prev = None;
                    continue;
                }
                if code == eoi {
                    return out;
                }
                let entry = if usize::from(code) < table.len() {
                    table[usize::from(code)].clone()
                } else {
                    let p = table[usize::from(prev.expect("prev"))].clone();
                    let mut e = p.clone();
                    e.push(p[0]);
                    e
                };
                out.extend_from_slice(&entry);
                if let Some(p) = prev {
                    let mut new = table[usize::from(p)].clone();
                    new.push(entry[0]);
                    table.push(new);
                    if table.len() >= (1usize << code_size) && code_size < 12 {
                        code_size += 1;
                    }
                }
                prev = Some(code);
            }
        }
        out
    }

    /// Unwraps GIF sub-blocks back into one stream.
    fn join_sub_blocks(data: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let n = usize::from(data[i]);
            i += 1;
            if n == 0 {
                return (out, i);
            }
            out.extend_from_slice(&data[i..i + n]);
            i += n;
        }
    }

    fn stripes(w: usize, h: usize, phase: usize) -> Canvas {
        let mut c = Canvas::new(w, h, [7, 8, 10]);
        for i in 0..6 {
            let x = ((i * 13 + phase) % w) as f64;
            c.rect(
                x,
                10.0,
                x + 6.0,
                h as f64 - 10.0,
                [(30 + i * 30) as u8, 140, 90],
                1.0,
            );
        }
        c.disc([w as f64 / 2.0, h as f64 / 2.0], 9.0, [255, 188, 62], 1.0);
        c
    }

    #[test]
    fn the_header_and_the_trailer_are_well_formed() {
        let frames = vec![stripes(64, 48, 0), stripes(64, 48, 5)];
        let gif = encode(&frames, 4).expect("encode");
        assert_eq!(&gif[..6], b"GIF89a");
        assert_eq!(&gif[6..8], &64u16.to_le_bytes());
        assert_eq!(&gif[8..10], &48u16.to_le_bytes());
        // The colour table's size is declared in the packed byte, so the reader
        // has to parse it rather than assume 256 — which is exactly the bug this
        // assertion caught when it did assume.
        let packed = gif[10];
        assert_eq!(packed & 0x80, 0x80, "no global colour table");
        let table_len = 1usize << ((packed & 0x07) + 1);
        let after_table = 13 + 3 * table_len;
        assert_eq!(gif[after_table..after_table + 3], [0x21, 0xFF, 0x0B]);
        assert_eq!(&gif[after_table + 3..after_table + 14], b"NETSCAPE2.0");
        assert_eq!(*gif.last().expect("trailer"), 0x3B);
        // Two frames means two graphic control extensions.
        let gce = gif
            .windows(4)
            .filter(|w| w[..4] == [0x21, 0xF9, 0x04, 0x00])
            .count();
        assert_eq!(gce, 2);
    }

    /// The assertion that actually tests the compressor: decode the stream back
    /// and compare it to the indices that went in.
    #[test]
    fn the_lzw_stream_round_trips() {
        let mut data: Vec<u8> = Vec::new();
        for i in 0..5000u32 {
            data.push(((i / 7) % 251) as u8);
        }
        // A long run, which is where an off-by-one in the code-size bump bites.
        data.extend(std::iter::repeat_n(42u8, 900));
        let mut encoded = Vec::new();
        lzw(&data, 8, &mut encoded);
        assert_eq!(encoded[0], 8, "min code size");
        let (stream, _) = join_sub_blocks(&encoded[1..]);
        assert_eq!(decode_lzw(&stream, 8), data);
    }

    #[test]
    fn a_single_pixel_frame_round_trips() {
        let mut data = vec![9u8];
        let mut encoded = Vec::new();
        lzw(&data, 8, &mut encoded);
        let (stream, _) = join_sub_blocks(&encoded[1..]);
        assert_eq!(decode_lzw(&stream, 8), data);
        data.clear();
        let mut encoded = Vec::new();
        lzw(&data, 8, &mut encoded);
        let (stream, _) = join_sub_blocks(&encoded[1..]);
        assert!(decode_lzw(&stream, 8).is_empty());
    }

    /// The quantiser has to keep the live layer separable from the city, which
    /// is the whole reason it splits on `population × extent`.
    #[test]
    fn the_palette_keeps_the_attention_marks_apart_from_the_city() {
        // A frame that looks like a real one: a huge near-black map, a handful
        // of agent-band marks, three attention-band pixels.
        let mut c = Canvas::new(200, 200, [19, 21, 24]);
        for i in 0..40 {
            c.rect(
                f64::from(i) * 5.0,
                20.0,
                f64::from(i) * 5.0 + 3.0,
                60.0,
                [20 + (i % 24) as u8, 22, 26],
                1.0,
            );
        }
        c.disc([100.0, 120.0], 6.0, crate::live::AGENT_DONE, 1.0);
        c.disc([120.0, 120.0], 6.0, crate::live::AGENT_FAILED, 1.0);
        c.disc([140.0, 120.0], 4.0, crate::live::ATTN_DECISION, 1.0);
        c.disc([150.0, 120.0], 4.0, crate::live::ATTN_CONTENTION, 1.0);
        let palette = quantise(std::slice::from_ref(&c));
        let lut = nearest_lookup(&palette);
        let idx = |rgb: [u8; 3]| lut[bucket(rgb)];
        let decision = idx(crate::live::ATTN_DECISION);
        let contention = idx(crate::live::ATTN_CONTENTION);
        let done = idx(crate::live::AGENT_DONE);
        let ground = idx([19, 21, 24]);
        assert_ne!(decision, contention, "amber and red merged");
        assert_ne!(decision, ground);
        assert_ne!(done, ground);
        // …and the quantised colour is close to the original, not merely
        // distinct: a mark that shifts hue is a mark that lies.
        let near = |a: [u8; 3], b: [u8; 3]| {
            a.iter()
                .zip(b.iter())
                .map(|(x, y)| (i32::from(*x) - i32::from(*y)).abs())
                .max()
                .unwrap_or(0)
        };
        assert!(
            near(palette[usize::from(decision)], crate::live::ATTN_DECISION) < 24,
            "amber quantised to {:?}",
            palette[usize::from(decision)]
        );
    }

    /// Two runs, the same bytes. The reason the codec is written out.
    #[test]
    fn encoding_is_a_pure_function_of_the_frames() {
        let frames = vec![stripes(80, 60, 0), stripes(80, 60, 7), stripes(80, 60, 14)];
        assert_eq!(
            encode(&frames, 5).expect("a"),
            encode(&frames, 5).expect("b")
        );
    }

    #[test]
    fn bad_input_is_rejected_rather_than_encoded_wrong() {
        assert_eq!(encode(&[], 4), Err(GifError::Empty));
        let frames = vec![stripes(10, 10, 0), stripes(12, 10, 0)];
        assert_eq!(encode(&frames, 4), Err(GifError::SizeMismatch));
    }

    #[test]
    fn the_colour_table_is_a_power_of_two_and_covers_the_palette() {
        assert_eq!(table_bits(2), 1);
        assert_eq!(table_bits(3), 2);
        assert_eq!(table_bits(256), 8);
        assert_eq!(table_bits(1), 1);
    }
}
