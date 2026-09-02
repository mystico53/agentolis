//! A tiny deterministic software rasteriser: scanline polygon fill, thick
//! polylines, discs, and a 5x7 bitmap font for the legend. 2x supersampled.

use std::cmp::Ordering;

use crate::geom::{pt, Pt};

pub type Rgb = [f64; 3];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub ss: usize,
    buf: Vec<f64>,
    // world -> device
    sx: f64,
    sy: f64,
    ox: f64,
    oy: f64,
}

impl Canvas {
    pub fn new(w: usize, h: usize, ss: usize, bg: Rgb) -> Self {
        let (bw, bh) = (w * ss, h * ss);
        let mut buf = vec![0.0; bw * bh * 3];
        for px in buf.chunks_mut(3) {
            px.copy_from_slice(&bg);
        }
        Canvas {
            w,
            h,
            ss,
            buf,
            sx: 1.0,
            sy: 1.0,
            ox: 0.0,
            oy: 0.0,
        }
    }

    pub fn fit(&mut self, lo: Pt, hi: Pt, margin_px: f64) {
        let bw = (self.w * self.ss) as f64;
        let bh = (self.h * self.ss) as f64;
        let m = margin_px * self.ss as f64;
        let s = ((bw - 2.0 * m) / (hi.x - lo.x).max(1e-6))
            .min((bh - 2.0 * m) / (hi.y - lo.y).max(1e-6));
        self.sx = s;
        self.sy = -s;
        self.ox = m + (bw - 2.0 * m - (hi.x - lo.x) * s) * 0.5 - lo.x * s;
        self.oy = bh - m - (bh - 2.0 * m - (hi.y - lo.y) * s) * 0.5 + lo.y * s;
    }

    pub fn scale(&self) -> f64 {
        self.sx
    }

    pub fn to_dev(&self, p: Pt) -> Pt {
        pt(p.x * self.sx + self.ox, p.y * self.sy + self.oy)
    }

    /// Device-space coordinates from pixel coordinates (for UI overlays).
    pub fn px(&self, x: f64, y: f64) -> Pt {
        pt(x * self.ss as f64, y * self.ss as f64)
    }

    #[inline]
    fn blend(&mut self, x: i64, y: i64, c: Rgb, a: f64) {
        let bw = (self.w * self.ss) as i64;
        let bh = (self.h * self.ss) as i64;
        if x < 0 || y < 0 || x >= bw || y >= bh || a <= 0.0 {
            return;
        }
        let i = ((y * bw + x) * 3) as usize;
        let a = a.min(1.0);
        for k in 0..3 {
            self.buf[i + k] = self.buf[i + k] * (1.0 - a) + c[k] * a;
        }
    }

    /// Even-odd scanline fill of a device-space polygon.
    fn fill_dev(&mut self, poly: &[Pt], c: Rgb, a: f64) {
        if poly.len() < 3 {
            return;
        }
        let bh = (self.h * self.ss) as i64;
        let mut ymin = f64::MAX;
        let mut ymax = f64::MIN;
        for p in poly {
            ymin = ymin.min(p.y);
            ymax = ymax.max(p.y);
        }
        let y0 = (ymin.floor() as i64).max(0);
        let y1 = (ymax.ceil() as i64).min(bh - 1);
        let mut xs: Vec<f64> = Vec::with_capacity(8);
        for y in y0..=y1 {
            let yc = y as f64 + 0.5;
            xs.clear();
            for i in 0..poly.len() {
                let p = poly[i];
                let q = poly[(i + 1) % poly.len()];
                if (p.y > yc) != (q.y > yc) {
                    let t = (yc - p.y) / (q.y - p.y);
                    xs.push(p.x + t * (q.x - p.x));
                }
            }
            xs.sort_by(|u, v| u.partial_cmp(v).unwrap_or(Ordering::Equal));
            let mut i = 0;
            while i + 1 < xs.len() {
                let (x0, x1) = (xs[i], xs[i + 1]);
                let a0 = x0.floor() as i64;
                let a1 = x1.ceil() as i64;
                for x in a0..=a1 {
                    let cov = ((x as f64 + 1.0).min(x1) - (x as f64).max(x0)).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        self.blend(x, y, c, a * cov);
                    }
                }
                i += 2;
            }
        }
    }

    pub fn fill(&mut self, poly: &[Pt], c: Rgb, a: f64) {
        let d: Vec<Pt> = poly.iter().map(|&p| self.to_dev(p)).collect();
        self.fill_dev(&d, c, a);
    }

    pub fn fill_px(&mut self, poly: &[Pt], c: Rgb, a: f64) {
        let d: Vec<Pt> = poly.iter().map(|&p| self.px(p.x, p.y)).collect();
        self.fill_dev(&d, c, a);
    }

    fn seg_dev(&mut self, a: Pt, b: Pt, w: f64, c: Rgb, al: f64) {
        let d = b.sub(a);
        let l = d.len();
        if l < 1e-9 {
            return;
        }
        let n = d.mul(1.0 / l).perp().mul(w * 0.5);
        let quad = [a.add(n), b.add(n), b.sub(n), a.sub(n)];
        self.fill_dev(&quad, c, al);
    }

    fn disc_dev(&mut self, ctr: Pt, r: f64, c: Rgb, a: f64) {
        // Fixed table rather than sin/cos: the PNG bytes must match across runs.
        const RING: [(f64, f64); 10] = [
            (1.0, 0.0),
            (0.809_016_994_374_947_4, 0.587_785_252_292_473_1),
            (0.309_016_994_374_947_45, 0.951_056_516_295_153_5),
            (-0.309_016_994_374_947_45, 0.951_056_516_295_153_5),
            (-0.809_016_994_374_947_4, 0.587_785_252_292_473_1),
            (-1.0, 0.0),
            (-0.809_016_994_374_947_4, -0.587_785_252_292_473_1),
            (-0.309_016_994_374_947_45, -0.951_056_516_295_153_5),
            (0.309_016_994_374_947_45, -0.951_056_516_295_153_5),
            (0.809_016_994_374_947_4, -0.587_785_252_292_473_1),
        ];
        let poly: Vec<Pt> = RING
            .iter()
            .map(|&(dx, dy)| pt(ctr.x + r * dx, ctr.y + r * dy))
            .collect();
        self.fill_dev(&poly, c, a);
    }

    pub fn disc(&mut self, ctr: Pt, r_px: f64, c: Rgb, a: f64) {
        let d = self.to_dev(ctr);
        self.disc_dev(d, r_px * self.ss as f64, c, a);
    }

    /// `w_px` is width in output pixels, independent of the world scale.
    pub fn polyline(&mut self, pts: &[Pt], w_px: f64, c: Rgb, a: f64, closed: bool) {
        if pts.len() < 2 {
            return;
        }
        let w = w_px * self.ss as f64;
        let d: Vec<Pt> = pts.iter().map(|&p| self.to_dev(p)).collect();
        let n = d.len();
        let last = if closed { n } else { n - 1 };
        for i in 0..last {
            self.seg_dev(d[i], d[(i + 1) % n], w, c, a);
        }
        if w > 1.6 {
            let stop = if closed { n } else { n - 1 };
            for &p in d.iter().take(stop + 1).skip(1) {
                self.disc_dev(p, w * 0.5, c, a);
            }
        }
    }

    /// A polyline whose width is given in *world* units, so a boulevard is
    /// genuinely wider than an alley on the ground rather than only in the ink.
    pub fn polyline_world(&mut self, pts: &[Pt], w_world: f64, c: Rgb, a: f64, min_px: f64) {
        let w_px = (w_world * self.sx / self.ss as f64).max(min_px);
        self.polyline(pts, w_px, c, a, false);
    }

    pub fn line_px(&mut self, a: Pt, b: Pt, w_px: f64, c: Rgb, al: f64) {
        let s = self.ss as f64;
        self.seg_dev(self.px(a.x, a.y), self.px(b.x, b.y), w_px * s, c, al);
    }

    /// Text in pixel coordinates, `s` pixels per font cell.
    pub fn text(&mut self, x: f64, y: f64, txt: &str, s: f64, c: Rgb, a: f64) {
        let ss = self.ss as f64;
        let mut cx = x;
        for ch in txt.chars() {
            let g = glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        let px0 = (cx + col as f64 * s) * ss;
                        let py0 = (y + row as f64 * s) * ss;
                        let q = [
                            pt(px0, py0),
                            pt(px0 + s * ss, py0),
                            pt(px0 + s * ss, py0 + s * ss),
                            pt(px0, py0 + s * ss),
                        ];
                        self.fill_dev(&q, c, a);
                    }
                }
            }
            cx += 6.0 * s;
        }
    }

    pub fn text_width(txt: &str, s: f64) -> f64 {
        txt.chars().count() as f64 * 6.0 * s
    }

    pub fn to_rgb8(&self) -> Vec<u8> {
        let ss = self.ss;
        let bw = self.w * ss;
        let mut out = vec![0u8; self.w * self.h * 3];
        let inv = 1.0 / (ss * ss) as f64;
        for y in 0..self.h {
            for x in 0..self.w {
                let mut acc = [0.0f64; 3];
                for dy in 0..ss {
                    for dx in 0..ss {
                        let i = (((y * ss + dy) * bw) + (x * ss + dx)) * 3;
                        for k in 0..3 {
                            acc[k] += self.buf[i + k];
                        }
                    }
                }
                let o = (y * self.w + x) * 3;
                for k in 0..3 {
                    out[o + k] = ((acc[k] * inv).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
        }
        out
    }
}

pub fn write_png(path: &str, w: usize, h: usize, rgb: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let bw = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(bw, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Balanced);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgb)?;
    Ok(())
}

/// Deterministic HSV -> RGB, h in [0,1).
pub fn hsv(h: f64, s: f64, v: f64) -> Rgb {
    let h = (h - h.floor()) * 6.0;
    let i = h.floor();
    let f = h - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i as i64 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

// ---------------------------------------------------------------------------
// 5x7 font
// ---------------------------------------------------------------------------

#[rustfmt::skip]
fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0x0E,0x11,0x11,0x1F,0x11,0x11,0x11],
        'B' => [0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E],
        'C' => [0x0E,0x11,0x10,0x10,0x10,0x11,0x0E],
        'D' => [0x1E,0x11,0x11,0x11,0x11,0x11,0x1E],
        'E' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F],
        'F' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x10],
        'G' => [0x0E,0x11,0x10,0x17,0x11,0x11,0x0F],
        'H' => [0x11,0x11,0x11,0x1F,0x11,0x11,0x11],
        'I' => [0x0E,0x04,0x04,0x04,0x04,0x04,0x0E],
        'J' => [0x07,0x02,0x02,0x02,0x02,0x12,0x0C],
        'K' => [0x11,0x12,0x14,0x18,0x14,0x12,0x11],
        'L' => [0x10,0x10,0x10,0x10,0x10,0x10,0x1F],
        'M' => [0x11,0x1B,0x15,0x15,0x11,0x11,0x11],
        'N' => [0x11,0x19,0x15,0x13,0x11,0x11,0x11],
        'O' => [0x0E,0x11,0x11,0x11,0x11,0x11,0x0E],
        'P' => [0x1E,0x11,0x11,0x1E,0x10,0x10,0x10],
        'Q' => [0x0E,0x11,0x11,0x11,0x15,0x12,0x0D],
        'R' => [0x1E,0x11,0x11,0x1E,0x14,0x12,0x11],
        'S' => [0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E],
        'T' => [0x1F,0x04,0x04,0x04,0x04,0x04,0x04],
        'U' => [0x11,0x11,0x11,0x11,0x11,0x11,0x0E],
        'V' => [0x11,0x11,0x11,0x11,0x11,0x0A,0x04],
        'W' => [0x11,0x11,0x11,0x15,0x15,0x1B,0x11],
        'X' => [0x11,0x11,0x0A,0x04,0x0A,0x11,0x11],
        'Y' => [0x11,0x11,0x0A,0x04,0x04,0x04,0x04],
        'Z' => [0x1F,0x01,0x02,0x04,0x08,0x10,0x1F],
        '0' => [0x0E,0x11,0x13,0x15,0x19,0x11,0x0E],
        '1' => [0x04,0x0C,0x04,0x04,0x04,0x04,0x0E],
        '2' => [0x0E,0x11,0x01,0x02,0x04,0x08,0x1F],
        '3' => [0x1F,0x02,0x04,0x02,0x01,0x11,0x0E],
        '4' => [0x02,0x06,0x0A,0x12,0x1F,0x02,0x02],
        '5' => [0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E],
        '6' => [0x06,0x08,0x10,0x1E,0x11,0x11,0x0E],
        '7' => [0x1F,0x01,0x02,0x04,0x08,0x08,0x08],
        '8' => [0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E],
        '9' => [0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C],
        '-' => [0x00,0x00,0x00,0x1F,0x00,0x00,0x00],
        '.' => [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C],
        ',' => [0x00,0x00,0x00,0x00,0x0C,0x04,0x08],
        ':' => [0x00,0x0C,0x0C,0x00,0x0C,0x0C,0x00],
        '/' => [0x01,0x01,0x02,0x04,0x08,0x10,0x10],
        '(' => [0x02,0x04,0x08,0x08,0x08,0x04,0x02],
        ')' => [0x08,0x04,0x02,0x02,0x02,0x04,0x08],
        '%' => [0x11,0x12,0x02,0x04,0x08,0x09,0x11],
        '+' => [0x00,0x04,0x04,0x1F,0x04,0x04,0x00],
        '=' => [0x00,0x00,0x1F,0x00,0x1F,0x00,0x00],
        '#' => [0x0A,0x1F,0x0A,0x0A,0x1F,0x0A,0x00],
        '>' => [0x08,0x04,0x02,0x01,0x02,0x04,0x08],
        '<' => [0x02,0x04,0x08,0x10,0x08,0x04,0x02],
        '_' => [0x00,0x00,0x00,0x00,0x00,0x00,0x1F],
        '|' => [0x04,0x04,0x04,0x04,0x04,0x04,0x04],
        _   => [0x00,0x00,0x00,0x00,0x00,0x00,0x00],
    }
}
