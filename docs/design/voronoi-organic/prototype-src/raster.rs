//! Supersampled RGB canvas: polygon fill, thick polylines, discs, 5x7 text.

pub const SS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P2 {
    pub x: f64,
    pub y: f64,
}
impl P2 {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

pub type Rgb = [u8; 3];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    buf: Vec<u8>, // internal, w*SS x h*SS
}

impl Canvas {
    pub fn new(w: usize, h: usize, bg: Rgb) -> Self {
        let n = w * SS * h * SS;
        let mut buf = Vec::with_capacity(n * 3);
        for _ in 0..n {
            buf.extend_from_slice(&bg);
        }
        Self { w, h, buf }
    }

    #[inline]
    fn iw(&self) -> usize {
        self.w * SS
    }
    #[inline]
    fn ih(&self) -> usize {
        self.h * SS
    }

    #[inline]
    fn blend(&mut self, x: i64, y: i64, c: Rgb, a: f64) {
        if x < 0 || y < 0 || x as usize >= self.iw() || y as usize >= self.ih() {
            return;
        }
        let i = (y as usize * self.iw() + x as usize) * 3;
        if a >= 1.0 {
            self.buf[i] = c[0];
            self.buf[i + 1] = c[1];
            self.buf[i + 2] = c[2];
        } else {
            for k in 0..3 {
                let old = f64::from(self.buf[i + k]);
                let new = f64::from(c[k]);
                self.buf[i + k] = (old + (new - old) * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// Nonzero-winding scanline fill. Points are in output-pixel coordinates.
    pub fn fill_poly(&mut self, pts: &[P2], c: Rgb, alpha: f64) {
        if pts.len() < 3 || alpha <= 0.0 {
            return;
        }
        let s = SS as f64;
        let (mut miny, mut maxy) = (f64::MAX, f64::MIN);
        for p in pts {
            miny = miny.min(p.y * s);
            maxy = maxy.max(p.y * s);
        }
        let y0 = (miny.floor() as i64).max(0);
        let y1 = (maxy.ceil() as i64).min(self.ih() as i64 - 1);
        let mut xs: Vec<(f64, i32)> = Vec::with_capacity(8);
        for y in y0..=y1 {
            let sy = y as f64 + 0.5;
            xs.clear();
            for i in 0..pts.len() {
                let a = pts[i];
                let b = pts[(i + 1) % pts.len()];
                let (ax, ay) = (a.x * s, a.y * s);
                let (bx, by) = (b.x * s, b.y * s);
                if (ay <= sy && by > sy) || (by <= sy && ay > sy) {
                    let t = (sy - ay) / (by - ay);
                    xs.push((ax + (bx - ax) * t, if by > ay { 1 } else { -1 }));
                }
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut wind = 0;
            for k in 0..xs.len().saturating_sub(1) {
                wind += xs[k].1;
                if wind != 0 {
                    let xa = xs[k].0.ceil() as i64;
                    let xb = xs[k + 1].0.floor() as i64;
                    for x in xa..=xb {
                        self.blend(x, y, c, alpha);
                    }
                }
            }
        }
    }

    pub fn disc(&mut self, at: P2, r: f64, c: Rgb, alpha: f64) {
        let s = SS as f64;
        let (cx, cy) = (at.x * s, at.y * s);
        let rr = r * s;
        let x0 = (cx - rr).floor() as i64;
        let x1 = (cx + rr).ceil() as i64;
        let y0 = (cy - rr).floor() as i64;
        let y1 = (cy + rr).ceil() as i64;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - cx;
                let dy = y as f64 + 0.5 - cy;
                if dx * dx + dy * dy <= rr * rr {
                    self.blend(x, y, c, alpha);
                }
            }
        }
    }

    pub fn line(&mut self, a: P2, b: P2, width: f64, c: Rgb, alpha: f64) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-9 {
            self.disc(a, width * 0.5, c, alpha);
            return;
        }
        let (nx, ny) = (-dy / len * width * 0.5, dx / len * width * 0.5);
        let quad = [
            P2::new(a.x + nx, a.y + ny),
            P2::new(b.x + nx, b.y + ny),
            P2::new(b.x - nx, b.y - ny),
            P2::new(a.x - nx, a.y - ny),
        ];
        self.fill_poly(&quad, c, alpha);
    }

    pub fn polyline(&mut self, pts: &[P2], width: f64, c: Rgb, alpha: f64) {
        if pts.len() < 2 {
            return;
        }
        for i in 0..pts.len() - 1 {
            self.line(pts[i], pts[i + 1], width, c, alpha);
        }
        if width > 1.2 {
            for p in pts.iter().skip(1).take(pts.len().saturating_sub(2)) {
                self.disc(*p, width * 0.5, c, alpha);
            }
        }
    }

    pub fn outline(&mut self, pts: &[P2], width: f64, c: Rgb, alpha: f64) {
        if pts.len() < 2 {
            return;
        }
        for i in 0..pts.len() {
            self.line(pts[i], pts[(i + 1) % pts.len()], width, c, alpha);
        }
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgb, alpha: f64) {
        self.fill_poly(
            &[
                P2::new(x, y),
                P2::new(x + w, y),
                P2::new(x + w, y + h),
                P2::new(x, y + h),
            ],
            c,
            alpha,
        );
    }

    /// 5x7 bitmap text, `scale` output-pixels per font pixel.
    pub fn text(&mut self, x: f64, y: f64, s: &str, scale: f64, c: Rgb) {
        let mut cx = x;
        for ch in s.chars() {
            let g = crate::font::glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (1 << (4 - col)) != 0 {
                        self.rect(
                            cx + f64::from(col) * scale,
                            y + row as f64 * scale,
                            scale,
                            scale,
                            c,
                            1.0,
                        );
                    }
                }
            }
            cx += 6.0 * scale;
        }
    }

    pub fn text_width(s: &str, scale: f64) -> f64 {
        s.chars().count() as f64 * 6.0 * scale
    }

    /// Box-downsample to output resolution.
    pub fn to_rgb(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.w * self.h * 3];
        let iw = self.iw();
        let n = (SS * SS) as u32;
        for y in 0..self.h {
            for x in 0..self.w {
                let mut acc = [0u32; 3];
                for sy in 0..SS {
                    for sx in 0..SS {
                        let i = ((y * SS + sy) * iw + (x * SS + sx)) * 3;
                        acc[0] += u32::from(self.buf[i]);
                        acc[1] += u32::from(self.buf[i + 1]);
                        acc[2] += u32::from(self.buf[i + 2]);
                    }
                }
                let o = (y * self.w + x) * 3;
                for k in 0..3 {
                    out[o + k] = (acc[k] / n) as u8;
                }
            }
        }
        out
    }
}
