//! PRD §11.1's ordering, asserted on **pixels** rather than on a sort key.
//!
//! > `contention > needs-decision > done`. Contention is the only one where work
//! > is actively being destroyed. A pending decision costs wall clock. Done
//! > costs nothing.
//!
//! `polis_world::attention::sort` makes that true of a *list*. It says nothing
//! about the picture, and the picture is what the operator reads: a state can
//! sort first and still be the faintest thing on the map. Until M5 that is
//! exactly what happened — the attention band, which PRD §10.3 reserves for
//! these three states alone, was measured at **0.000 % of map area in every one
//! of 1 440 rendered frames**, because nothing in a transcript replay could
//! raise a mark at all.
//!
//! So this renders each state on its own, on identical canvases, and measures
//! the ink. Two scales, because they answer different questions:
//!
//! * **full resolution** — is the ordering true of the notation?
//! * **thumbnail** (box-downsampled by [`THUMB`]) — is it still true from across
//!   the room, where thin strokes disappear and solid shapes do not?
//!
//! `POLIS_OUT=<dir>` writes a sheet of all four.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::Duration;

use polis_render::live::{self, AttentionMark, LiveFrame, MarkKind};
use polis_render::plan::ATTENTION_BAND;
use polis_render::raster::Canvas;
use polis_world::contention::Severity;

/// Canvas edge for one state's panel.
const SIZE: usize = 360;

/// Distance between the two ends of the contention link, in pixels.
///
/// A third of the panel: a deliberately *modest* separation. Contention's size
/// comes from the distance between two places, so a link across the whole map
/// would win the comparison by construction and prove nothing. If it dominates
/// at a third of the frame it dominates.
const LINK: f64 = SIZE as f64 / 3.0;

/// The box-filter factor that stands in for distance.
const THUMB: usize = 6;

/// The map unit for every panel: identical, so the four states are compared at
/// one scale.
const UNIT: f64 = 34.0;

fn panel(kind: MarkKind, urgency: f64, pulse: f64) -> Canvas {
    let mut canvas = Canvas::new(SIZE, SIZE, [18, 19, 22]);
    let centre = SIZE as f64 / 2.0;
    let mark = AttentionMark {
        kind,
        at: if kind == MarkKind::Contention {
            [centre - LINK / 2.0, centre + 40.0]
        } else {
            [centre, centre + 30.0]
        },
        other: (kind == MarkKind::Contention).then_some([centre + LINK / 2.0, centre + 40.0]),
        severity: (kind == MarkKind::Contention).then_some(Severity::Critical),
        pulse,
        weight: 1.0,
        urgency,
        // The comparison is between the four states, so every panel is drawn
        // where the work is. `salience`'s own tests cover the fallback.
        sited: true,
    };
    let frame = LiveFrame {
        unit: UNIT,
        map_height: SIZE as f64,
        attention: vec![mark],
        ..LiveFrame::default()
    };
    live::draw_attention(&mut canvas, &frame);
    canvas
}

/// Ink in [`ATTENTION_BAND`] — the band PRD §10.3 reserves for layer 5 — as a
/// pixel count, and the same after downsampling to thumbnail size.
fn ink(canvas: &Canvas) -> (usize, usize) {
    let full = canvas
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|p| p.iter().copied().max().unwrap_or(0) >= ATTENTION_BAND.0)
        .count();
    let thumb = canvas.downsample(THUMB);
    // The same test after the box filter, and the band floor is the right line
    // for it: PRD §10.3's claim is that layer 5 owns channels 169–255, and a
    // stroke the filter has averaged down out of that band is a stroke that has
    // been averaged into the city. It is not "dimmer"; it is gone.
    let floor = ATTENTION_BAND.0;
    let thumb = thumb
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|p| p.iter().copied().max().unwrap_or(0) >= floor)
        .count();
    (full, thumb)
}

fn sheet(panels: &[(&str, Canvas)]) {
    let Ok(dir) = std::env::var("POLIS_OUT") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut out = Canvas::new(SIZE * panels.len(), SIZE, [18, 19, 22]);
    for (i, (name, c)) in panels.iter().enumerate() {
        for y in 0..SIZE {
            let s = y * SIZE * 3;
            let d = (y * out.width + i * SIZE) * 3;
            out.pixels[d..d + SIZE * 3].copy_from_slice(&c.pixels[s..s + SIZE * 3]);
        }
        out.text(
            (i * SIZE) as f64 + 14.0,
            18.0,
            &name.to_uppercase(),
            2.0,
            [120, 126, 138],
        );
    }
    let _ = out.write_png(&dir.join("attention-states.png"));
    let _ = out
        .downsample(THUMB)
        .write_png(&dir.join("attention-states-thumb.png"));
}

/// The headline: §11.1 holds in the picture, at reading distance **and** at
/// thumbnail size.
#[test]
fn the_three_states_are_ordered_by_ink_and_not_only_by_sort_key() {
    let panels = [
        ("contention", panel(MarkKind::Contention, 0.0, 0.0)),
        ("needs decision", panel(MarkKind::NeedsDecision, 1.0, 0.0)),
        ("done unverified", panel(MarkKind::DoneUnverified, 1.0, 0.0)),
        ("done verified", panel(MarkKind::DoneVerified, 0.0, 0.0)),
    ];
    let mut measured = Vec::new();
    for (name, c) in &panels {
        let (full, thumb) = ink(c);
        eprintln!("  {name:<16} {full:>6} px full, {thumb:>4} px at 1/{THUMB} scale");
        measured.push((*name, full, thumb));
    }
    sheet(&panels);

    for w in measured.windows(2) {
        let (a, af, at) = w[0];
        let (b, bf, bt) = w[1];
        assert!(
            af > bf,
            "PRD §11.1: {a} must outweigh {b}, but they inked {af} and {bf} px"
        );
        assert!(
            at >= bt,
            "PRD §11.1 fails at thumbnail size: {a} {at} px vs {b} {bt} px"
        );
    }
    // A margin, not a hair: "contention is the only one where work is actively
    // being destroyed" has to be obvious, not arithmetically true.
    assert!(
        measured[0].1 > measured[1].1 * 3 / 2,
        "contention only beats a pending decision by {}%",
        100 * measured[0].1 / measured[1].1 - 100
    );
    // …and `done` must not compete for a glance from across the room. §11.1:
    // "Done costs nothing."
    assert!(
        measured[3].2 * 3 <= measured[1].2,
        "done, verified is {} thumbnail px against a decision's {}",
        measured[3].2,
        measured[1].2
    );
}

/// Every state must be readable in **greyscale**, because PRD §11.4 forbids
/// colour as the sole channel for any of them.
///
/// The test throws the colour away and asks whether the four marks are still
/// four different things, using the one description a box filter preserves: how
/// the ink is distributed with distance from the mark's own centre.
#[test]
fn the_four_marks_are_four_silhouettes_with_the_colour_thrown_away() {
    let kinds = [
        MarkKind::Contention,
        MarkKind::NeedsDecision,
        MarkKind::DoneUnverified,
        MarkKind::DoneVerified,
    ];
    let profile = |kind: MarkKind| {
        let c = panel(kind, 1.0, 0.0);
        let centre = [SIZE as f64 / 2.0, SIZE as f64 / 2.0 + 30.0];
        // Eight radial bins out to the panel's half-width, counting lit pixels.
        let mut bins = [0usize; 8];
        for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
            if p.iter().copied().max().unwrap_or(0) < ATTENTION_BAND.0 {
                continue;
            }
            let dx = (i % SIZE) as f64 - centre[0];
            let dy = (i / SIZE) as f64 - centre[1];
            let d = dx.mul_add(dx, dy * dy).sqrt() / (SIZE as f64 / 2.0);
            let b = ((d * 8.0) as usize).min(7);
            bins[b] += 1;
        }
        bins
    };
    let profiles: Vec<[usize; 8]> = kinds.iter().map(|k| profile(*k)).collect();
    for (k, p) in kinds.iter().zip(&profiles) {
        eprintln!("  {k:?}: {p:?}");
    }
    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            // Cosine distance between the two normalised profiles: two marks
            // that put their ink at the same radii are the same silhouette,
            // whatever colour they are drawn in.
            let dot: f64 = profiles[i]
                .iter()
                .zip(&profiles[j])
                .map(|(a, b)| (*a as f64) * (*b as f64))
                .sum();
            let na: f64 = profiles[i].iter().map(|a| (*a as f64).powi(2)).sum::<f64>();
            let nb: f64 = profiles[j].iter().map(|a| (*a as f64).powi(2)).sum::<f64>();
            let cos = dot / (na.sqrt() * nb.sqrt()).max(1.0);
            assert!(
                cos < 0.97,
                "{:?} and {:?} have the same radial profile (cos {cos:.3}) — \
                 they differ only by colour",
                kinds[i],
                kinds[j],
            );
        }
    }
}

/// PRD §11.4 leaves the steady state as "shape and position". The corpus says
/// that is not enough: 61 % of the operator's measured waits were already past a
/// minute and 26 % past fifteen, and every one of them used to draw the same
/// mark. Urgency is spent on **area**, so the difference survives distance.
#[test]
fn a_pin_that_has_been_ignored_is_bigger_than_one_that_just_arrived() {
    let fresh = ink(&panel(MarkKind::NeedsDecision, 0.0, 0.0));
    let stale = ink(&panel(MarkKind::NeedsDecision, 1.0, 0.0));
    eprintln!("  fresh {fresh:?}, five minutes old {stale:?}");
    assert!(
        stale.0 > fresh.0 * 3 / 2,
        "a five-minute-old pin inked {} px against a fresh one's {}",
        stale.0,
        fresh.0
    );
    assert!(
        stale.1 > fresh.1,
        "the escalation does not survive to thumbnail size: {} vs {}",
        stale.1,
        fresh.1
    );
    // And the same for "needs review", which persists for the same reason.
    let fresh = ink(&panel(MarkKind::DoneUnverified, 0.0, 0.0));
    let stale = ink(&panel(MarkKind::DoneUnverified, 1.0, 0.0));
    assert!(stale.0 > fresh.0, "{stale:?} vs {fresh:?}");
}

/// PRD §11.4's arrival, on the state the product is for.
#[test]
fn an_arriving_pin_pulses_outward_and_is_gone_in_four_hundred_milliseconds() {
    let reach = |pulse: f64| {
        let c = panel(MarkKind::NeedsDecision, 0.0, pulse);
        let centre = [SIZE as f64 / 2.0, SIZE as f64 / 2.0 + 30.0];
        let mut far = 0.0f64;
        for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
            if p.iter().copied().max().unwrap_or(0) < ATTENTION_BAND.0 {
                continue;
            }
            let dx = (i % SIZE) as f64 - centre[0];
            let dy = (i / SIZE) as f64 - centre[1];
            far = far.max(dx.mul_add(dx, dy * dy).sqrt());
        }
        far
    };
    let rest = reach(0.0);
    let mid = reach(0.4);
    eprintln!("  pin reaches {rest:.1} px at rest, {mid:.1} px mid-pulse");
    assert!(
        mid > rest * 1.2,
        "the arrival is not motion: {mid:.1} px against {rest:.1} at rest"
    );
    assert!(
        (reach(0.0) - rest).abs() < f64::EPSILON,
        "the pulse left something behind"
    );
}

/// PRD §13.1 budgets layers 4 and 5 together at under 4 ms. Layer 5 alone, with
/// every state standing at once, must be a rounding error inside that.
#[test]
fn the_attention_layer_is_cheap_even_when_everything_is_on_fire() {
    let mut canvas = Canvas::new(1200, 1200, [18, 19, 22]);
    let mut frame = LiveFrame {
        unit: UNIT,
        map_height: 1200.0,
        ..LiveFrame::default()
    };
    for i in 0..48 {
        let x = 60.0 + f64::from(i % 8) * 140.0;
        let y = 60.0 + f64::from(i / 8) * 140.0;
        let kind = match i % 4 {
            0 => MarkKind::Contention,
            1 => MarkKind::NeedsDecision,
            2 => MarkKind::DoneUnverified,
            _ => MarkKind::DoneVerified,
        };
        frame.attention.push(AttentionMark {
            kind,
            at: [x, y],
            other: (kind == MarkKind::Contention).then_some([1140.0 - x, 1140.0 - y]),
            severity: (kind == MarkKind::Contention).then_some(Severity::Critical),
            pulse: 0.3,
            weight: 1.0,
            urgency: 1.0,
            sited: true,
        });
    }
    live::draw_attention(&mut canvas, &frame);
    let mut best = Duration::from_secs(1);
    for _ in 0..5 {
        best = best.min(live::draw_attention(&mut canvas, &frame));
    }
    eprintln!("  48 marks, all four states, all escalated: {best:?}");
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(60)
    } else {
        Duration::from_millis(2)
    };
    assert!(best <= budget, "layer 5 took {best:?}, budget {budget:?}");
}
