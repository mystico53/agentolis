//! One kernel, two crates (PRD §7.4, §10.4, ADR-0020).
//!
//! `polis_world::territory::Territory::anchor` is the point at which a thread's
//! density field is highest, and its whole claim to being the right place for
//! the thread's mark is that it is the argmax of **the field that is actually
//! drawn**. That claim is only true while the world and the rasteriser evaluate
//! the same curve, and they are in different crates with the dependency arrow
//! running one way. So the curve lives in `polis_layout::quartic` and both call
//! it — and this file is what says so out loud, because a second definition
//! would not fail to compile, it would just move the ring a few units off its
//! own cloud and nothing in either crate would notice.
//!
//! It also pins the relationship between the two *peaks*, which is easy to get
//! backwards: `Territory::field_peak` is the maximum over kernel centres and
//! `CloudField::peak` is the maximum over a rasterisation lattice. Both are
//! lower bounds on the true continuous peak, by different amounts, and neither
//! bounds the other.
//!
//! And it closes the loop that unit confusion opened: `RESTING_WEIGHT` is a
//! promise about a number `polis-world` computes, `CLOUD_ISO[0]` is a threshold
//! `polis-render` applies, and until
//! `a_rested_territory_is_drawn_rather_than_merely_selected` there was nothing
//! in the workspace that put the two on the same side of an assertion.

// The fixture states one geometry in `f64` and hands it to both crates, one of
// which is `f32`; and an exact zero outside the radius is the property being
// asserted, not an approximation of one.
#![allow(clippy::cast_possible_truncation, clippy::float_cmp)]

use polis_layout::Point;
use polis_render::live::{CloudField, CloudKernel};
use polis_world::territory::{Kernel, Territory};

/// Kernel radius, in the units both sides happen to share here. Large on
/// purpose: `CloudField::sample` evaluates on a lattice of roughly 3-pixel
/// cells, so the lattice's own error is `~2 (δ/r)²` and a big `r` makes it small
/// enough that a real disagreement about the *curve* cannot hide inside it.
const R: f64 = 300.0;

/// Where the field is centred on the sampled canvas.
const CX: f64 = 400.0;
const CY: f64 = 400.0;

fn world(kernels: &[(f64, f64, f64)]) -> Territory {
    let now = std::time::Instant::now();
    let mut t = Territory::for_extent(1000.0);
    for (x, y, w) in kernels {
        t.kernels.push(Kernel {
            centre: Point::new(*x as f32, *y as f32),
            weight: *w as f32,
            radius: R as f32,
            at: now,
        });
    }
    t
}

fn rendered(kernels: &[(f64, f64, f64)]) -> CloudField {
    let cloud: Vec<CloudKernel> = kernels
        .iter()
        .map(|(x, y, w)| CloudKernel {
            at: [*x, *y],
            radius: R,
            weight: *w,
            thread: 0,
        })
        .collect();
    CloudField::sample(&cloud, 1200, 1200).expect("a field over a canvas that contains it")
}

/// The three points that identify the quartic and rule out every bell curve
/// that is not it: one at the centre, one where the shape is steepest, and one
/// just inside the radius where a Gaussian would still be plainly alight and
/// this is already dark.
#[test]
fn the_world_and_the_renderer_use_one_kernel() {
    assert!((polis_layout::quartic(0.0) - 1.0).abs() < 1e-12);
    assert!((polis_layout::quartic(0.25) - 0.5625).abs() < 1e-12);
    assert_eq!(polis_layout::quartic(1.0), 0.0);

    let one = [(CX, CY, 1.0)];
    let t = world(&one);
    let field = rendered(&one);

    for (frac, expect) in [(0.0f64, 1.0f64), (0.5, 0.5625), (0.95, 0.009_506_25)] {
        let at = (CX + frac * R, CY);
        #[allow(clippy::cast_possible_truncation)] // test coordinates, all small
        let world_says = t.density_at(Point::new(at.0 as f32, at.1 as f32));
        let drawn = field.at(at.0, at.1);
        assert!(
            (f64::from(world_says) - expect).abs() < 1e-5,
            "the world's field at r = {frac}·radius is {world_says}, not {expect}"
        );
        // The lattice samples cell centres and bilinearly resamples between
        // them, so it reads slightly low near a peak and slightly off elsewhere.
        // 0.02 is an order of magnitude above that and an order below the gap
        // between the quartic and any Gaussian through the same centre.
        assert!(
            (f64::from(drawn) - expect).abs() < 0.02,
            "the rasteriser's field at r = {frac}·radius is {drawn}, not {expect}"
        );
    }
}

/// Which peak bounds which — the thing the plan for this change got backwards.
#[test]
fn each_peak_is_a_lower_bound_on_the_field_and_neither_bounds_the_other() {
    // Far apart: no kernel reaches another, so the true maximum *is* a kernel
    // centre and both estimators should find it. The lattice loses only its
    // sub-cell offset, which at this radius is under a thousandth.
    let apart = [(CX, CY, 1.0), (CX + 2.5 * R, CY, 0.6)];
    let (t, field) = (world(&apart), rendered(&apart));
    let (peak, drawn) = (t.field_peak(), field.peak());
    assert!(
        (peak - 1.0).abs() < 1e-5,
        "the heaviest kernel stands alone, so the peak is its weight: {peak}"
    );
    assert!(
        drawn <= peak * 1.000_01 && drawn >= peak * 0.999,
        "and the lattice finds the same peak, less its sub-cell offset: {drawn} against {peak}"
    );

    // Half a radius apart: the sum peaks *between* the two centres, at
    // 2·quartic(0.0625) = 1.758, and `field_peak` cannot see that point because
    // it only looks at centres. It reports 1 + quartic(0.25) = 1.5625. The
    // lattice does see it. So `field_peak` is a lower bound here and the
    // ordering from the previous case is reversed — which is exactly why the
    // two are not interchangeable and why `Territory::field_peak`'s doc calls
    // itself an estimator.
    let close = [(CX, CY, 1.0), (CX + 0.5 * R, CY, 1.0)];
    let (t, field) = (world(&close), rendered(&close));
    let (peak, drawn) = (t.field_peak(), field.peak());
    assert!(
        (peak - 1.5625).abs() < 1e-4,
        "the best kernel centre carries {peak}"
    );
    assert!(
        drawn > peak,
        "and the true peak is between the two, which the lattice finds and the \
         centre scan cannot: {drawn} against {peak}"
    );
    assert!(
        (f64::from(drawn) - 1.758_056).abs() < 0.01,
        "at the midpoint of two kernels half a radius apart: {drawn}"
    );
}

/// PRD §10.4's cloud, drawn rather than merely chosen.
///
/// > let dormant territories dissipate entirely
///
/// A converged territory that goes quiet is held at `RESTING_WEIGHT` by
/// `Territory::rest` so that the operator opening the map on agents who have
/// just stopped sees where they were. That promise is only kept if the number
/// held is the one `CLOUD_ISO` thresholds — the **field**, per ADR-0020's *"one
/// full-weight kernel == 1.0"* — and for most of `rest`'s life it was the mass
/// instead. The two are equal for one kernel and diverge for exactly the
/// multi-lobed shape PRD §6.4 exists to draw, which is how 7.0 % of selected
/// clouds came to be computed, ranked, handed to the rasteriser and never
/// painted.
///
/// So this drives the real `Territory` through the real `observe`/`decay`, hands
/// its rested kernels to the real `CloudField::sample`, and asks the rasteriser
/// itself whether anything is above the fringe. No ordering between
/// `field_peak()` and `CloudField::peak()` is asserted, in either direction:
/// `each_peak_is_a_lower_bound_on_the_field_and_neither_bounds_the_other` above
/// is why there is none to assert.
#[test]
fn a_rested_territory_is_drawn_rather_than_merely_selected() {
    use polis_events::{SessionId, ThreadId, ToolKind};
    use polis_render::live::CLOUD_ISO;
    use polis_world::territory::RESTING_WEIGHT;
    use polis_world::{Observation, PathScope};
    use std::time::Duration;

    let t0 = std::time::Instant::now();
    let mut t = Territory::for_extent(1000.0);
    // PRD §6.4's shape: *"An agent working in `auth` with one worker in `tests`
    // gets two lobes and a thin connecting band."* Two well-separated lobes are
    // the minimum arrangement in which mass and peak are different numbers.
    for i in 0..6u32 {
        for (dir, at) in [
            ("src/auth", Point::new(300.0, 300.0)),
            ("tests/unit", Point::new(700.0, 700.0)),
        ] {
            t.observe(
                &Observation {
                    thread: ThreadId::of_session(SessionId::new("s")),
                    worker: None,
                    path: polis_events::LogicalPath::new(&format!("{dir}/f{i}.rs"))
                        .expect("a path"),
                    scope: PathScope::File,
                    tool: ToolKind::Edit,
                    at: t0,
                    weight: None,
                },
                1.0,
                Some(at),
            );
        }
    }
    assert!(t.placement().is_somewhere(), "it has lobes to hold");

    // Seven and a half minutes quiet: past the point PRD §6.3's half-life takes
    // the field under the floor, and inside `DORMANT_AFTER`, which is the window
    // `rest` exists for.
    t.decay(t0 + Duration::from_secs(450));

    let peak = t.field_peak();
    let mass = t.mass();
    assert!(
        (f64::from(peak) - f64::from(RESTING_WEIGHT)).abs() < 1e-3,
        "the world holds the field's peak at RESTING_WEIGHT: {peak}"
    );

    // The same kernels, through the shipped sampler, at the identity camera
    // `polis_render::frame` uses when its scale is 1.
    let cloud: Vec<CloudKernel> = t
        .kernels
        .iter()
        .map(|k| CloudKernel {
            at: [f64::from(k.centre.x), f64::from(k.centre.y)],
            radius: f64::from(k.radius),
            weight: f64::from(k.weight),
            thread: 0,
        })
        .collect();
    let drawn = CloudField::sample(&cloud, 1000, 1000).expect("a field over the canvas");
    assert!(
        f64::from(drawn.peak()) >= CLOUD_ISO[0],
        "a rested territory has to clear the outermost iso band or the cloud is \
         computed and never painted: the lattice reads {} against a fringe of {}",
        drawn.peak(),
        CLOUD_ISO[0]
    );

    // And the number that used to be normalised, priced. Had `rest` put `mass`
    // on RESTING_WEIGHT, every density would be smaller by exactly this ratio,
    // and this is what the fringe would have been asked to accept.
    let under_the_old_rule = f64::from(peak) * f64::from(RESTING_WEIGHT) / f64::from(mass);
    assert!(
        under_the_old_rule < CLOUD_ISO[0],
        "the fixture has to reproduce the defect or it is not testing the fix: \
         mass-normalised this field would peak at {under_the_old_rule}, which is \
         above the fringe {}",
        CLOUD_ISO[0]
    );
}
