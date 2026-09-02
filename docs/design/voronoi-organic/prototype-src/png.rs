//! Minimal deterministic PNG writer: fixed-Huffman deflate + greedy LZ77.
//!
//! No external crates. Same input bytes -> same output bytes, always.

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

struct BitWriter {
    out: Vec<u8>,
    bit: u32,
    acc: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), bit: 0, acc: 0 }
    }
    /// LSB-first bit packing (deflate's convention for extra bits / block headers).
    fn bits(&mut self, value: u32, n: u32) {
        self.acc |= (value & ((1u32 << n) - 1)) << self.bit;
        self.bit += n;
        while self.bit >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.bit -= 8;
        }
    }
    /// Huffman codes are transmitted MSB-first, so reverse before packing.
    fn huff(&mut self, code: u32, n: u32) {
        let mut r = 0u32;
        for i in 0..n {
            r |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.bits(r, n);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bit > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn emit_literal(w: &mut BitWriter, byte: u8) {
    let v = u32::from(byte);
    if v < 144 {
        w.huff(0x30 + v, 8);
    } else {
        w.huff(0x190 + (v - 144), 9);
    }
}

fn emit_match(w: &mut BitWriter, len: usize, dist: usize) {
    let mut li = 28;
    while li > 0 && LEN_BASE[li] as usize > len {
        li -= 1;
    }
    let sym = 257 + li as u32;
    if sym < 280 {
        w.huff(sym - 256, 7);
    } else {
        w.huff(0xC0 + (sym - 280), 8);
    }
    let e = u32::from(LEN_EXTRA[li]);
    if e > 0 {
        w.bits((len - LEN_BASE[li] as usize) as u32, e);
    }
    let mut di = 29;
    while di > 0 && DIST_BASE[di] as usize > dist {
        di -= 1;
    }
    w.huff(di as u32, 5);
    let de = u32::from(DIST_EXTRA[di]);
    if de > 0 {
        w.bits((dist - DIST_BASE[di] as usize) as u32, de);
    }
}

/// Fixed-Huffman deflate with a greedy 3-byte-hash LZ77 matcher.
fn deflate(data: &[u8]) -> Vec<u8> {
    const HASH_BITS: u32 = 16;
    const HASH_SIZE: usize = 1 << HASH_BITS;
    const MAX_CHAIN: usize = 24;
    const WINDOW: usize = 32768;

    let mut w = BitWriter::new();
    w.bits(1, 1); // final block
    w.bits(1, 2); // fixed Huffman

    let mut head = vec![u32::MAX; HASH_SIZE];
    let mut prev = vec![u32::MAX; data.len().max(1)];

    let hash = |d: &[u8], i: usize| -> usize {
        let a = u32::from(d[i]);
        let b = u32::from(d[i + 1]);
        let c = u32::from(d[i + 2]);
        (((a << 10) ^ (b << 5) ^ c).wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    };

    let mut i = 0usize;
    while i < data.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + 3 <= data.len() {
            let h = hash(data, i);
            let mut cand = head[h];
            let mut chain = 0;
            while cand != u32::MAX && chain < MAX_CHAIN {
                let c = cand as usize;
                let dist = i - c;
                if dist > WINDOW || dist == 0 {
                    break;
                }
                let maxl = (data.len() - i).min(258);
                let mut l = 0usize;
                while l < maxl && data[c + l] == data[i + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_dist = dist;
                    if l >= 258 {
                        break;
                    }
                }
                cand = prev[c];
                chain += 1;
            }
            prev[i] = head[h];
            head[h] = i as u32;
        }
        if best_len >= 3 {
            emit_match(&mut w, best_len, best_dist);
            // Insert the skipped positions so later matches can find them.
            for k in (i + 1)..(i + best_len).min(data.len().saturating_sub(2)) {
                let h = hash(data, k);
                prev[k] = head[h];
                head[h] = k as u32;
            }
            i += best_len;
        } else {
            emit_literal(&mut w, data[i]);
            i += 1;
        }
    }
    w.huff(0, 7); // end of block
    w.finish()
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut crc_in = Vec::with_capacity(4 + body.len());
    crc_in.extend_from_slice(kind);
    crc_in.extend_from_slice(body);
    out.extend_from_slice(&crc_in);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

/// Encode an RGB8 buffer (`w*h*3` bytes) as a PNG.
pub fn encode_rgb(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), w * h * 3);
    // Filter type 1 (Sub) on every row: cheap, and it helps a lot on flat fills.
    let mut raw = Vec::with_capacity(h * (1 + w * 3));
    for y in 0..h {
        raw.push(1u8);
        let row = &rgb[y * w * 3..(y + 1) * w * 3];
        for x in 0..w * 3 {
            let left = if x >= 3 { row[x - 3] } else { 0 };
            raw.push(row[x].wrapping_sub(left));
        }
    }
    let mut z = Vec::new();
    z.push(0x78);
    z.push(0x01);
    z.extend_from_slice(&deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
