//! Diagnostic renders. Legibility of the structure beats beauty here: blocks,
//! lots and buildings must be told apart at a glance.

use polis_layout::determinism as det;

use crate::city::{City, RoadClass, NOLABEL, WORLD};
use crate::geom;
use crate::raster::{Canvas, Rgb, P2};

pub const MAP: usize = 1800;
pub const INFO: usize = 210;
const MARGIN: f64 = 26.0;

fn hsv(h: f64, s: f64, v: f64) -> Rgb {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

struct View {
    scale: f64,
}
impl View {
    fn new() -> Self {
        Self {
            scale: (MAP as f64 - 2.0 * MARGIN) / WORLD,
        }
    }
    fn p(&self, p: geom::Pt) -> P2 {
        P2::new(MARGIN + p.x * self.scale, MARGIN + p.y * self.scale)
    }
    fn poly(&self, ps: &[geom::Pt]) -> Vec<P2> {
        ps.iter().map(|p| self.p(*p)).collect()
    }
}

/// Quarters of the same top-level tree share a hue family, and only shift a
/// little within it. Colour then carries "which part of the repo am I in",
/// which is the wayfinding job PRD 8 gives the district layer.
fn district_hue(key: &str) -> f64 {
    let top = key.split('/').next().unwrap_or(key);
    let base = (det::fnv1a64_str(top) % 3600) as f64 / 10.0;
    let off = (det::fnv1a64_str(key) % 260) as f64 / 10.0 - 13.0;
    base + off
}

pub fn render_city(c: &City, title: &str, info: &[String]) -> (usize, usize, Vec<u8>) {
    let v = View::new();
    let h = MAP + INFO;
    let mut cv = Canvas::new(MAP, h, [7, 10, 16]);

    // ---- ground -----------------------------------------------------
    let res = crate::city::RES;
    let cell = crate::city::CELL;
    let px = cell * v.scale;
    for j in 0..res {
        for i in 0..res {
            let k = j * res + i;
            if !c.land[k] {
                continue;
            }
            let col: Rgb = if c.dlabel[k] == NOLABEL {
                [13, 17, 15]
            } else {
                continue;
            };
            let p = v.p(geom::pt(i as f64 * cell, j as f64 * cell));
            cv.rect(p.x, p.y, px + 0.6, px + 0.6, col, 1.0);
        }
    }

    // ---- blocks -----------------------------------------------------
    for b in &c.blocks {
        if b.poly.len() < 3 {
            continue;
        }
        let d = &c.districts[b.district as usize];
        let hue = district_hue(&d.key);
        if b.files.is_empty() {
            // Unbuilt block: a square, a garden, a yard. Open ground.
            cv.fill_poly(&v.poly(&b.poly), hsv(hue * 0.10 + 74.0, 0.20, 0.235), 1.0);
        } else {
            let val = 0.11 + 0.25 * d.age_rank + 0.010 * ((b.rank % 3) as f64);
            cv.fill_poly(&v.poly(&b.poly), hsv(hue, 0.40, val), 1.0);
        }
    }

    // ---- lots -------------------------------------------------------
    for b in &c.blocks {
        for lot in &b.lots {
            if lot.len() >= 3 {
                cv.fill_poly(&v.poly(lot), [40, 42, 50], 0.62);
                cv.outline(&v.poly(lot), 1.2, [118, 126, 140], 0.85);
            }
        }
    }

    // ---- buildings --------------------------------------------------
    let big = c.repo.files.len() < 800;
    for bl in &c.buildings {
        if bl.poly.len() < 3 {
            continue;
        }
        let f = &c.repo.files[bl.file as usize];
        let warm = (det::fnv1a64_str(&f.path) % 40) as f64;
        cv.fill_poly(&v.poly(&bl.poly), hsv(34.0 + warm * 0.7, 0.19, 0.90), 1.0);
        if big {
            cv.outline(&v.poly(&bl.poly), 1.0, [70, 64, 56], 0.9);
        }
    }

    // ---- roads ------------------------------------------------------
    for ch in &c.chains {
        if !ch.alive || ch.class != RoadClass::Secondary {
            continue;
        }
        cv.polyline(&v.poly(&ch.pts), 1.5, [128, 133, 143], 0.85);
    }
    for ch in &c.chains {
        if !ch.alive || ch.class != RoadClass::Arterial {
            continue;
        }
        cv.polyline(&v.poly(&ch.pts), 3.6, [214, 208, 190], 0.96);
    }
    for ch in &c.chains {
        if !ch.alive || ch.class != RoadClass::Perimeter {
            continue;
        }
        cv.polyline(&v.poly(&ch.pts), 2.4, [104, 122, 108], 0.9);
    }

    // ---- streets (cross-district imports, routed on the network) ----
    for (path, count) in &c.street_paths {
        let w = (1.6 + (f64::from(*count)).sqrt() * 1.1).min(7.0);
        cv.polyline(&v.poly(path), w, [46, 190, 186], 0.55);
    }

    // ---- district labels: the wayfinding layer ----------------------
    let mut named: Vec<(&crate::city::DistrictData, u32)> =
        c.districts.iter().map(|d| (d, d.px)).collect();
    named.sort_by(|a, b| b.1.cmp(&a.1));
    let mut boxes: Vec<(f64, f64, f64, f64)> = Vec::new();
    for (d, _) in named.iter().take(if c.repo.files.len() > 800 { 46 } else { 30 }) {
        let name = d.key.trim_end_matches("/·");
        let name = if name.is_empty() { "/" } else { name };
        // `src` alone is not wayfinding; qualify a generic leaf with its parent.
        let parts: Vec<&str> = name.split('/').collect();
        let leaf = parts.last().copied().unwrap_or(name);
        let short: String = if parts.len() > 1
            && matches!(leaf, "src" | "tests" | "lib" | "bin" | "test" | "internal")
        {
            format!("{}/{leaf}", parts[parts.len() - 2]).to_uppercase()
        } else {
            leaf.to_uppercase()
        };
        let p = v.p(d.centroid);
        let w = Canvas::text_width(&short, 2.6);
        let bx = (p.x - w * 0.5 - 4.0, p.y - 12.0, w + 8.0, 22.0);
        // Overlapping labels are worse than missing ones for wayfinding.
        if boxes.iter().any(|b| {
            bx.0 < b.0 + b.2 && b.0 < bx.0 + bx.2 && bx.1 < b.1 + b.3 && b.1 < bx.1 + bx.3
        }) {
            continue;
        }
        boxes.push(bx);
        cv.rect(bx.0, bx.1, bx.2, bx.3, [8, 10, 14], 0.66);
        cv.text(p.x - w * 0.5, p.y - 9.0, &short, 2.6, [232, 236, 240]);
    }

    draw_legend(&mut cv, 26.0, MAP as f64 - 178.0, &legend_city());
    draw_info(&mut cv, title, info, MAP as f64);
    (MAP, h, cv.to_rgb())
}

pub fn render_junctions(c: &City, title: &str, info: &[String]) -> (usize, usize, Vec<u8>) {
    use std::collections::BTreeMap;
    let v = View::new();
    let h = MAP + INFO;
    let mut cv = Canvas::new(MAP, h, [8, 9, 13]);

    let mut deg: BTreeMap<u32, u32> = BTreeMap::new();
    for ch in &c.chains {
        if !ch.alive {
            continue;
        }
        *deg.entry(ch.a).or_insert(0) += 1;
        *deg.entry(ch.b).or_insert(0) += 1;
    }
    for ch in &c.chains {
        if !ch.alive {
            continue;
        }
        let col: Rgb = match ch.class {
            RoadClass::Arterial => [110, 118, 132],
            RoadClass::Secondary => [58, 64, 76],
            RoadClass::Perimeter => [48, 62, 52],
        };
        cv.polyline(&v.poly(&ch.pts), 1.4, col, 1.0);
    }
    for (n, d) in &deg {
        let col: Rgb = match d {
            1 => [255, 60, 200],
            2 => [90, 110, 255],
            3 => [70, 190, 235],
            4 => [255, 176, 40],
            _ => [255, 64, 48],
        };
        let r = match d {
            1 => 4.5,
            2 => 3.0,
            3 => 2.4,
            4 => 4.0,
            _ => 5.2,
        };
        cv.disc(v.p(c.jpos[*n as usize]), r, col, 1.0);
    }
    draw_legend(
        &mut cv,
        26.0,
        MAP as f64 - 148.0,
        &[
            ("DEG 1 - DANGLING END", [255, 60, 200]),
            ("DEG 2", [90, 110, 255]),
            ("DEG 3 - T JUNCTION", [70, 190, 235]),
            ("DEG 4 - CROSSROADS", [255, 176, 40]),
            ("DEG 5+ - IRREGULAR", [255, 64, 48]),
        ],
    );
    draw_info(&mut cv, title, info, MAP as f64);
    (MAP, h, cv.to_rgb())
}

fn draw_info(cv: &mut Canvas, title: &str, info: &[String], w: f64) {
    let top = MAP as f64;
    cv.rect(0.0, top, w, INFO as f64, [14, 16, 21], 1.0);
    cv.rect(0.0, top, w, 2.0, [70, 78, 92], 1.0);
    cv.text(24.0, top + 16.0, title, 3.0, [236, 240, 245]);
    for (i, l) in info.iter().enumerate().take(9) {
        cv.text(
            24.0,
            top + 48.0 + i as f64 * 17.0,
            l,
            1.9,
            [172, 182, 196],
        );
    }
}

pub fn legend_city() -> Vec<(&'static str, Rgb)> {
    vec![
        ("ARTERIAL ROAD = DISTRICT BORDER", [214, 208, 190]),
        ("SECONDARY STREET = BLOCK BORDER", [128, 133, 143]),
        ("CITY LIMIT", [104, 122, 108]),
        ("BLOCK FILL = DISTRICT (DARKER = OLDER)", [64, 58, 74]),
        ("UNBUILT BLOCK = OPEN GROUND", [32, 44, 32]),
        ("LOT OUTLINE", [96, 104, 118]),
        ("BUILDING = FILE", [224, 214, 196]),
        ("STREET = CROSS-DISTRICT IMPORTS", [46, 190, 186]),
    ]
}

pub fn draw_legend(cv: &mut Canvas, x: f64, y: f64, items: &[(&str, Rgb)]) {
    let rows = items.len();
    let wmax = items
        .iter()
        .map(|(t, _)| Canvas::text_width(t, 1.9))
        .fold(0.0f64, f64::max);
    cv.rect(x - 8.0, y - 8.0, wmax + 46.0, rows as f64 * 20.0 + 14.0, [10, 12, 17], 0.86);
    for (i, (t, c)) in items.iter().enumerate() {
        let yy = y + i as f64 * 20.0;
        cv.rect(x, yy + 2.0, 22.0, 10.0, *c, 1.0);
        cv.text(x + 30.0, yy + 1.0, t, 1.9, [198, 206, 218]);
    }
}
