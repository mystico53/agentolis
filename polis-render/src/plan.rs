//! The static city plan: a repository rendered to a PNG (PRD §15 M1).
//!
//! This is the product path from a repository to an image. `polis snapshot` runs
//! it; the M1 gate no longer needs a harness of its own.
//!
//! # The presentation scheme
//!
//! Taken from the design bake-off's `voronoi-organic` packet, which lost on
//! structure and won on presentation:
//!
//! * **Hue families keyed on the top-level directory.** A package reads as one
//!   colour family whatever its internal depth, so `web/store/session` and
//!   `web/ui/panel` are visibly the same neighbourhood. Saturation and lightness
//!   carry depth; hue carries the package.
//! * **In-map district labels.** PRD §8 asks the district skeleton to stay
//!   readable at every zoom, and a label is the cheapest way to make it so.
//! * **A drawn city limit.** An outline the eye can trace tells you where the
//!   settlement stops, which a fading fringe does not.
//! * **A metrics footer**, from `accretion`'s renders: the numbers that decide
//!   whether the picture is a city or a diagram, printed on the picture.
//!
//! # Nothing here reads a clock
//!
//! The footer prints structure, never a timing. A wall-clock number in the image
//! would make the PNG non-reproducible while the layout underneath was perfect —
//! which is exactly the class of leak PRD §7.4 exists to prevent, and it is a
//! mistake the bake-off actually made. Timings go to stdout.

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

use std::collections::{BTreeMap, BTreeSet};

use polis_events::LogicalPath;
use polis_layout::city::{City, Structure};
use polis_layout::{Point, RoadClass};

use crate::raster::{hsl, shade, Canvas, Px, Rgb};

/// Background.
const BG: Rgb = [12, 13, 17];
/// Widest road.
const ROAD_HI: Rgb = [236, 235, 228];
/// Middle road.
const ROAD_MID: Rgb = [196, 198, 200];
/// Narrowest road.
const ROAD_LO: Rgb = [142, 147, 156];
/// Lot boundary.
const LOTLINE: Rgb = [96, 102, 112];
/// Block keyline.
const BLOCKLINE: Rgb = [16, 17, 21];
/// Ordinary building.
const BLDG: Rgb = [238, 233, 220];
/// Monument (PRD §8).
const MONU: Rgb = [255, 206, 84];
/// Industrial mass (PRD §8).
const INDUS: Rgb = [92, 96, 104];
/// Import route (PRD §9).
const STREET: Rgb = [92, 232, 216];
/// District and package border.
const BORDER: Rgb = [255, 246, 214];
/// The drawn city limit.
const LIMIT: Rgb = [122, 138, 168];
/// Label plate.
const PLATE: Rgb = [8, 9, 12];
/// Label text.
const LABEL: Rgb = [232, 232, 236];

/// How many district labels the map carries at most.
const MAX_LABELS: usize = 18;

/// A world-to-pixel transform that fits a bounding box into a canvas.
#[derive(Debug, Clone, Copy)]
pub struct View {
    scale: f64,
    offset_x: f64,
    offset_y: f64,
    height: f64,
}

impl View {
    /// Fit a world bounding box into `width × height` pixels with a margin.
    #[must_use]
    pub fn fit(lo: [f64; 2], hi: [f64; 2], width: usize, height: usize, margin: f64) -> Self {
        let dw = (hi[0] - lo[0]).max(1e-6);
        let dh = (hi[1] - lo[1]).max(1e-6);
        let scale = ((width as f64 - 2.0 * margin) / dw).min((height as f64 - 2.0 * margin) / dh);
        Self {
            scale,
            offset_x: margin - lo[0] * scale + ((width as f64 - 2.0 * margin) - dw * scale) * 0.5,
            offset_y: margin - lo[1] * scale + ((height as f64 - 2.0 * margin) - dh * scale) * 0.5,
            height: height as f64,
        }
    }

    /// World point to pixel. The world's `y` points up; the image's points down.
    #[must_use]
    pub fn at(&self, p: Point) -> Px {
        [
            f64::from(p.x) * self.scale + self.offset_x,
            self.height - (f64::from(p.y) * self.scale + self.offset_y),
        ]
    }

    /// Project a whole ring.
    #[must_use]
    pub fn ring(&self, points: &[Point]) -> Vec<Px> {
        points.iter().map(|p| self.at(*p)).collect()
    }

    /// Pixels per world unit.
    #[must_use]
    pub fn scale(&self) -> f64 {
        self.scale
    }
}

/// Hue by top-level directory, lightness by depth: the directory tree, as colour.
///
/// Adjacent quarters of the same package read as one family, which is what makes
/// a package legible at a zoom where its individual districts are not.
#[must_use]
pub fn district_colours(city: &City) -> BTreeMap<LogicalPath, Rgb> {
    let mut tops: Vec<&str> = city
        .layout
        .districts
        .keys()
        .map(|p| p.components().next().unwrap_or(""))
        .collect();
    tops.sort_unstable();
    tops.dedup();
    let n = tops.len().max(1) as f64;
    let industrial: BTreeSet<&LogicalPath> = city.industrial.iter().map(|m| &m.host).collect();

    let mut out = BTreeMap::new();
    for path in city.layout.districts.keys() {
        let top = path.components().next().unwrap_or("");
        let index = tops.iter().position(|t| *t == top).unwrap_or(0) as f64;
        // A per-district nudge inside the family, so siblings are told apart
        // without leaving the package's hue.
        let nudge = ((fnv1a64(path.as_str().as_bytes()) >> 11) & 0xFFFF) as f64 / 65_535.0;
        let hue = index / n + (nudge - 0.5) * (0.34 / n);
        let depth = path.depth() as f64;
        let dull = industrial.contains(path);
        // Low saturation and low lightness: the ground is a tint, not a
        // statement. The roads and the buildings carry the reading.
        let saturation = if dull {
            0.03
        } else {
            (0.36 - depth * 0.020).clamp(0.17, 0.38)
        };
        let lightness = if dull {
            0.17
        } else {
            (0.19 + nudge * 0.030 + depth * 0.010).clamp(0.15, 0.27)
        };
        out.insert(path.clone(), hsl(hue, saturation, lightness));
    }
    out
}

/// FNV-1a, so the colour nudge is stable without depending on the layout crate's
/// internals.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The bounding box of every block.
fn world_bounds(city: &City) -> ([f64; 2], [f64; 2]) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for block in &city.layout.blocks {
        for v in &block.boundary.vertices {
            lo[0] = lo[0].min(f64::from(v.x));
            lo[1] = lo[1].min(f64::from(v.y));
            hi[0] = hi[0].max(f64::from(v.x));
            hi[1] = hi[1].max(f64::from(v.y));
        }
    }
    if !lo[0].is_finite() {
        let e = f64::from(city.layout.extent).max(1.0);
        return ([-e, -e], [e, e]);
    }
    (lo, hi)
}

/// Which road segments separate two different districts, and which two different
/// top-level packages.
///
/// A district border in this city **is** a street — the districts tile the block
/// graph, so a border is exactly the set of road edges with a different district
/// on each side. Recovered here by matching block ring edges back onto road
/// nodes, so the renderer needs nothing the layout does not already publish.
fn borders(city: &City) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let key = |p: Point| -> (i64, i64) {
        (
            (f64::from(p.x) * 1_000.0).round() as i64,
            (f64::from(p.y) * 1_000.0).round() as i64,
        )
    };
    let mut node_at: BTreeMap<(i64, i64), u32> = BTreeMap::new();
    for (i, n) in city.layout.roads.nodes.iter().enumerate() {
        node_at.insert(key(n.position), u32::try_from(i).unwrap_or(u32::MAX));
    }
    let mut segment_at: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for (i, s) in city.layout.roads.segments.iter().enumerate() {
        segment_at.insert((s.from.0.min(s.to.0), s.from.0.max(s.to.0)), i);
    }
    let mut sides: BTreeMap<usize, BTreeSet<LogicalPath>> = BTreeMap::new();
    for block in &city.layout.blocks {
        let ring = &block.boundary.vertices;
        for i in 0..ring.len() {
            let a = node_at.get(&key(ring[i]));
            let b = node_at.get(&key(ring[(i + 1) % ring.len()]));
            if let (Some(&a), Some(&b)) = (a, b) {
                if let Some(&e) = segment_at.get(&(a.min(b), a.max(b))) {
                    sides.entry(e).or_default().insert(block.district.clone());
                }
            }
        }
    }
    let mut district = BTreeSet::new();
    let mut package = BTreeSet::new();
    for (e, set) in sides {
        if set.len() < 2 {
            continue;
        }
        district.insert(e);
        let tops: BTreeSet<&str> = set
            .iter()
            .map(|p| p.components().next().unwrap_or(""))
            .collect();
        if tops.len() > 1 {
            package.insert(e);
        }
    }
    (district, package)
}

/// The convex hull of the settled ground: the drawn city limit.
fn city_limit(city: &City) -> Vec<Point> {
    let mut pts: Vec<Point> = city
        .layout
        .blocks
        .iter()
        .flat_map(|b| b.boundary.vertices.iter().copied())
        .collect();
    if pts.len() < 3 {
        return pts;
    }
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then_with(|| a.y.total_cmp(&b.y)));
    pts.dedup_by(|a, b| a.x == b.x && a.y == b.y);
    let turn = |o: Point, a: Point, b: Point| -> f64 {
        let ox = f64::from(o.x);
        let oy = f64::from(o.y);
        (f64::from(a.x) - ox) * (f64::from(b.y) - oy)
            - (f64::from(a.y) - oy) * (f64::from(b.x) - ox)
    };
    let mut lower: Vec<Point> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && turn(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Point> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && turn(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Render the city plan.
///
/// `supersample` is the integer factor the image is drawn at before being box-
/// filtered down; 2 is the useful setting and 1 is for tests.
#[must_use]
#[allow(clippy::too_many_lines)] // one layer per paragraph; splitting hides the order
/// Draws the city plan.
///
/// `streets` turns on PRD §9's import layer. **Off at this zoom by default**:
///
/// > Streets are a toggleable layer, off by default at the widest zoom.
///
/// This is the whole-city view, and the import graph drawn over the whole city
/// at once is the thing the first render got wrong — a mat of lines over the
/// plan that reads as noise rather than as information. The layer is drawn on
/// request (`polis snapshot --streets`), and when it is drawn it follows the
/// roads rather than cutting across the ground.
pub fn render_plan(
    city: &City,
    structure: &Structure,
    title: &str,
    pixels: usize,
    supersample: usize,
    streets: bool,
) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let mut canvas = Canvas::new(w, w, BG);
    let footer = w as f64 * 0.085;
    let (lo, hi) = world_bounds(city);
    let view = View::fit(lo, hi, w, w - footer as usize, w as f64 * 0.022);
    let colours = district_colours(city);

    // Every stroke is a fraction of the median block's diameter, so the drawing
    // reads the same at 90 files and at 5 000.
    let unit = {
        let mut d: Vec<f64> = city
            .layout
            .blocks
            .iter()
            .map(|b| f64::from(b.boundary.area()).max(0.0).sqrt())
            .collect();
        d.sort_by(f64::total_cmp);
        view.scale() * if d.is_empty() { 1.0 } else { d[d.len() / 2] }
    };
    let industrial_hosts: BTreeSet<&LogicalPath> =
        city.industrial.iter().map(|m| &m.host).collect();

    // 1. The ground of each district, block by block. Districts tile: every
    //    block belongs to exactly one, so this layer has no gaps.
    for block in &city.layout.blocks {
        let ring = view.ring(&block.boundary.vertices);
        let colour = colours
            .get(&block.district)
            .copied()
            .unwrap_or([26, 28, 33]);
        canvas.fill_polygon(&ring, colour, 1.0);
        if industrial_hosts.contains(&block.district) {
            canvas.fill_polygon(&ring, INDUS, 0.72);
        }
        canvas.stroke_polygon(&ring, (unit * 0.011).max(0.6), BLOCKLINE, 0.75);
    }

    // 2. Lots: the buildable parcels, a shade lighter than the ground, so a
    //    block reads as a group of plots rather than as one painted cell.
    let block_district: BTreeMap<u32, &LogicalPath> = city
        .layout
        .blocks
        .iter()
        .map(|b| (b.id.0, &b.district))
        .collect();
    for lot in &city.layout.lots {
        let Some(district) = block_district.get(&lot.block.0) else {
            continue;
        };
        if industrial_hosts.contains(*district) {
            continue;
        }
        let ring = view.ring(&lot.boundary.vertices);
        let base = colours
            .get(*district)
            .map_or([34, 36, 42], |c| shade(*c, 1.55));
        canvas.fill_polygon(&ring, base, 1.0);
        canvas.stroke_polygon(&ring, (unit * 0.010).max(0.6), LOTLINE, 0.50);
    }

    // 3. Buildings.
    let lot_block: BTreeMap<u32, u32> = city
        .layout
        .lots
        .iter()
        .map(|l| (l.id.0, l.block.0))
        .collect();
    for building in city.layout.buildings.values() {
        if let Some(b) = lot_block.get(&building.lot.0) {
            if let Some(d) = block_district.get(b) {
                if industrial_hosts.contains(*d) {
                    continue;
                }
            }
        }
        let ring = view.ring(&building.footprint.vertices);
        let monument = city.is_monument(&building.path);
        let colour = if monument {
            MONU
        } else {
            // Height lightens the roof, but never to paper white: a
            // saturated highlight reads as a hole in the map.
            shade(BLDG, (0.78 + f64::from(building.height) * 0.05).min(0.97))
        };
        canvas.fill_polygon(&ring, colour, 1.0);
        canvas.stroke_polygon(&ring, (unit * 0.007).max(0.5), [22, 22, 26], 0.60);
        if monument {
            canvas.stroke_polygon(&ring, (unit * 0.018).max(1.1), [255, 236, 170], 0.90);
        }
    }

    // 4. Roads, by class. The network is what the eye should find first.
    let (district_edges, package_edges) = borders(city);
    for (i, segment) in city.layout.roads.segments.iter().enumerate() {
        let (Some(a), Some(b)) = (
            city.layout.roads.nodes.get(segment.from.0 as usize),
            city.layout.roads.nodes.get(segment.to.0 as usize),
        ) else {
            continue;
        };
        let line = [view.at(a.position), view.at(b.position)];
        let (width, colour) = match segment.class {
            RoadClass::Arterial => (unit * 0.082, ROAD_HI),
            RoadClass::Street => (unit * 0.046, ROAD_MID),
            RoadClass::Alley => (unit * 0.026, ROAD_LO),
        };
        canvas.polyline(&line, width.max(1.0), colour, 0.96);
        if package_edges.contains(&i) {
            canvas.polyline(&line, (unit * 0.024).max(1.2), BORDER, 0.90);
        } else if district_edges.contains(&i) {
            canvas.polyline(&line, (unit * 0.012).max(0.8), BORDER, 0.26);
        }
    }

    // 5. The city limit.
    let limit = city_limit(city);
    if limit.len() >= 3 {
        let ring = view.ring(&limit);
        canvas.stroke_polygon(&ring, (unit * 0.020).max(1.2), LIMIT, 0.55);
    }

    // 6. Streets: cross-district imports routed along the roads (PRD §9). A
    //    toggleable layer, off at this zoom unless asked for, and drawn along
    //    the road network rather than across open ground. Width is the number of
    //    distinct import edges the relation carries, which is what PRD §9 asks
    //    the width to mean.
    if streets {
        let mut lines: Vec<_> = city.layout.streets.iter().collect();
        lines.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.edge_count),
                s.from.clone(),
                s.to.clone(),
            )
        });
        lines.truncate((city.layout.blocks.len() / 9).clamp(6, 60));
        for street in lines {
            let line = view.ring(&street.polyline);
            let width = (unit * (0.010 + 0.004 * f64::from(street.edge_count.min(8)))).max(0.9);
            canvas.polyline(&line, width, STREET, 0.55);
        }
    }

    // 7. District labels (PRD §8).
    draw_labels(&mut canvas, city, &view, unit, ss);

    draw_footer(&mut canvas, city, structure, title, w, footer, ss);
    canvas.downsample(ss)
}

/// Name the largest quarters, avoiding collisions.
fn draw_labels(canvas: &mut Canvas, city: &City, view: &View, unit: f64, ss: usize) {
    let block_by_id: BTreeMap<u32, &polis_layout::Block> =
        city.layout.blocks.iter().map(|b| (b.id.0, b)).collect();
    let mut ranked: Vec<(f64, Px, &LogicalPath)> = Vec::new();
    for (path, district) in &city.layout.districts {
        let mut area = 0.0f64;
        let mut sum = [0.0f64, 0.0f64];
        for id in &district.blocks {
            if let Some(block) = block_by_id.get(&id.0) {
                let a = f64::from(block.boundary.area());
                let c = block.boundary.centroid();
                area += a;
                sum[0] += f64::from(c.x) * a;
                sum[1] += f64::from(c.y) * a;
            }
        }
        if area <= 0.0 || district.blocks.len() < 2 {
            continue;
        }
        let centre = Point::new((sum[0] / area) as f32, (sum[1] / area) as f32);
        ranked.push((area, view.at(centre), path));
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.2.cmp(b.2)));

    let size = (unit * 0.040).clamp(2.0 * ss as f64, 3.4 * ss as f64);
    let mut taken: Vec<(Px, f64, f64)> = Vec::new();
    let mut seen_leaf: BTreeSet<String> = BTreeSet::new();
    for (_, at, path) in ranked {
        if taken.len() >= MAX_LABELS {
            break;
        }
        let full = path.as_str();
        let leaf = full.rsplit('/').next().unwrap_or("");
        let name = if full.is_empty() {
            "ROOT".to_owned()
        } else if seen_leaf.contains(leaf) {
            let mut parts = full.rsplitn(3, '/');
            let last = parts.next().unwrap_or("");
            match parts.next() {
                Some(up) => format!("{up}/{last}"),
                None => last.to_owned(),
            }
        } else {
            leaf.to_owned()
        };
        seen_leaf.insert(leaf.to_owned());
        let tw = Canvas::text_width(&name, size);
        let th = 10.0 * ss as f64;
        // The plate is wider and taller than the glyphs; the keep-out has to
        // clear the plate, or two labels touch and both become unreadable.
        if taken.iter().any(|(o, ow, oh)| {
            (o[0] - at[0]).abs() < (ow + tw) * 0.5 + 8.0 * ss as f64
                && (o[1] - at[1]).abs() < (oh + th) * 1.05
        }) {
            continue;
        }
        taken.push((at, tw, th));
        canvas.rect(
            at[0] - tw * 0.5 - 3.0 * ss as f64,
            at[1] - 6.0 * ss as f64,
            at[0] + tw * 0.5 + 3.0 * ss as f64,
            at[1] + 9.0 * ss as f64,
            PLATE,
            0.62,
        );
        canvas.text(
            at[0] - tw * 0.5,
            at[1] - 3.0 * ss as f64,
            &name,
            size,
            LABEL,
        );
    }
}

/// The legend and the metrics footer.
fn draw_footer(
    canvas: &mut Canvas,
    city: &City,
    structure: &Structure,
    title: &str,
    w: usize,
    height: f64,
    ss: usize,
) {
    let y0 = w as f64 - height;
    canvas.rect(0.0, y0, w as f64, w as f64, PLATE, 0.94);
    canvas.rect(0.0, y0, w as f64, y0 + 2.0 * ss as f64, [70, 74, 84], 1.0);
    let fs = 2.0 * ss as f64;
    let pad = 14.0 * ss as f64;
    canvas.text(pad, y0 + 10.0 * ss as f64, title, fs * 1.5, [240, 240, 240]);

    let swatches = [
        ("DISTRICT GROUND", [46u8, 52u8, 64u8]),
        ("LOT", [64, 72, 88]),
        ("BUILDING", BLDG),
        ("MONUMENT", MONU),
        ("INDUSTRIAL", INDUS),
        ("ARTERIAL", ROAD_HI),
        ("STREET", ROAD_MID),
        ("ALLEY", ROAD_LO),
        ("PACKAGE BORDER", BORDER),
        ("IMPORT ROUTE", STREET),
        ("CITY LIMIT", LIMIT),
    ];
    let mut x = pad;
    let ly = y0 + 34.0 * ss as f64;
    for (name, colour) in swatches {
        canvas.rect(
            x,
            ly,
            x + 11.0 * ss as f64,
            ly + 11.0 * ss as f64,
            colour,
            1.0,
        );
        canvas.text(
            x + 15.0 * ss as f64,
            ly + 2.0 * ss as f64,
            name,
            fs * 0.95,
            [205, 208, 214],
        );
        x += 15.0 * ss as f64 + Canvas::text_width(name, fs * 0.95) + 16.0 * ss as f64;
        if x > w as f64 - 200.0 * ss as f64 {
            break;
        }
    }
    let r = &city.report;
    // Kept under 130 characters: the footer is drawn at a fixed size, and a
    // line that runs off the right edge is worse than a shorter label.
    let line1 = format!(
        "ROADS V={} E={} COMP={} CYCLES={} CROSSINGS={} DEG4+={} DANGLING={} STROKE={}% OF DIAMETER",
        r.road_nodes,
        r.road_segments,
        r.components,
        r.cycles,
        r.crossings,
        r.complex_junctions,
        r.dangling,
        (structure.longest_stroke * 100.0).round() as i64
    );
    let line2 = format!(
        "BLOCKS={} SLIVERS={} P95/P05={}X AGE={}X LOTS={} BUILT={}/{} COVER={}% ON-ROAD={} OFF-LOT={} DISTRICTS={} SPLIT={}",
        r.blocks,
        r.slivers,
        (structure.block_hierarchy * 10.0).round() as i64 / 10,
        (structure.age_gradient * 10.0).round() as i64 / 10,
        r.lots,
        r.buildings,
        r.files - r.massed,
        (structure.coverage * 100.0).round() as i64,
        structure.buildings_on_road,
        structure.buildings_outside_lot,
        r.districts,
        r.fragmented_districts
    );
    canvas.text(
        pad,
        y0 + 56.0 * ss as f64,
        &line1,
        fs * 0.95,
        [168, 174, 186],
    );
    canvas.text(
        pad,
        y0 + 72.0 * ss as f64,
        &line2,
        fs * 0.95,
        [168, 174, 186],
    );
}

/// Render the road graph alone, junctions coloured by degree.
///
/// The "is it a tree?" test, and the render the design bake-off was most
/// confident about: a network with cycles and no dangling ends is immediately,
/// unarguably not a tree.
#[must_use]
pub fn render_junctions(
    city: &City,
    structure: &Structure,
    title: &str,
    pixels: usize,
    supersample: usize,
) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let mut canvas = Canvas::new(w, w, [10, 11, 14]);
    let nodes = &city.layout.roads.nodes;
    if nodes.is_empty() {
        return canvas.downsample(ss);
    }
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for n in nodes {
        lo[0] = lo[0].min(f64::from(n.position.x));
        lo[1] = lo[1].min(f64::from(n.position.y));
        hi[0] = hi[0].max(f64::from(n.position.x));
        hi[1] = hi[1].max(f64::from(n.position.y));
    }
    let footer = w as f64 * 0.075;
    let view = View::fit(lo, hi, w, w - footer as usize, w as f64 * 0.025);

    let mut degree = vec![0usize; nodes.len()];
    let mut lengths: Vec<f64> = Vec::with_capacity(city.layout.roads.segments.len());
    for s in &city.layout.roads.segments {
        if let (Some(a), Some(b)) = (nodes.get(s.from.0 as usize), nodes.get(s.to.0 as usize)) {
            degree[s.from.0 as usize] += 1;
            degree[s.to.0 as usize] += 1;
            lengths.push(f64::from(a.position.distance(b.position)) * view.scale());
        }
    }
    lengths.sort_by(f64::total_cmp);
    let unit = if lengths.is_empty() {
        8.0
    } else {
        lengths[lengths.len() / 2]
    };

    for s in &city.layout.roads.segments {
        if let (Some(a), Some(b)) = (nodes.get(s.from.0 as usize), nodes.get(s.to.0 as usize)) {
            canvas.polyline(
                &[view.at(a.position), view.at(b.position)],
                (unit * 0.055).max(1.0),
                [124, 134, 152],
                0.95,
            );
        }
    }
    let palette = |d: usize| -> (Rgb, f64) {
        match d.min(5) {
            0 | 1 => ([255, 70, 70], unit * 0.20),
            2 => ([70, 100, 190], unit * 0.065),
            3 => ([70, 200, 170], unit * 0.105),
            4 => ([255, 205, 60], unit * 0.155),
            _ => ([255, 110, 235], unit * 0.195),
        }
    };
    // Low degrees first, so the rare high-degree junctions sit on top.
    let mut order: Vec<(usize, usize)> = degree.iter().copied().zip(0..nodes.len()).collect();
    order.sort_unstable();
    for (d, i) in order {
        let (colour, radius) = palette(d);
        canvas.disc(view.at(nodes[i].position), radius.max(1.1), colour, 1.0);
    }

    let y0 = w as f64 - footer;
    canvas.rect(0.0, y0, w as f64, w as f64, [7, 8, 10], 0.96);
    canvas.rect(0.0, y0, w as f64, y0 + 2.0 * ss as f64, [70, 74, 84], 1.0);
    let fs = 2.0 * ss as f64;
    let pad = 14.0 * ss as f64;
    canvas.text(pad, y0 + 9.0 * ss as f64, title, fs * 1.5, [240, 240, 240]);
    let mut histogram = BTreeMap::new();
    for d in &degree {
        *histogram.entry((*d).min(5)).or_insert(0usize) += 1;
    }
    let mut x = pad;
    let ly = y0 + 32.0 * ss as f64;
    for d in 1..=5usize {
        let count = histogram.get(&d).copied().unwrap_or(0);
        let label = match d {
            1 => format!("DEG 1 (DANGLING) : {count}"),
            2 => format!("DEG 2 (BEND) : {count}"),
            5 => format!("DEG 5+ : {count}"),
            _ => format!("DEG {d} : {count}"),
        };
        let (colour, _) = palette(d);
        canvas.disc(
            [x + 6.0 * ss as f64, ly + 6.0 * ss as f64],
            6.0 * ss as f64,
            colour,
            1.0,
        );
        canvas.text(
            x + 17.0 * ss as f64,
            ly + 2.0 * ss as f64,
            &label,
            fs * 0.95,
            [205, 208, 214],
        );
        x += 17.0 * ss as f64 + Canvas::text_width(&label, fs * 0.95) + 20.0 * ss as f64;
    }
    let r = &city.report;
    let junctions = r.junctions.max(1);
    let line = format!(
        "V={} E={} COMP={} CYCLES(E-V+C)={} CROSSINGS WITHOUT A NODE={} DEG4+ SHARE={}% STROKE={}% (A TREE HAS CYCLES=0)",
        r.road_nodes,
        r.road_segments,
        r.components,
        r.cycles,
        r.crossings,
        (100.0 * r.complex_junctions as f64 / junctions as f64).round() as i64,
        (structure.longest_stroke * 100.0).round() as i64
    );
    canvas.text(
        pad,
        y0 + 54.0 * ss as f64,
        &line,
        fs * 0.95,
        [168, 174, 186],
    );
    canvas.downsample(ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_layout::city;
    use polis_repo::synthetic;

    fn small_city() -> City {
        city::generate_city(&synthetic::repository(240, 0x51))
    }

    #[test]
    fn a_plan_is_drawn_and_is_not_blank() {
        let c = small_city();
        let s = city::measure(&c);
        let canvas = render_plan(&c, &s, "TEST", 320, 1, true);
        assert_eq!(canvas.width, 320);
        let distinct: BTreeSet<[u8; 3]> = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2]])
            .collect();
        assert!(
            distinct.len() > 40,
            "only {} distinct colours: the plan is blank",
            distinct.len()
        );
    }

    #[test]
    fn the_render_is_byte_identical_across_runs() {
        let c = small_city();
        let s = city::measure(&c);
        let a = render_plan(&c, &s, "TEST", 240, 1, true).encode_png();
        let b = render_plan(&c, &s, "TEST", 240, 1, true).encode_png();
        assert_eq!(a, b, "the PNG moved between two runs");
        let j1 = render_junctions(&c, &s, "TEST", 240, 1).encode_png();
        let j2 = render_junctions(&c, &s, "TEST", 240, 1).encode_png();
        assert_eq!(j1, j2);
    }

    #[test]
    fn a_package_is_one_hue_family() {
        let c = small_city();
        let colours = district_colours(&c);
        let mut by_top: BTreeMap<&str, Vec<Rgb>> = BTreeMap::new();
        for (path, colour) in &colours {
            by_top
                .entry(path.components().next().unwrap_or(""))
                .or_default()
                .push(*colour);
        }
        // Two districts of the same package must be closer in hue than the
        // spacing between packages.
        let families = by_top.len().max(2) as f64;
        for (top, family) in &by_top {
            if family.len() < 2 {
                continue;
            }
            let hues: Vec<f64> = family.iter().map(|c| hue_of(*c)).collect();
            let span = hues.iter().copied().fold(f64::MIN, f64::max)
                - hues.iter().copied().fold(f64::MAX, f64::min);
            assert!(
                span < 1.0 / families,
                "{top} spans {span} of the hue circle with {families} packages"
            );
        }
    }

    fn hue_of(c: Rgb) -> f64 {
        let r = f64::from(c[0]) / 255.0;
        let g = f64::from(c[1]) / 255.0;
        let b = f64::from(c[2]) / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        if d < 1e-9 {
            return 0.0;
        }
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h / 6.0
    }

    #[test]
    fn the_city_limit_encloses_every_block() {
        let c = small_city();
        let limit = city_limit(&c);
        assert!(limit.len() >= 3);
        // A convex hull: every block vertex is on or inside it.
        for block in &c.layout.blocks {
            for v in &block.boundary.vertices {
                let inside = (0..limit.len()).all(|i| {
                    let a = limit[i];
                    let b = limit[(i + 1) % limit.len()];
                    let cross = (f64::from(b.x) - f64::from(a.x))
                        * (f64::from(v.y) - f64::from(a.y))
                        - (f64::from(b.y) - f64::from(a.y)) * (f64::from(v.x) - f64::from(a.x));
                    cross >= -1e-3
                });
                assert!(inside, "{v:?} is outside the drawn city limit");
            }
        }
    }
}
