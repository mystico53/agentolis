//! **The deliverable measurement**: can the operator tell that an agent is
//! blocked on them, without reading anything?
//!
//! This is `salience.rs`'s question asked of the other colour. PRD §11.2 calls
//! *needs decision* **"the primary state; it is what the product is for"**, PRD
//! §10.3 reserves the top of the contrast range for it and its two siblings, and
//! a live frame taken at the moment one thread was genuinely blocked on a human
//! measured:
//!
//! | ink above L100 | pixels | share |
//! |---|---|---|
//! | decoration — labels, delegation tethers | 34 213 | 74.7 % |
//! | red failure | 11 451 | 24.0 % |
//! | **amber "needs decision"** | **243** | **0.5 %** |
//!
//! Half a percent, for the state the product exists for. The attention band had
//! measured **0.000 % of map area in every frame ever rendered**.
//!
//! So the test is the same thumbnail test the red alarm is held to, in the
//! amber channel. Both frames are box-downsampled by [`THUMB`] — which is what
//! the eye does to a screen on the far side of a room — and compared on:
//!
//! | number | what it means |
//! |---|---|
//! | `peak` | the amberest single thumbnail pixel, as `min(r, g) − b` |
//! | `hot` | thumbnail pixels at or above [`HOT`]: how much of the map says *waiting on you* |
//! | `mass` | mean amber-excess per thumbnail pixel: area × strength |
//!
//! Amber-excess rather than "the red channel", for the same reason `salience`
//! uses red-excess: every ink Polis draws has a red channel and only
//! [`live::ATTN_DECISION`] has *both* red and green dominant over blue. Teal
//! `[118, 236, 206]` scores −88, contention red `[255, 92, 78]` scores 14, the
//! agent band's warmest ink (scaffolding, `[140, 148, 112]`) scores 28, and the
//! base map — clamped at channel 48 and near-grey — scores 0. [`HOT`] is set
//! above all of them.
//!
//! Everything here is hermetic: a synthetic repository, a synthetic city, and a
//! `PermissionRequest` **hook** through the real wire path, which is the channel
//! PRD §11.2 actually sources this state from. `POLIS_OUT=<dir>` writes the two
//! frames and their thumbnails as PNGs.

// A measurement harness: every line divides one count by another.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use polis_events::{EventKind, SessionId, ThreadId};
use polis_ingest::hook_listener::{decode_datagram, to_event};
use polis_layout::city::{self, City};
use polis_render::frame::{FrameOptions, FrameRenderer};
use polis_render::live::{self, AttentionMark, MarkKind};
use polis_render::plan::ATTENTION_BAND;
use polis_render::raster::Canvas;
use polis_render::salience::{self, AlarmKind, Site};
use polis_repo::{synthetic, RepoTree};
use polis_world::{snapshot, World};

/// The box-filter factor that stands in for distance. A 600-pixel map becomes a
/// 60-pixel thumbnail.
const THUMB: usize = 10;

/// Map size for the measurement, in pixels. A multiple of [`THUMB`].
const PIXELS: usize = 600;

/// Amber-excess at which a thumbnail pixel counts as *shouting*.
///
/// [`live::ATTN_DECISION`] is `[255, 188, 62]`, an amber-excess of 126 where it
/// is solid. A thumbnail pixel at 40 is therefore about a third covered by the
/// mark, which on a near-black map is unambiguously amber and not a warm grey —
/// and it is comfortably above the 28 the warmest agent-band ink can reach.
const HOT: i32 = 40;

/// A session id shaped like the real ones.
const SESSION: &str = "40fe953e-32c1-4e9b-8f19-bb9368680fc7";

/// The working directory every payload carries.
const CWD: &str = "C:/repo";

/// What one thumbnail says about *waiting on you*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Amber {
    peak: i32,
    hot: usize,
    mass: i64,
    pixels: usize,
}

impl Amber {
    fn of(thumb: &Canvas) -> Self {
        let mut peak = 0;
        let mut hot = 0;
        let mut mass = 0i64;
        for p in thumb.pixels.as_chunks::<3>().0 {
            let excess = i32::from(p[0]).min(i32::from(p[1])) - i32::from(p[2]);
            if excess > peak {
                peak = excess;
            }
            if excess >= HOT {
                hot += 1;
            }
            mass += i64::from(excess.max(0));
        }
        Self {
            peak,
            hot,
            mass,
            pixels: thumb.width * thumb.height,
        }
    }

    fn mean(self) -> f64 {
        self.mass as f64 / self.pixels as f64
    }

    fn report(self, name: &str) -> String {
        format!(
            "{name}: peak {:>3} amber-excess, {:>4} hot px of {} ({:.2}% of the thumbnail), \
             mean {:.2}",
            self.peak,
            self.hot,
            self.pixels,
            100.0 * self.hot as f64 / self.pixels as f64,
            self.mean(),
        )
    }
}

fn small_city() -> City {
    city::generate_city(&synthetic::repository(320, 0x51))
}

fn world_for(city: &City) -> World {
    let mut world = World::new(RepoTree::default(), city.layout.clone());
    world
        .mapper_mut()
        .add_worktree(polis_events::WorktreeId::PRIMARY, std::path::Path::new(CWD))
        .expect("primary root");
    world
}

/// One datagram in `polis-hook`'s wire format (PRD §4.2).
fn feed(world: &mut World, kind: EventKind, body: &str, at: Instant) {
    let payload = body.as_bytes();
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&(kind as u32).to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    let mut event = to_event(decode_datagram(&frame).expect("a well-formed datagram"));
    event.meta.observed = at;
    world.apply(&event);
    world.tick(at);
}

fn envelope(name: &str) -> String {
    format!(
        r#""session_id":"{SESSION}","transcript_path":"{CWD}/.jsonl","cwd":"{CWD}","permission_mode":"acceptEdits","hook_event_name":"{name}""#
    )
}

/// A session that has done a little work in one district, so the thread has a
/// place to be drawn.
fn working_session(city: &City, t0: Instant) -> World {
    let mut world = world_for(city);
    feed(
        &mut world,
        EventKind::SessionStart,
        &format!(r#"{{{},"source":"startup"}}"#, envelope("SessionStart")),
        t0,
    );
    for (i, path) in city.layout.buildings.keys().take(12).enumerate() {
        feed(
            &mut world,
            EventKind::PreToolUse,
            &format!(
                r#"{{{},"tool_name":"Edit","tool_input":{{"file_path":"{CWD}/{path}"}},"tool_use_id":"toolu_{i:02}"}}"#,
                envelope("PreToolUse")
            ),
            t0 + Duration::from_millis(200 * (i as u64 + 1)),
        );
    }
    world
}

fn render(city: &City, world: &World) -> Canvas {
    let mut r = FrameRenderer::new(
        city,
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let (_, reader) = snapshot::from_world(world);
    let snap = reader.load();
    r.render_owned(&snap, Duration::from_millis(40))
}

fn write(name: &str, canvas: &Canvas) {
    let Ok(dir) = std::env::var("POLIS_OUT") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = canvas.write_png(&dir.join(format!("{name}.png")));
    let _ = canvas
        .downsample(THUMB)
        .write_png(&dir.join(format!("{name}-thumb.png")));
}

/// **The acceptance test.** A map with a blocked agent and a map without are
/// distinguishable at thumbnail size, without reading a counter.
///
/// PRD §11.4 is the mechanism and it is not about hue:
///
/// > Peripheral vision is poor at colour and good at motion onset. […] Colour
/// > alone is never the sole channel for any state.
///
/// So the steady state is measured with the arrival pulse **over** — no motion,
/// no flare, nothing but shape and area — because that is the state a mark
/// spends its whole life in. Measured over the operator's corpus, 61 % of real
/// waits were already past a minute.
#[test]
fn a_blocked_agent_is_visible_from_across_the_room() {
    let c = small_city();
    let t0 = Instant::now();

    let clean = working_session(&c, t0);
    let clean_frame = render(&c, &clean);

    let mut blocked = working_session(&c, t0);
    feed(
        &mut blocked,
        EventKind::PermissionRequest,
        &format!(
            r#"{{{},"tool_name":"Bash","tool_input":{{"command":"rm -rf build"}}}}"#,
            envelope("PermissionRequest")
        ),
        t0 + Duration::from_secs(4),
    );
    assert_eq!(
        blocked.attention.len(),
        1,
        "the hook did not raise the primary state: {:?}",
        blocked.attention
    );
    // Past the 400 ms pulse: what is measured is the standing mark, not its
    // arrival.
    blocked.tick(t0 + Duration::from_secs(9));
    let blocked_frame = render(&c, &blocked);

    write("decision-clean", &clean_frame);
    write("decision-blocked", &blocked_frame);

    let a = Amber::of(&clean_frame.downsample(THUMB));
    let b = Amber::of(&blocked_frame.downsample(THUMB));
    eprintln!("  {}", a.report("clean  "));
    eprintln!("  {}", b.report("blocked"));
    eprintln!(
        "  before the beacon, the same mark was worth {} attention-band px",
        pin_only(&blocked_frame, &blocked)
    );

    assert_eq!(
        a.hot, 0,
        "a map with nobody waiting on the operator has {} amber thumbnail pixels",
        a.hot
    );
    // The bar the red alarm set for itself: this is *area*, not a hotter hue.
    // Two thumbnail pixels of difference is not a difference — that was the
    // measured state of the failure channel before `salience` existed, and it
    // is the state the amber channel was in until this file.
    assert!(
        b.hot >= 12,
        "a blocked agent lights {} thumbnail pixels of {}; that is not a difference \
         a glance can make",
        b.hot,
        b.pixels
    );
    assert!(
        b.peak >= HOT * 2,
        "the amberest thumbnail pixel is {} — the mark washed out under the box filter",
        b.peak
    );
    // Mean amber-excess over the *whole* thumbnail. The clean map is not zero
    // here and cannot be: the agent band's warmest ink — PRD §8's scaffolding,
    // `[140, 148, 112]` — scores 28, well under [`HOT`] but not under nothing.
    // So the claim is a ratio on the total, not an absence.
    assert!(
        b.mean() > a.mean() * 2.0,
        "blocked mean {:.2} against clean {:.2}",
        b.mean(),
        a.mean()
    );
}

/// What the same mark was worth **before** the beacon: the attention-band ink
/// of the frame, minus every pixel the beacons put there.
///
/// Exact rather than remembered. The pin is unchanged by this work, so
/// subtracting the beacons from the rendered frame is the frame the previous
/// notation drew — no second build and no stale number in a comment.
fn pin_only(frame: &Canvas, world: &World) -> usize {
    let (_, reader) = snapshot::from_world(world);
    let snap = reader.load();
    let mut r = FrameRenderer::new(
        &small_city(),
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let live_frame = r.build_frame(&snap, Duration::from_millis(40));
    let radius = live::glyph_radius(live_frame.unit, live_frame.map_height);
    let (decisions, contended) =
        salience::beacons(&live_frame.attention, radius, live_frame.map_height);
    let mut mask = Canvas::new(frame.width, frame.height, [0, 0, 0]);
    salience::draw(&mut mask, &decisions, radius);
    salience::draw(&mut mask, &contended, radius);
    frame
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .zip(mask.pixels.as_chunks::<3>().0)
        .filter(|(p, m)| {
            p.iter().copied().max().unwrap_or(0) >= ATTENTION_BAND.0
                && m.iter().copied().max().unwrap_or(0) == 0
        })
        .count()
}

/// The state must survive **greyscale**, because PRD §11.4 forbids colour as the
/// sole channel — so the two frames also have to differ in where the ink is,
/// not only in what colour it is.
#[test]
fn the_difference_survives_the_colour_being_thrown_away() {
    let c = small_city();
    let t0 = Instant::now();
    let clean = render(&c, &working_session(&c, t0));

    let mut blocked = working_session(&c, t0);
    feed(
        &mut blocked,
        EventKind::PermissionRequest,
        &format!(
            r#"{{{},"tool_name":"Bash","tool_input":{{"command":"rm -rf build"}}}}"#,
            envelope("PermissionRequest")
        ),
        t0 + Duration::from_secs(4),
    );
    blocked.tick(t0 + Duration::from_secs(9));
    let with = render(&c, &blocked);

    // Luma, and only the top of the range: PRD §10.3 gives layer 5 channels
    // 169–255 and clamps the base map at 48, so a difference here is the
    // attention layer and cannot be the city.
    let bright = |c: &Canvas| {
        c.pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| p.iter().copied().max().unwrap_or(0) >= ATTENTION_BAND.0)
            .count()
    };
    let (a, b) = (bright(&clean), bright(&with));
    eprintln!(
        "  attention band: clean {a} px, blocked {b} px of {}",
        PIXELS * PIXELS
    );
    assert_eq!(a, 0, "the attention band is not empty on a clean map");
    assert!(
        b > 900,
        "one blocked agent owns {b} px of the band PRD §10.3 reserves for it"
    );

    // …and at thumbnail size, in greyscale, the two frames differ in *cells*
    // rather than in a shade: a box filter erases a two-pixel outline and keeps
    // a ring around a district.
    let (ta, tb) = (clean.downsample(THUMB), with.downsample(THUMB));
    let changed = ta
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .zip(tb.pixels.as_chunks::<3>().0)
        .filter(|(x, y)| {
            let lx = i32::from(x[0]).max(i32::from(x[1])).max(i32::from(x[2]));
            let ly = i32::from(y[0]).max(i32::from(y[1])).max(i32::from(y[2]));
            (ly - lx) >= 24
        })
        .count();
    eprintln!("  {changed} thumbnail cells got measurably brighter");
    assert!(
        changed >= 12,
        "only {changed} thumbnail cells changed; the mark is not a shape at this scale"
    );
}

/// PRD §11.1's ordering, held to the same bar with the beacons in play.
///
/// > `contention > needs-decision > done`.
///
/// `attention_layer.rs` asserts this on whole rendered panels. This asserts it
/// on the beacons alone, which is where the ordering is now *decided*: the ring
/// is by far the largest thing either state draws, so if the tiers were equal
/// the picture would say the two states are equal however the pins differ.
#[test]
fn the_beacons_keep_contention_above_a_pending_decision() {
    let r = 6.0;
    let h = 600.0;
    let mark = |kind: MarkKind, other: Option<[f64; 2]>| AttentionMark {
        kind,
        at: [200.0, 300.0],
        other,
        severity: None,
        pulse: 0.0,
        weight: 1.0,
        urgency: 0.0,
        sited: true,
    };
    let ink = |beacons: &[salience::Alarm]| {
        let mut canvas = Canvas::new(600, 600, [0, 0, 0]);
        salience::draw(&mut canvas, beacons, r);
        canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| p.iter().copied().max().unwrap_or(0) > 0)
            .count()
    };

    let (decision, _) = salience::beacons(&[mark(MarkKind::NeedsDecision, None)], r, h);
    let (_, contention) =
        salience::beacons(&[mark(MarkKind::Contention, Some([420.0, 300.0]))], r, h);
    let (done, _) = salience::beacons(&[mark(MarkKind::DoneVerified, None)], r, h);

    assert_eq!(decision.len(), 1);
    assert_eq!(contention.len(), 2, "a relation gets a ring at each end");
    assert!(done.is_empty(), "PRD §11.1: done costs nothing");

    let (d, c) = (ink(&decision), ink(&contention));
    eprintln!("  needs-decision {d} px, contention {c} px");
    assert!(
        c > d * 3 / 2,
        "contention inked {c} px against a decision's {d}; §11.1 is not visible"
    );
}

/// **End to end**: a thread with no territory and no trail still draws.
///
/// The case from the operator's screenshot — all three `WAITING ON YOU` rows
/// belonged to threads the rail called unplaced — reproduced through the real
/// hook wire path. A session that has asked for a permission before touching a
/// single file has nowhere on the map to put a pin, and dropping it is the one
/// outcome PRD §11.2a cannot afford.
#[test]
fn an_unplaced_blocked_agent_still_draws() {
    let c = small_city();
    let t0 = Instant::now();
    let mut world = world_for(&c);
    feed(
        &mut world,
        EventKind::SessionStart,
        &format!(r#"{{{},"source":"startup"}}"#, envelope("SessionStart")),
        t0,
    );
    // No `PreToolUse`, so the thread has no territory, no trail and no anchor.
    feed(
        &mut world,
        EventKind::PermissionRequest,
        &format!(
            r#"{{{},"tool_name":"Bash","tool_input":{{"command":"rm -rf build"}}}}"#,
            envelope("PermissionRequest")
        ),
        t0 + Duration::from_secs(1),
    );
    world.tick(t0 + Duration::from_secs(6));
    assert_eq!(blocked_threads(&world), 1);

    let frame = render(&c, &world);
    write("decision-unplaced", &frame);
    let band = frame
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|p| p.iter().copied().max().unwrap_or(0) >= ATTENTION_BAND.0)
        .count();
    eprintln!("  an unplaced blocked agent inked {band} px of the attention band");
    assert!(
        band > 900,
        "a thread with no scope was dropped rather than drawn: {band} px"
    );

    // …and the notation says the scope is not known: the ring reaches past the
    // spokes, which only the accuracy circle does.
    let (_, reader) = snapshot::from_world(&world);
    let snap = reader.load();
    let mut r = FrameRenderer::new(
        &c,
        FrameOptions {
            pixels: PIXELS,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        },
    );
    let live_frame = r.build_frame(&snap, Duration::from_millis(40));
    let mark = live_frame
        .attention
        .iter()
        .find(|m| m.kind == MarkKind::NeedsDecision)
        .expect("the mark reached the frame");
    assert!(!mark.sited, "an unplaced decision claimed a district");
}

/// How many threads the world says are blocked on a human.
fn blocked_threads(world: &World) -> usize {
    world
        .attention
        .iter()
        .filter(|m| {
            matches!(
                m.kind,
                polis_world::attention::AttentionKind::NeedsDecision { .. }
            )
        })
        .count()
}

/// A pin that has stood for five minutes is physically bigger than one that
/// arrived a second ago — the slow channel PRD §11.4 leaves out and the corpus
/// demands (61 % of real waits past a minute, 26 % past fifteen).
#[test]
fn an_ignored_decision_grows() {
    let r = 6.0;
    let ring = |urgency: f64| {
        salience::rings(
            &[Site {
                at: [300.0, 300.0],
                count: 1,
                age: 0.0,
                pulse: 0.0,
                urgency,
                sited: true,
            }],
            AlarmKind::NeedsDecision,
            r,
            600.0,
        )[0]
        .radius
    };
    let fresh = ring(0.0);
    let stale = ring(1.0);
    assert!(
        stale > fresh * 1.3,
        "a five-minute wait draws {stale:.1} px against a fresh {fresh:.1}"
    );
}

/// An agent blocked on a human whose scope is **not yet known** still draws, and
/// the drawing says so.
///
/// In the operator's screenshot all three `WAITING ON YOU` rows belonged to
/// threads the rail called unplaced, so the state the product exists for was
/// being dropped exactly when it mattered. The accuracy ring is the notation
/// that lets it be drawn honestly — it is geometry, so it survives greyscale.
#[test]
fn an_unplaced_decision_draws_an_accuracy_ring_and_a_placed_one_does_not() {
    let r = 6.0;
    let site = |sited: bool| Site {
        at: [300.0, 300.0],
        count: 1,
        age: 0.0,
        pulse: 0.0,
        urgency: 0.0,
        sited,
    };
    let reach = |sited: bool| {
        let ring = salience::rings(&[site(sited)], AlarmKind::NeedsDecision, r, 600.0);
        let mut far = 0.0f64;
        for s in salience::strokes(ring[0], r) {
            for p in &s.points {
                let dx = p[0] - 300.0;
                let dy = p[1] - 300.0;
                far = far.max(dx.mul_add(dx, dy * dy).sqrt());
            }
        }
        (ring[0].radius, far)
    };
    let (radius, placed) = reach(true);
    let (_, unplaced) = reach(false);
    assert!(
        unplaced > placed * 1.15,
        "the accuracy ring reaches {unplaced:.1} px against a placed mark's {placed:.1}"
    );
    assert!(
        unplaced >= radius * salience::ALARM_UNSITED - 0.001,
        "the accuracy ring is inside the spokes it is supposed to clear"
    );
    // …and the ink is the same amber throughout: an uncertainty that changed
    // colour would be a second colour channel, which PRD §11.4 forbids.
    let strokes = salience::strokes(
        salience::rings(&[site(false)], AlarmKind::NeedsDecision, r, 600.0)[0],
        r,
    );
    let hue = live::ATTN_DECISION;
    for s in &strokes {
        assert_eq!(
            (u32::from(s.ink[0]) * 100 / u32::from(hue[0]).max(1)),
            (u32::from(s.ink[1]) * 100 / u32::from(hue[1]).max(1)),
            "a stroke of the accuracy ring is a different colour: {:?}",
            s.ink
        );
    }
}

/// Several unplaced threads waiting on the operator are **one** ring with a
/// count, not a stack of marks on one pixel.
///
/// This is the whole answer to `polis_world::place`'s objection to drawing at
/// the repository root — *"putting every shell call the session ever ran on one
/// pixel at the centre of the map"*. That failure mode is about volume;
/// clustering removes it.
#[test]
fn every_unplaced_decision_folds_into_one_ring() {
    let r = 6.0;
    let sites: Vec<Site> = (0..7)
        .map(|_| Site {
            at: [300.0, 300.0],
            count: 1,
            age: 0.0,
            pulse: 0.0,
            urgency: 0.0,
            sited: false,
        })
        .collect();
    let rings = salience::rings(&sites, AlarmKind::NeedsDecision, r, 600.0);
    assert_eq!(
        rings.len(),
        1,
        "seven pins at the civic square drew {rings:?}"
    );
    assert_eq!(rings[0].count, 7, "the ring lost the count");
    assert!(!rings[0].sited);
}

/// Determinism (PRD §7.4): the same frame draws the same bytes.
#[test]
fn the_beacons_are_deterministic() {
    let r = 6.0;
    let marks: Vec<AttentionMark> = (0..9)
        .map(|i| AttentionMark {
            kind: MarkKind::NeedsDecision,
            at: [
                80.0 + f64::from(i % 4) * 130.0,
                90.0 + f64::from(i / 4) * 150.0,
            ],
            other: None,
            severity: None,
            pulse: 0.0,
            weight: 1.0,
            urgency: f64::from(i) / 9.0,
            sited: i % 3 == 0,
        })
        .collect();
    let (a, _) = salience::beacons(&marks, r, 600.0);
    let mut shuffled = marks.clone();
    shuffled.reverse();
    let (b, _) = salience::beacons(&shuffled, r, 600.0);
    assert_eq!(a, b);

    let mut c1 = Canvas::new(600, 600, [0, 0, 0]);
    let mut c2 = Canvas::new(600, 600, [0, 0, 0]);
    salience::draw(&mut c1, &a, r);
    salience::draw(&mut c2, &b, r);
    assert_eq!(c1.pixels, c2.pixels);
}

/// The thread the hook fired for, for the record.
#[allow(dead_code)]
fn thread() -> ThreadId {
    ThreadId::of_session(SessionId::new(SESSION))
}

// ---------------------------------------------------------------------------
// The operator's own sessions
// ---------------------------------------------------------------------------

/// **The precision gate, run before anything was made louder.**
///
/// A previous review found `polis watch` on a clean scratch repository showing
/// four `WAITING ON YOU` marks of which none was an agent actually blocked on a
/// human. Making a false signal louder is worse than leaving it quiet, so this
/// replays the operator's real sessions through the **shipped** rules —
/// `World::apply`, `polis_world::apply::turn_boundary`, the real
/// [`polis_world::attention::DecisionSource`] set — and asks of every mark it
/// raises: did a human actually come back?
///
/// A raise ends one of three ways, and only the first two are the product
/// working:
///
/// | outcome | meaning |
/// |---|---|
/// | **resolved** | the mark cleared, which in a transcript means a genuine human turn arrived — the agent *was* blocked and the operator unblocked it |
/// | **standing** | still raised when the transcript ends: either a session sitting at a prompt right now, or one that exited and left no record saying so |
/// | **replaced** | superseded by a later raise on the same thread and source before anyone answered |
///
/// ```sh
/// POLIS_REPO=qurio-toolset POLIS_THREADS=6 cargo test -p polis-render --release \
///     --test decision_salience -- --ignored --nocapture waiting_on_you
/// ```
#[test]
#[ignore = "reads the operator's real sessions"]
fn waiting_on_you_is_measured_before_it_is_amplified() {
    use std::collections::BTreeMap;

    // **Not** idle-compressed. The gaps this measurement is about *are* the
    // idle gaps: a wait is the time between an agent stopping and a human
    // coming back, and `compress_idle_gaps` is designed to delete exactly that.
    let Some(samples) = real_sessions_with(false) else {
        return;
    };
    let mut raised: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut resolved: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut replaced: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut standing: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut waits: Vec<f64> = Vec::new();

    for (label, city, schedule) in samples {
        let mut world = World::new(RepoTree::default(), city.layout.clone());
        world.set_ubiquity(Box::new(polis_world::DenylistUbiquity::default()));
        let origin = Instant::now();
        let mut driver = polis_world::replay::ReplayDriver::with_origin(schedule, origin);
        // (thread, source) -> when it was raised.
        let mut live: BTreeMap<(String, &'static str), Instant> = BTreeMap::new();
        let mut session_raised = 0u32;
        while driver.step(&mut world) {
            let mut now: BTreeMap<(String, &'static str), Instant> = BTreeMap::new();
            for m in &world.attention {
                if let polis_world::attention::AttentionKind::NeedsDecision {
                    thread, source, ..
                } = &m.kind
                {
                    now.insert((thread.as_str().to_owned(), source.label()), m.since);
                }
            }
            for (key, since) in &now {
                match live.get(key) {
                    Some(had) if had == since => {}
                    Some(_) => {
                        *replaced.entry(key.1).or_default() += 1;
                        *raised.entry(key.1).or_default() += 1;
                        session_raised += 1;
                    }
                    None => {
                        *raised.entry(key.1).or_default() += 1;
                        session_raised += 1;
                    }
                }
            }
            for (key, since) in &live {
                if !now.contains_key(key) {
                    *resolved.entry(key.1).or_default() += 1;
                    let now = origin + Duration::from_millis(driver.clock().position_ms());
                    waits.push(now.saturating_duration_since(*since).as_secs_f64());
                }
            }
            live = now;
        }
        for key in live.keys() {
            *standing.entry(key.1).or_default() += 1;
        }
        eprintln!("  {label}: {session_raised} raised");
    }

    let total: u32 = raised.values().sum();
    eprintln!(
        "\n  {:<16} {:>7} {:>9} {:>9} {:>9}",
        "source", "raised", "resolved", "replaced", "standing"
    );
    for (source, n) in &raised {
        eprintln!(
            "  {source:<16} {n:>7} {:>9} {:>9} {:>9}",
            resolved.get(source).copied().unwrap_or(0),
            replaced.get(source).copied().unwrap_or(0),
            standing.get(source).copied().unwrap_or(0),
        );
    }
    let ok: u32 = resolved.values().sum();
    let again: u32 = replaced.values().sum();
    eprintln!(
        "\n  {ok} of {total} raises were followed by a human coming back ({:.1}%)",
        100.0 * f64::from(ok) / f64::from(total.max(1))
    );
    // The number that actually answers "does this over-report".
    //
    // A *replacement* is the same thread ending a second turn before anyone
    // answered the first, so it is one continuous wait counted twice, not a
    // second false alarm — `AttentionKind::same_subject` folds it into one pin
    // and the map only ever showed one. Counting distinct waits is therefore
    // the honest denominator, and it is the one that decides whether this
    // signal may be amplified.
    let distinct = total.saturating_sub(again);
    eprintln!(
        "  distinct waits {distinct}; {ok} ended with a human coming back ({:.1}%), \
         {} still standing when the transcript ends",
        100.0 * f64::from(ok) / f64::from(distinct.max(1)),
        standing.values().sum::<u32>(),
    );
    waits.sort_by(f64::total_cmp);
    if !waits.is_empty() {
        let p = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
        eprintln!(
            "  answered waits: n={} p50={:.0}s p75={:.0}s p90={:.0}s max={:.0}s; under 30 s: {}",
            waits.len(),
            p(0.5),
            p(0.75),
            p(0.9),
            waits[waits.len() - 1],
            waits.iter().filter(|w| **w < 30.0).count(),
        );
    }
    assert!(total > 0, "no session raised the primary state at all");
    // The gate on the amplification. Measured at 96.5 % over six of the
    // operator's real sessions (142 distinct waits, 137 answered, 5 standing);
    // 80 % is the line below which a district-scale amber ring would be
    // shouting about something that is usually not there, and PRD §17 is
    // explicit that a false alert is worse than a quiet one.
    assert!(
        f64::from(ok) >= f64::from(distinct) * 0.8,
        "only {ok} of {distinct} distinct waits ended with a human coming back — \
         this signal is not precise enough to be made louder"
    );
}

/// **Amber's share of the bright ink**, on the operator's own repository.
///
/// The number this whole change exists to move. Reported before and after in one
/// run: the beacons are drawn onto a mask of their own and subtracted, so
/// "before" is this frame as the previous notation drew it rather than a
/// remembered figure from a screenshot.
///
/// ```sh
/// POLIS_REPO=qurio-toolset POLIS_THREADS=6 POLIS_OUT=<dir> \
///   cargo test -p polis-render --release --test decision_salience -- --ignored \
///   --nocapture amber_share
/// ```
#[test]
#[ignore = "reads the operator's real sessions"]
fn amber_share_of_the_bright_ink() {
    let Some(samples) = real_sessions() else {
        return;
    };
    for (label, city, schedule) in samples {
        let mut world = World::new(RepoTree::default(), city.layout.clone());
        world.set_ubiquity(Box::new(polis_world::DenylistUbiquity::default()));
        let origin = Instant::now();
        let mut driver = polis_world::replay::ReplayDriver::with_origin(schedule, origin);
        let mut renderer = FrameRenderer::new(
            &city,
            FrameOptions {
                pixels: 900,
                supersample: 2,
                ..FrameOptions::default()
            },
        );
        let (publisher, reader) = snapshot::from_world(&world);

        // The frame this measurement is about: the busiest moment at which a
        // thread is genuinely blocked on a human. A frame with no pin cannot
        // say anything about the pin's share.
        let mut best: Option<(usize, Canvas, Canvas)> = None;
        let mut steps = 0u64;
        while driver.step(&mut world) {
            steps += 1;
            if !steps.is_multiple_of(97) {
                continue;
            }
            let blocked = world
                .attention
                .iter()
                .filter(|m| {
                    matches!(
                        m.kind,
                        polis_world::attention::AttentionKind::NeedsDecision { .. }
                    )
                })
                .count();
            if blocked == 0 {
                continue;
            }
            publisher.force(&world);
            let snap = reader.load();
            let live_frame = renderer.build_frame(&snap, Duration::from_millis(40));
            let radius = live::glyph_radius(live_frame.unit, live_frame.map_height);
            let (decisions, contended) =
                salience::beacons(&live_frame.attention, radius, live_frame.map_height);
            let frame = renderer.render_owned(&snap, Duration::from_millis(40));
            let mut mask = Canvas::new(frame.width, frame.height, [0, 0, 0]);
            salience::draw(&mut mask, &decisions, radius);
            salience::draw(&mut mask, &contended, radius);
            let score = census(&frame, &mask).0;
            if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
                best = Some((score, frame, mask));
            }
        }
        let Some((_, frame, mask)) = best else {
            eprintln!("  {label}: no frame in this replay had a thread waiting on a human");
            continue;
        };
        let (_, after, before) = census(&frame, &mask);
        eprintln!("  {label}");
        eprintln!("    before the beacon: {}", before.report());
        eprintln!("    after:             {}", after.report());
        write("real-blocked", &frame);
    }
}

/// Bright ink, split the way the review split it.
#[derive(Debug, Clone, Copy, Default)]
struct Bright {
    amber: usize,
    red: usize,
    teal: usize,
    decoration: usize,
}

impl Bright {
    fn total(self) -> usize {
        self.amber + self.red + self.teal + self.decoration
    }

    fn report(self) -> String {
        let t = self.total().max(1) as f64;
        format!(
            "amber {:>6} ({:>5.2}%) · red {:>6} ({:>5.2}%) · teal {:>5} ({:>4.2}%) · decoration {:>6} ({:>5.2}%)",
            self.amber,
            100.0 * self.amber as f64 / t,
            self.red,
            100.0 * self.red as f64 / t,
            self.teal,
            100.0 * self.teal as f64 / t,
            self.decoration,
            100.0 * self.decoration as f64 / t,
        )
    }
}

/// Classifies every pixel above L100 — the review's own threshold — into the
/// four things the top of the range can be carrying, once as drawn and once
/// with the beacons' own pixels removed.
fn census(frame: &Canvas, mask: &Canvas) -> (usize, Bright, Bright) {
    let mut after = Bright::default();
    let mut before = Bright::default();
    for (p, m) in frame
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .zip(mask.pixels.as_chunks::<3>().0)
    {
        let luma = 0.2126f64.mul_add(
            f64::from(p[0]),
            0.7152f64.mul_add(f64::from(p[1]), 0.0722 * f64::from(p[2])),
        );
        if luma <= 100.0 {
            continue;
        }
        let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
        let bucket = if r.min(g) - b >= HOT {
            0
        } else if r > g && r - g.max(b) >= 20 {
            1
        } else if g > r && b > r {
            2
        } else {
            3
        };
        let beacon = m.iter().copied().max().unwrap_or(0) > 0;
        for (target, skip) in [(&mut after, false), (&mut before, beacon)] {
            if skip {
                continue;
            }
            match bucket {
                0 => target.amber += 1,
                1 => target.red += 1,
                2 => target.teal += 1,
                _ => target.decoration += 1,
            }
        }
    }
    (after.amber, after, before)
}

/// The operator's largest real sessions for one repository, each on its own
/// city. Skips cleanly on a machine with no `~/.claude/projects`.
fn real_sessions() -> Option<Vec<(String, City, polis_world::replay::ReplaySchedule)>> {
    real_sessions_with(true)
}

/// The same, with control over PRD-irrelevant idle compression: a measurement
/// *about* waiting must not run on a timeline whose waits have been deleted.
fn real_sessions_with(
    compress: bool,
) -> Option<Vec<(String, City, polis_world::replay::ReplaySchedule)>> {
    use polis_world::sessions::{IndexOptions, SessionIndex};

    let projects = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())?;
    let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
    let want = std::env::var("POLIS_REPO").unwrap_or_else(|_| "qurio-toolset".to_owned());
    let n: usize = std::env::var("POLIS_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let mut candidates: Vec<_> = index
        .sessions
        .iter()
        .filter(|s| s.repo_exists && s.repo.is_some())
        .filter(|s| {
            s.repo
                .as_ref()
                .is_some_and(|r| r.to_string_lossy().contains(&want))
        })
        .cloned()
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    candidates.truncate(n);
    if candidates.is_empty() {
        eprintln!("skipped: no session under a repository matching {want}");
        return None;
    }
    let repo_path = candidates[0].repo.clone()?;
    let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index");
    let city = city::generate_city(repo.tree());
    let mapper = polis_events::PathMapper::new(&repo_path).expect("mapper");
    Some(
        candidates
            .iter()
            .filter_map(|s| {
                let mut schedule = s.schedule(&mapper).ok()?;
                if compress {
                    schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
                }
                Some((
                    format!("{} ({} events)", s.session, schedule.len()),
                    city.clone(),
                    schedule,
                ))
            })
            .collect(),
    )
}
