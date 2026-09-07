//! A deterministic software rasteriser and PNG writer.
//!
//! # Why a CPU rasteriser lives in the GPU crate
//!
//! PRD §15's M1 gate ends in "render it to a window (**or PNG**)", and a PNG has
//! to be producible with no adapter, no surface and no window — on a CI runner,
//! in a headless container, from a test. Taking the wgpu path for that means
//! standing up a device and reading back a texture, which is both fragile across
//! drivers and, more importantly, **not reproducible**: rasterisation rules
//! differ between vendors, so two machines would disagree about the bytes of the
//! image while agreeing perfectly about the layout underneath.
//!
//! So the static plan is drawn here, in integer-stable `f64` arithmetic, and the
//! same bytes come out on every machine. That is what lets the M1 gate compare
//! images as well as layouts.
//!
//! The PNG is written with **stored (uncompressed) deflate blocks**. That is a
//! valid zlib stream every decoder accepts, it needs no dependency, and its
//! output is a pure function of the pixels — which a compressor's heuristics
//! would not be.

// The middle of the pipeline is numeric geometry, and five lint families fire on
// nearly every line of it without telling us anything:
//
// * the `cast_*` family — every cast here lands in a bucket index or a quantised
//   sort key that is clamped or wrapped on purpose;
// * `float_cmp` — exact float comparison is how a determinism tie is broken
//   (PRD §7.4), and an approximate comparison there would be the bug;
// * `many_single_char_names` and `similar_names` — `a`, `b`, `c`, `n`, `p` are
//   the names the geometry itself uses;
// * `too_many_lines` — a pipeline stage read as one ordered sequence is clearer
//   than the same code cut into fragments each called once;
// * `assigning_clones` — the buffers reassigned here are rebuilt from scratch,
//   so `clone_from` would save nothing.
#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::io;
use std::path::Path;

/// A colour, without alpha. Alpha is a blend parameter, not a channel.
pub type Rgb = [u8; 3];

/// A point in device space: pixels, y down.
pub type Px = [f64; 2];

/// An RGB image being drawn into.
#[derive(Debug, Clone)]
pub struct Canvas {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Row-major RGB.
    pub pixels: Vec<u8>,
    /// Scanline crossing buffer, reused so a fill does not allocate.
    crossings: Vec<f64>,
}

impl Canvas {
    /// A canvas filled with one colour.
    #[must_use]
    pub fn new(width: usize, height: usize, background: Rgb) -> Self {
        let mut pixels = Vec::with_capacity(width * height * 3);
        for _ in 0..width * height {
            pixels.extend_from_slice(&background);
        }
        Self {
            width,
            height,
            pixels,
            crossings: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn blend(&mut self, x: usize, y: usize, colour: Rgb, alpha: f64) {
        if alpha <= 0.0 || x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 3;
        let a = alpha.clamp(0.0, 1.0);
        for (channel, src) in self.pixels[i..i + 3].iter_mut().zip(colour.iter()) {
            let dst = f64::from(*channel);
            let src = f64::from(*src);
            *channel = (dst + (src - dst) * a).round().clamp(0.0, 255.0) as u8;
        }
    }

    /// Fill a simple polygon by even-odd scanline, with horizontal antialiasing.
    pub fn fill_polygon(&mut self, points: &[Px], colour: Rgb, alpha: f64) {
        let n = points.len();
        if n < 3 || self.height == 0 || self.width == 0 {
            return;
        }
        let mut ymin = f64::INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for p in points {
            ymin = ymin.min(p[1]);
            ymax = ymax.max(p[1]);
        }
        if ymax < 0.0 || ymin > self.height as f64 {
            return;
        }
        let y0 = ymin.floor().max(0.0) as usize;
        let y1 = (ymax.ceil().min((self.height - 1) as f64)).max(0.0) as usize;
        let mut crossings = std::mem::take(&mut self.crossings);
        for y in y0..=y1 {
            let sy = y as f64 + 0.5;
            crossings.clear();
            for i in 0..n {
                let a = points[i];
                let b = points[(i + 1) % n];
                let (lo, hi) = if a[1] < b[1] { (a, b) } else { (b, a) };
                if sy >= lo[1] && sy < hi[1] {
                    let t = (sy - lo[1]) / (hi[1] - lo[1]);
                    crossings.push(lo[0] + (hi[0] - lo[0]) * t);
                }
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(f64::total_cmp);
            let mut k = 0;
            while k + 1 < crossings.len() {
                let xa = crossings[k];
                let xb = crossings[k + 1];
                k += 2;
                // Exact area coverage: pixel `i` spans `[i, i + 1)`, and the
                // span covers `min(i + 1, xb) - max(i, xa)` of it. Getting this
                // off by half a pixel is what makes a downsampled edge bleed.
                let i0 = xa.floor() as i64;
                let i1 = xb.ceil() as i64 - 1;
                if i1 < i0 {
                    continue;
                }
                let mut paint = |x: i64, cover: f64| {
                    if x >= 0 && (x as usize) < self.width && cover > 0.0 {
                        self.blend(x as usize, y, colour, alpha * cover.min(1.0));
                    }
                };
                if i0 == i1 {
                    paint(i0, xb - xa);
                } else {
                    paint(i0, (i0 + 1) as f64 - xa);
                    for x in i0 + 1..i1 {
                        paint(x, 1.0);
                    }
                    paint(i1, xb - i1 as f64);
                }
            }
        }
        self.crossings = crossings;
    }

    /// Stroke a closed ring.
    pub fn stroke_polygon(&mut self, points: &[Px], width: f64, colour: Rgb, alpha: f64) {
        let n = points.len();
        if n < 2 {
            return;
        }
        for i in 0..n {
            self.segment(points[i], points[(i + 1) % n], width, colour, alpha);
        }
    }

    /// Stroke an open polyline, rounding the interior joins.
    pub fn polyline(&mut self, points: &[Px], width: f64, colour: Rgb, alpha: f64) {
        if points.len() < 2 {
            return;
        }
        for i in 0..points.len() - 1 {
            self.segment(points[i], points[i + 1], width, colour, alpha);
        }
        if width > 1.6 {
            for p in points.iter().skip(1).take(points.len().saturating_sub(2)) {
                self.disc(*p, width * 0.5, colour, alpha);
            }
        }
    }

    /// Stroke one segment as a quad.
    pub fn segment(&mut self, a: Px, b: Px, width: f64, colour: Rgb, alpha: f64) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let l = (dx * dx + dy * dy).sqrt();
        if l < 1e-9 {
            self.disc(a, width * 0.5, colour, alpha);
            return;
        }
        let nx = -dy / l * width * 0.5;
        let ny = dx / l * width * 0.5;
        let quad = [
            [a[0] + nx, a[1] + ny],
            [b[0] + nx, b[1] + ny],
            [b[0] - nx, b[1] - ny],
            [a[0] - nx, a[1] - ny],
        ];
        self.fill_polygon(&quad, colour, alpha);
    }

    /// Fill a disc, with a one-pixel feathered edge.
    pub fn disc(&mut self, centre: Px, radius: f64, colour: Rgb, alpha: f64) {
        if radius <= 0.0 {
            return;
        }
        let x0 = (centre[0] - radius - 1.0).floor().max(0.0) as usize;
        let x1 = ((centre[0] + radius + 1.0).ceil()).clamp(0.0, (self.width - 1) as f64) as usize;
        let y0 = (centre[1] - radius - 1.0).floor().max(0.0) as usize;
        let y1 = ((centre[1] + radius + 1.0).ceil()).clamp(0.0, (self.height - 1) as f64) as usize;
        let r2 = radius * radius;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - centre[0];
                let dy = y as f64 + 0.5 - centre[1];
                let d2 = dx * dx + dy * dy;
                if d2 <= r2 {
                    self.blend(x, y, colour, alpha);
                } else if d2 <= (radius + 1.0) * (radius + 1.0) {
                    let d = d2.sqrt();
                    self.blend(
                        x,
                        y,
                        colour,
                        alpha * (radius + 1.0 - d).clamp(0.0, 1.0) * 0.7,
                    );
                }
            }
        }
    }

    /// Fill an axis-aligned rectangle.
    pub fn rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, colour: Rgb, alpha: f64) {
        self.fill_polygon(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1]], colour, alpha);
    }

    /// Draw text in the built-in 5×7 font, `scale` pixels per font pixel.
    ///
    /// Text lives in a UI overlay in the interactive renderer (PRD §13); this is
    /// for the static plan, where there is no overlay to put it in.
    pub fn text(&mut self, x: f64, y: f64, text: &str, scale: f64, colour: Rgb) {
        let mut cx = x;
        for ch in text.chars() {
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (1 << (4 - col)) != 0 {
                        let px = cx + f64::from(col) * scale;
                        let py = y + row as f64 * scale;
                        self.rect(px, py, px + scale, py + scale, colour, 1.0);
                    }
                }
            }
            cx += 6.0 * scale;
        }
    }

    /// Width of a string in the built-in font.
    #[must_use]
    pub fn text_width(text: &str, scale: f64) -> f64 {
        text.chars().count() as f64 * 6.0 * scale
    }

    /// Box-downsample by an integer factor. This is the antialiasing.
    #[must_use]
    pub fn downsample(&self, factor: usize) -> Self {
        let factor = factor.max(1);
        let width = self.width / factor;
        let height = self.height / factor;
        let mut out = Self::new(width, height, [0, 0, 0]);
        let n = (factor * factor) as u32;
        for y in 0..height {
            for x in 0..width {
                let mut acc = [0u32; 3];
                for dy in 0..factor {
                    for dx in 0..factor {
                        let i = ((y * factor + dy) * self.width + (x * factor + dx)) * 3;
                        for (a, p) in acc.iter_mut().zip(self.pixels[i..i + 3].iter()) {
                            *a += u32::from(*p);
                        }
                    }
                }
                let o = (y * width + x) * 3;
                for (p, a) in out.pixels[o..o + 3].iter_mut().zip(acc.iter()) {
                    *p = (*a / n) as u8;
                }
            }
        }
        out
    }

    /// Write the canvas to a PNG file.
    pub fn write_png(&self, path: &Path) -> io::Result<()> {
        std::fs::write(path, self.encode_png())
    }

    /// Encode the canvas as PNG bytes.
    #[must_use]
    pub fn encode_png(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity(self.height * (1 + self.width * 3));
        for y in 0..self.height {
            raw.push(0u8); // filter: none
            let s = y * self.width * 3;
            raw.extend_from_slice(&self.pixels[s..s + self.width * 3]);
        }
        // zlib header, then stored deflate blocks: no compressor, no dependency,
        // and byte-identical output for identical pixels.
        let mut z = vec![0x78u8, 0x01];
        let mut i = 0;
        loop {
            let n = (raw.len() - i).min(65_535);
            let last = u8::from(i + n >= raw.len());
            z.push(last);
            z.extend_from_slice(&(n as u16).to_le_bytes());
            z.extend_from_slice(&(!(n as u16)).to_le_bytes());
            z.extend_from_slice(&raw[i..i + n]);
            i += n;
            if last == 1 {
                break;
            }
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut out: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(self.width as u32).to_be_bytes());
        ihdr.extend_from_slice(&(self.height as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour
        chunk(&mut out, *b"IHDR", &ihdr);
        chunk(&mut out, *b"IDAT", &z);
        chunk(&mut out, *b"IEND", &[]);
        out
    }
}

/// HSL to RGB, for the district hue families.
#[must_use]
pub fn hsl(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let h = hue.rem_euclid(1.0) * 6.0;
    let c = (1.0 - (2.0f64.mul_add(lightness, -1.0)).abs()) * saturation;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = c.mul_add(-0.5, lightness);
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Multiply a colour's channels, clamped.
#[must_use]
pub fn shade(colour: Rgb, factor: f64) -> Rgb {
    [
        (f64::from(colour[0]) * factor).clamp(0.0, 255.0) as u8,
        (f64::from(colour[1]) * factor).clamp(0.0, 255.0) as u8,
        (f64::from(colour[2]) * factor).clamp(0.0, 255.0) as u8,
    ]
}

fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &x in data {
        c = table[((c ^ u32::from(x)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// A 5×7 bitmap font. Each row is five bits, most significant on the left.
fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        '=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '%' => [0x19, 0x1A, 0x02, 0x04, 0x08, 0x0B, 0x13],
        '#' => [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '\'' => [0x04, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        '*' => [0x00, 0x0A, 0x04, 0x1F, 0x04, 0x0A, 0x00],
        _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filled_square_covers_the_pixels_it_should() {
        let mut c = Canvas::new(16, 16, [0, 0, 0]);
        c.fill_polygon(
            &[[4.0, 4.0], [12.0, 4.0], [12.0, 12.0], [4.0, 12.0]],
            [255, 255, 255],
            1.0,
        );
        let at = |x: usize, y: usize| c.pixels[(y * 16 + x) * 3];
        assert_eq!(at(8, 8), 255, "the middle is not filled");
        assert_eq!(at(1, 1), 0, "the outside was painted");
        assert_eq!(at(14, 14), 0);
    }

    #[test]
    fn the_png_header_and_chunks_are_well_formed() {
        let c = Canvas::new(4, 3, [10, 20, 30]);
        let png = c.encode_png();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &4u32.to_be_bytes());
        assert_eq!(&png[20..24], &3u32.to_be_bytes());
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "no IEND CRC");
    }

    #[test]
    fn encoding_is_a_pure_function_of_the_pixels() {
        let mut a = Canvas::new(24, 24, [1, 2, 3]);
        let mut b = Canvas::new(24, 24, [1, 2, 3]);
        for c in [&mut a, &mut b] {
            c.disc([12.0, 12.0], 6.0, [200, 100, 50], 0.8);
            c.text(1.0, 1.0, "POLIS", 1.0, [255, 255, 255]);
        }
        assert_eq!(a.encode_png(), b.encode_png());
    }

    #[test]
    fn downsampling_averages() {
        let mut c = Canvas::new(4, 4, [0, 0, 0]);
        c.rect(0.0, 0.0, 2.0, 4.0, [255, 255, 255], 1.0);
        let small = c.downsample(2);
        assert_eq!(small.width, 2);
        assert_eq!(small.pixels[0], 255);
        assert_eq!(small.pixels[3], 0);
    }

    #[test]
    fn hsl_hits_the_primaries() {
        assert_eq!(hsl(0.0, 1.0, 0.5), [255, 0, 0]);
        assert_eq!(hsl(1.0 / 3.0, 1.0, 0.5), [0, 255, 0]);
        assert_eq!(hsl(2.0 / 3.0, 1.0, 0.5), [0, 0, 255]);
        assert_eq!(hsl(0.0, 0.0, 0.0), [0, 0, 0]);
    }

    #[test]
    fn text_width_matches_what_text_draws() {
        let scale = 2.0;
        let mut c = Canvas::new(120, 20, [0, 0, 0]);
        c.text(0.0, 0.0, "AB", scale, [255, 255, 255]);
        let width = Canvas::text_width("AB", scale);
        assert!((width - 24.0).abs() < 1e-9, "{width}");
        // Nothing is drawn past the advance.
        let x = width as usize + 1;
        assert_eq!(c.pixels[(3 * 120 + x) * 3], 0);
    }
}
