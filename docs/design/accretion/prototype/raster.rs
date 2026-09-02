//! A tiny deterministic software rasteriser and PNG writer, so the diagnostic
//! render is reproducible from the same bits as the layout.

use crate::geom::P;

pub type Rgb = [u8; 3];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u8>,
    xs: Vec<f64>,
}

impl Canvas {
    pub fn new(w: usize, h: usize, bg: Rgb) -> Self {
        let mut buf = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            buf.extend_from_slice(&bg);
        }
        Self {
            w,
            h,
            buf,
            xs: Vec::new(),
        }
    }

    #[inline]
    fn blend(&mut self, x: usize, y: usize, c: Rgb, a: f64) {
        if a <= 0.0 {
            return;
        }
        let i = (y * self.w + x) * 3;
        let a = a.clamp(0.0, 1.0);
        for k in 0..3 {
            let dst = f64::from(self.buf[i + k]);
            let src = f64::from(c[k]);
            self.buf[i + k] = (dst + (src - dst) * a).round().clamp(0.0, 255.0) as u8;
        }
    }

    /// Even-odd scanline fill. Rings here are simple, so even-odd and nonzero
    /// agree.
    pub fn fill_poly(&mut self, pts: &[P], c: Rgb, alpha: f64) {
        let n = pts.len();
        if n < 3 {
            return;
        }
        let mut ymin = f64::INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for p in pts {
            ymin = ymin.min(p[1]);
            ymax = ymax.max(p[1]);
        }
        let y0 = (ymin.floor().max(0.0)) as usize;
        let y1 = (ymax.ceil().min((self.h - 1) as f64)).max(0.0) as usize;
        if ymax < 0.0 || ymin > self.h as f64 {
            return;
        }
        for y in y0..=y1 {
            let sy = y as f64 + 0.5;
            self.xs.clear();
            for i in 0..n {
                let a = pts[i];
                let b = pts[(i + 1) % n];
                let (lo, hi) = if a[1] < b[1] { (a, b) } else { (b, a) };
                if sy >= lo[1] && sy < hi[1] {
                    let t = (sy - lo[1]) / (hi[1] - lo[1]);
                    self.xs.push(lo[0] + (hi[0] - lo[0]) * t);
                }
            }
            if self.xs.len() < 2 {
                continue;
            }
            self.xs
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let xs = std::mem::take(&mut self.xs);
            let mut k = 0;
            while k + 1 < xs.len() {
                let xa = xs[k];
                let xb = xs[k + 1];
                k += 2;
                let ia = xa.ceil().max(0.0) as i64;
                let ib = xb.floor().min((self.w - 1) as f64) as i64;
                // Partial coverage at the two ends.
                if ia > 0 && (ia as f64 - xa) > 0.0 && ((ia - 1) as usize) < self.w {
                    self.blend((ia - 1) as usize, y, c, alpha * (ia as f64 - xa).min(1.0));
                }
                for x in ia..=ib {
                    if x >= 0 && (x as usize) < self.w {
                        self.blend(x as usize, y, c, alpha);
                    }
                }
                let nx = ib + 1;
                if nx >= 0 && (nx as usize) < self.w && (xb - ib as f64) > 0.0 {
                    self.blend(nx as usize, y, c, alpha * (xb - ib as f64).min(1.0));
                }
            }
            self.xs = xs;
            self.xs.clear();
        }
    }

    pub fn stroke_poly(&mut self, pts: &[P], w: f64, c: Rgb, alpha: f64) {
        let n = pts.len();
        if n < 2 {
            return;
        }
        for i in 0..n {
            self.segment(pts[i], pts[(i + 1) % n], w, c, alpha);
        }
    }

    pub fn polyline(&mut self, pts: &[P], w: f64, c: Rgb, alpha: f64) {
        if pts.len() < 2 {
            return;
        }
        for i in 0..pts.len() - 1 {
            self.segment(pts[i], pts[i + 1], w, c, alpha);
        }
        if w > 1.6 {
            for p in pts.iter().skip(1).take(pts.len().saturating_sub(2)) {
                self.disc(*p, w * 0.5, c, alpha);
            }
        }
    }

    pub fn segment(&mut self, a: P, b: P, w: f64, c: Rgb, alpha: f64) {
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let l = (dx * dx + dy * dy).sqrt();
        if l < 1e-9 {
            self.disc(a, w * 0.5, c, alpha);
            return;
        }
        let nx = -dy / l * w * 0.5;
        let ny = dx / l * w * 0.5;
        let quad = [
            [a[0] + nx, a[1] + ny],
            [b[0] + nx, b[1] + ny],
            [b[0] - nx, b[1] - ny],
            [a[0] - nx, a[1] - ny],
        ];
        self.fill_poly(&quad, c, alpha);
    }

    pub fn disc(&mut self, c0: P, r: f64, c: Rgb, alpha: f64) {
        if r <= 0.0 {
            return;
        }
        let x0 = (c0[0] - r).floor().max(0.0) as usize;
        let x1 = (c0[0] + r).ceil().min((self.w - 1) as f64).max(0.0) as usize;
        let y0 = (c0[1] - r).floor().max(0.0) as usize;
        let y1 = (c0[1] + r).ceil().min((self.h - 1) as f64).max(0.0) as usize;
        let r2 = r * r;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - c0[0];
                let dy = y as f64 + 0.5 - c0[1];
                let d2 = dx * dx + dy * dy;
                if d2 <= r2 {
                    self.blend(x, y, c, alpha);
                } else if d2 <= (r + 1.0) * (r + 1.0) {
                    let d = d2.sqrt();
                    self.blend(x, y, c, alpha * (r + 1.0 - d).clamp(0.0, 1.0) * 0.7);
                }
            }
        }
    }

    pub fn rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, c: Rgb, alpha: f64) {
        self.fill_poly(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1]], c, alpha);
    }

    /// 5x7 bitmap text, `s` pixels per font pixel.
    pub fn text(&mut self, x: f64, y: f64, txt: &str, s: f64, c: Rgb) {
        let mut cx = x;
        for ch in txt.chars() {
            let g = glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        self.rect(
                            cx + f64::from(col) * s,
                            y + row as f64 * s,
                            cx + f64::from(col) * s + s,
                            y + row as f64 * s + s,
                            c,
                            1.0,
                        );
                    }
                }
            }
            cx += 6.0 * s;
        }
    }

    pub fn text_width(txt: &str, s: f64) -> f64 {
        txt.chars().count() as f64 * 6.0 * s
    }

    /// Box-downsample by `k`.
    pub fn downsample(&self, k: usize) -> Canvas {
        let w = self.w / k;
        let h = self.h / k;
        let mut out = Canvas::new(w, h, [0, 0, 0]);
        let kk = (k * k) as u32;
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0u32; 3];
                for dy in 0..k {
                    for dx in 0..k {
                        let i = ((y * k + dy) * self.w + (x * k + dx)) * 3;
                        for c in 0..3 {
                            acc[c] += u32::from(self.buf[i + c]);
                        }
                    }
                }
                let o = (y * w + x) * 3;
                for c in 0..3 {
                    out.buf[o + c] = (acc[c] / kk) as u8;
                }
            }
        }
        out
    }

    pub fn write_png(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.encode_png())
    }

    pub fn encode_png(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity(self.h * (1 + self.w * 3));
        for y in 0..self.h {
            raw.push(0u8); // filter: none
            let s = y * self.w * 3;
            raw.extend_from_slice(&self.buf[s..s + self.w * 3]);
        }
        let mut z = vec![0x78u8, 0x01];
        let mut i = 0;
        while i < raw.len() {
            let n = (raw.len() - i).min(65535);
            let last = if i + n >= raw.len() { 1u8 } else { 0u8 };
            z.push(last);
            z.extend_from_slice(&(n as u16).to_le_bytes());
            z.extend_from_slice(&(!(n as u16)).to_le_bytes());
            z.extend_from_slice(&raw[i..i + n]);
            i += n;
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut out: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(self.w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(self.h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn adler32(d: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &x in d {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(d: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &x in d {
        c = table[((c ^ u32::from(x)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// 5x7 font. Each row is five bits, MSB left.
fn glyph(ch: char) -> [u8; 7] {
    let c = ch.to_ascii_uppercase();
    match c {
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
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '\'' => [0x04, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    }
}
