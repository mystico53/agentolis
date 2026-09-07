//! One thread, one colour, and no two live threads the same one (PRD §11.4).
//!
//! > **Colour alone is never the sole channel for any state.** (PRD §11.4)
//!
//! The operator's report was blunt: *"the same color is super bad, can you make
//! sure the colors have to be different?"*. Two live sessions had been handed
//! the same hue by `polis_render::live::thread_slot`, an FNV hash modulo twelve
//! whose own doc-comment accepted collisions on purpose. The world now assigns
//! the slot once per thread, exclusively, and stores it on
//! [`polis_world::Thread::tint`].
//!
//! Two claims, and neither can be seen from a unit test of the ring:
//!
//! 1. **exclusivity** — the first twelve threads of a world are mutually
//!    distinct, and a thread's colour never moves once assigned;
//! 2. **parity** — the window and the headless renderer paint one recording
//!    identically. Those two drive the world through different tick cadences
//!    (`ReplayDriver::advance` ticks once per call and the window calls it many
//!    times; `run_to_end` applies the whole schedule and ticks exactly once), so
//!    retirement — which is tick-driven — interleaves with thread creation
//!    differently in each. That is why a slot is **never released**: the ring
//!    records which id took each one and keeps it, so neither "is this slot
//!    free" nor "how many times has this thread been created" can depend on how
//!    often the world was ticked. `polis_app::palette` names the failure that
//!    would otherwise follow, in as many words — a colour that meant one thread
//!    in the window and another in a recorded GIF *"would be worse than no
//!    colour at all"*.
//!
//! Claim 2 needs a real schedule and a real driver, which is why this is an
//! integration test and not an inline one.
//!
//! # The layer is the same claim, one field over
//!
//! [`polis_world::Thread::layer`] is the id the renderer groups cloud kernels
//! by, the id `polis_render::live::CloudTween` matches its two sides on, and the
//! index the tint table above is looked up in. It is assigned in the same breath
//! as the tint, for the same reason and with the same never-released rule — so
//! the tests for it live here, beside the ones whose argument they borrow. What
//! it does **not** share is the twelve-slot ceiling: two threads may end up the
//! same colour, and must never end up on the same layer, because two territories
//! on one layer are summed and PRD §6.4's crowd signal goes quiet.
//!
//! Every test here was checked against four sabotages of the ring — recycling a
//! released slot, forgetting whose a slot was, skipping the reset in
//! `World::reset`, and not assigning at all — and each sabotage fails at least
//! one of them. Do not weaken an assertion without re-running that check.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use polis_events::{
    Channel, Event, EventKind, EventMeta, HookEvent, HookPayload, Payload, RecordedEvent,
    RecordingHeader, SessionId, ThreadId, WallTime, WorktreeId, IDENTITY_SLOTS, RECORDING_FORMAT,
};
use polis_ingest::transcript::ParseStats;
use polis_layout::CityLayout;
use polis_world::replay::{ReplayDriver, ReplaySchedule, DEFAULT_IDLE_GAP_CAP};
use polis_world::{World, THREAD_RETIRE_AFTER};

const REPO: &str = "C:/repo";

fn world() -> World {
    let mut w = World::for_replay(CityLayout::default());
    w.mapper_mut()
        .add_worktree(WorktreeId::PRIMARY, std::path::Path::new(REPO))
        .expect("primary root");
    w
}

fn thread(session: &str) -> ThreadId {
    ThreadId::of_session(SessionId::new(session))
}

/// A `SessionStart` hook for one session — the cheapest event that creates a
/// thread and stamps it alive.
fn session_start(session: &str, at: Instant) -> Event {
    let payload = HookPayload {
        session_id: Some(SessionId::new(session)),
        hook_event_name: Some("SessionStart".to_owned()),
        cwd: Some(REPO.to_owned()),
        ..HookPayload::default()
    };
    let mut meta = EventMeta::now(Channel::Hook).with_session(SessionId::new(session));
    meta.observed = at;
    Event::new(
        meta,
        Payload::Hook(Box::new(HookEvent {
            kind: EventKind::SessionStart,
            truncated: false,
            payload,
        })),
    )
}

fn recorded(offset_ms: u64, session: &str) -> RecordedEvent {
    RecordedEvent {
        wall: WallTime::from_unix_millis(i64::try_from(offset_ms).expect("offset fits")),
        monotonic_offset_ms: offset_ms,
        // The driver rebases `meta.observed` onto its own origin, so the
        // instant here is a placeholder and only `monotonic_offset_ms` is read.
        event: session_start(session, Instant::now()),
    }
}

/// Session ids `s01` … `s13`. Twelve is [`IDENTITY_SLOTS`], so a thirteenth
/// distinct thread is the one that has to fall back.
fn session_name(n: usize) -> String {
    format!("s{n:02}")
}

/// The thread that stays alive for the whole recording, holding the slot the
/// late arrival wants. `"a"` and `"polis"` share a hue preference — that is the
/// collision the operator reported, pinned by literals in `polis_events::ids`.
const HOLDER: &str = "a";
/// The late arrival, which prefers [`HOLDER`]'s slot and cannot have it.
const LATECOMER: &str = "polis";

/// A recording built to force the two drivers apart, if anything can.
///
/// * [`HOLDER`] starts at `t=0` and takes its preferred slot. `s02` … `s12`
///   follow inside the first second and fill the remaining eleven, so all
///   twelve are spoken for.
/// * [`HOLDER`] speaks again every ten minutes, so it is never quiet for
///   [`THREAD_RETIRE_AFTER`] under *any* tick cadence. `s02` … `s12` say nothing
///   more.
/// * `s02` speaks again at forty minutes. Under a driver that ticks as it goes,
///   `s02` has been retired by then and is rebuilt; under `run_to_end` nothing
///   has been ticked and it was never retired at all. That is the "a thread is
///   created twice in one driver and once in the other" case.
/// * [`LATECOMER`] arrives a minute after that, wanting a slot [`HOLDER`] is
///   still holding. This is the case a *recycling* ring gets wrong in the most
///   visible way: in the window the eleven retirements have freed slots, so the
///   probe walks off [`HOLDER`]'s slot onto a recycled one; in the headless run
///   nothing was ever freed, so it falls back to its bare preference. Two
///   different colours for one thread on one recording.
///
/// Idle gaps are compressed to [`DEFAULT_IDLE_GAP_CAP`] so the playback timeline
/// is a few seconds while the world still ages forty-one minutes.
fn divergent_schedule() -> ReplaySchedule {
    let minute = 60 * 1_000;
    let mut events = vec![recorded(0, HOLDER)];
    for n in 2..=12 {
        events.push(recorded((n as u64 - 1) * 100, &session_name(n)));
    }
    for m in [10, 20, 30, 40] {
        events.push(recorded(m * minute, HOLDER));
    }
    events.push(recorded(40 * minute + 30_000, &session_name(2)));
    events.push(recorded(41 * minute, LATECOMER));
    ReplaySchedule::from_events(
        RecordingHeader {
            format: RECORDING_FORMAT,
            wall_origin: WallTime::UNIX_EPOCH,
            producer: "identity_hue".to_owned(),
        },
        events,
        ParseStats::default(),
        Vec::new(),
    )
    .with_idle_gap_cap(Some(DEFAULT_IDLE_GAP_CAP))
}

/// Every thread the world holds, and the colour it is drawn in.
fn tints(world: &World) -> BTreeMap<String, u8> {
    world
        .threads
        .values()
        .map(|t| (t.id.as_str().to_owned(), t.tint))
        .collect()
}

/// The headless renderer's driver: apply everything, tick once.
fn headless(schedule: ReplaySchedule, origin: Instant) -> World {
    let mut world = world();
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    driver.run_to_end(&mut world);
    world
}

/// The window's driver: advance by real elapsed time, over and over, ticking
/// every time (`polis_app::app` calls this once per frame).
fn windowed(schedule: ReplaySchedule, origin: Instant) -> World {
    let mut world = world();
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    driver.clock_mut().play();
    let mut guard = 0;
    while !driver.is_finished() {
        driver.advance(Duration::from_millis(200), &mut world);
        guard += 1;
        assert!(guard < 1_000, "the driver never reached the end");
    }
    world
}

/// One event at a time, ticking after each — the scrubber's own driver, and the
/// finest tick cadence the product has.
fn stepped(schedule: ReplaySchedule, origin: Instant) -> World {
    let mut world = world();
    let mut driver = ReplayDriver::with_origin(schedule, origin);
    while driver.step(&mut world) {}
    world
}

/// The assertion that only exists as prose today, made mechanical: the same
/// recording, played by the window's driver and by the headless one, paints
/// every thread the same colour.
///
/// This is the test the reuse design could not pass. With a recycled ring the
/// window frees `s02` … `s12`'s slots at the forty-minute tick and hands one to
/// `s13`; the headless run never ticks in the middle and gives `s13` its bare
/// preference instead.
#[test]
fn the_window_and_the_headless_renderer_paint_one_recording_identically() {
    let origin = Instant::now();
    let a = tints(&headless(divergent_schedule(), origin));
    let b = tints(&windowed(divergent_schedule(), origin));
    let c = tints(&stepped(divergent_schedule(), origin));

    // The schedule is built so all three end with the same threads alive.
    assert!(
        a.contains_key(HOLDER) && a.contains_key(LATECOMER) && a.contains_key("s02"),
        "the fixture stopped exercising the case it was written for: {a:?}"
    );
    assert_eq!(
        a, b,
        "the window and the headless renderer disagree on colour"
    );
    assert_eq!(a, c, "stepping one event at a time changed a colour");
}

/// Past twelve, and in every driver alike, the thirteenth thread falls back to
/// its bare preference rather than to a recycled slot — the price of never
/// releasing one, asserted rather than assumed.
///
/// The exhaustion count is asserted beside it because it is the second half of
/// the parity claim: `s02` is retired and rebuilt by two of these three drivers
/// and not by the third, and a ring that treated the rebuild as a fresh claim
/// would read 2 here where the headless run reads 1.
#[test]
fn the_thirteenth_thread_of_a_recording_takes_its_bare_preference_in_every_driver() {
    let origin = Instant::now();
    let want = thread(LATECOMER).hue_preference();
    for (name, world) in [
        ("headless", headless(divergent_schedule(), origin)),
        ("window", windowed(divergent_schedule(), origin)),
        ("stepped", stepped(divergent_schedule(), origin)),
    ] {
        let t = world
            .thread(&thread(LATECOMER))
            .expect("the latecomer is live");
        assert_eq!(
            t.tint, want,
            "{name}: a slot the ring had already spent was handed out again"
        );
        assert_eq!(
            t.tint,
            world
                .thread(&thread(HOLDER))
                .expect("the holder is live")
                .tint,
            "{name}: past twelve the collision is certain, and this is it"
        );
        assert_eq!(
            world.health.identity_hues_exhausted, 1,
            "{name}: exhaustion has to be visible, not silent"
        );
    }
}

/// A backwards seek resets the world and re-applies from the start. If the ring
/// survived the reset, every thread in the re-applied city would find its slot
/// burnt and take its bare preference — the same city, silently repainted, with
/// nothing failing anywhere near the cause.
#[test]
fn replay_colours_the_same_city_twice() {
    let origin = Instant::now();
    let mut world = world();
    let mut driver = ReplayDriver::with_origin(divergent_schedule(), origin);
    driver.run_to_end(&mut world);
    let first = tints(&world);
    assert!(!first.is_empty(), "nothing was replayed");

    // `World::reset` on its own.
    world.reset(origin);
    let mut driver = ReplayDriver::with_origin(divergent_schedule(), origin);
    driver.run_to_end(&mut world);
    assert_eq!(tints(&world), first, "a reset world painted a second city");
    assert_eq!(
        world.health.identity_hues_exhausted, 1,
        "the exhaustion count is a per-run counter, not a total"
    );

    // And through the seek that actually performs one: forward to the end,
    // backwards past every thread creation, forward again.
    let mut world = self::world();
    let mut driver = ReplayDriver::with_origin(divergent_schedule(), origin);
    let end = driver.schedule().duration_ms();
    driver.seek(end, &mut world);
    let forward = tints(&world);
    driver.seek(0, &mut world);
    driver.seek(end, &mut world);
    assert_eq!(
        tints(&world),
        forward,
        "seeking backwards past a thread creation repainted the city"
    );
}

/// The reported defect, in the smallest form that reproduces it: `"a"` and
/// `"polis"` both hash to slot 4 (pinned in `polis_events::ids`), and a world
/// holding both must not draw them the same colour.
///
/// Then the other half of the promise — an unrelated thread arriving or leaving
/// moves nobody's colour, which is the rule the hash was chosen for in the first
/// place and which an assignment could easily have broken.
#[test]
fn two_threads_that_want_the_same_hue_get_different_ones() {
    let at = Instant::now();
    let mut w = world();
    assert_eq!(
        thread("a").hue_preference(),
        thread("polis").hue_preference(),
        "the fixture stopped reproducing the collision"
    );

    w.apply(&session_start("a", at));
    w.apply(&session_start("polis", at + Duration::from_secs(1)));
    let a = w.thread(&thread("a")).expect("a").tint;
    let p = w.thread(&thread("polis")).expect("polis").tint;
    assert_ne!(a, p, "two live threads were handed one colour");
    assert_eq!(a, thread("a").hue_preference(), "the first keeps its wish");
    assert_eq!(p, (a + 1) % IDENTITY_SLOTS, "the probe walks one slot up");

    // A third thread, and neither of the first two moves.
    w.apply(&session_start("third", at + Duration::from_secs(2)));
    assert_eq!(w.thread(&thread("a")).expect("a").tint, a);
    assert_eq!(w.thread(&thread("polis")).expect("polis").tint, p);

    // And one of them leaves. The survivor is untouched — the slot is not
    // returned to the ring, so nothing shuffles up into the gap.
    w.dismiss_thread(&thread("a"));
    assert_eq!(
        w.thread(&thread("polis")).expect("polis").tint,
        p,
        "an unrelated thread ending repainted a live one"
    );

    // A dismissed thread whose session speaks again is rebuilt under the same
    // `ThreadId` — `dismiss_thread` is deliberately not a tombstone — and it is
    // the same session, so it gets the same colour. That is also what keeps the
    // window and a recorded frame in step: whether a thread is retired and
    // rebuilt mid-run depends on tick cadence, and this is what makes the answer
    // not depend on it.
    w.apply(&session_start("a", at + Duration::from_secs(3)));
    assert_eq!(
        w.thread(&thread("a")).expect("a returns").tint,
        a,
        "a session that came back was repainted"
    );
    assert_eq!(
        w.health.identity_hues_exhausted, 0,
        "a returning thread must be a lookup, not a fresh claim"
    );
}

/// A reset world is a fresh world, hues included.
///
/// The ring is keyed on thread ids, so re-applying the *same* schedule over a
/// stale ring would come out right by accident — see `World::reset`. What goes
/// wrong is a reset onto a **different** set of sessions: twelve slots still
/// spoken for by twelve ids that no longer exist, and every thread in the new
/// city painted in its bare preference.
#[test]
fn a_reset_world_hands_out_hues_as_a_fresh_one_does() {
    let at = Instant::now();
    let mut w = world();
    for n in 1..=12 {
        w.apply(&session_start(&session_name(n), at));
    }
    assert_eq!(w.health.identity_hues_exhausted, 0, "twelve fit exactly");

    w.reset(at);
    w.apply(&session_start(HOLDER, at));
    w.apply(&session_start(LATECOMER, at + Duration::from_secs(1)));
    assert_ne!(
        w.thread(&thread(HOLDER)).expect("holder").tint,
        w.thread(&thread(LATECOMER)).expect("latecomer").tint,
        "a reset world was still holding the previous city's colours"
    );
    assert_eq!(
        w.health.identity_hues_exhausted, 0,
        "a reset world cannot start exhausted"
    );
}

/// Twelve threads, twelve colours, and the thirteenth says so out loud.
///
/// `THREAD_RETIRE_AFTER` is only referenced to make the point that these are all
/// live at once: nothing in this test is old enough to be retired.
#[test]
fn past_twelve_live_threads_colour_degrades_to_the_preference() {
    let at = Instant::now();
    let mut w = world();
    for n in 1..=13 {
        w.apply(&session_start(&session_name(n), at));
    }
    assert!(
        THREAD_RETIRE_AFTER > Duration::from_secs(1),
        "nothing here has had time to retire"
    );

    let live: Vec<u8> = (1..=12)
        .map(|n| {
            w.thread(&thread(&session_name(n)))
                .expect("live thread")
                .tint
        })
        .collect();
    let mut distinct = live.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        IDENTITY_SLOTS as usize,
        "the first twelve threads of a world must be mutually distinct: {live:?}"
    );
    assert!(
        live.iter().all(|t| *t < IDENTITY_SLOTS),
        "no thread may carry `live::NO_TINT`, or the map would draw it neutral"
    );

    let thirteenth = w
        .thread(&thread(&session_name(13)))
        .expect("thirteenth")
        .tint;
    assert_eq!(
        thirteenth,
        thread(&session_name(13)).hue_preference(),
        "past the ring, the fallback is the bare preference and nothing else"
    );
    assert_eq!(w.health.identity_hues_exhausted, 1);
}

// ---------------------------------------------------------------------------
// The cloud layer — `Thread::layer`
// ---------------------------------------------------------------------------

/// Past twelve threads the colour repeats and the layer must not.
///
/// This is the one place the two ids come apart, and it is why the renderer
/// cannot simply group by the tint: `past_twelve_live_threads_colour_degrades_to
/// _the_preference` asserts that the thirteenth thread shares a hue with a live
/// one, and a thirteenth thread sharing a *layer* would have its territory
/// summed into that thread's — one cloud where there are two, and no crowd
/// signal on the ground they share.
#[test]
fn two_threads_never_share_a_layer_even_past_twelve() {
    let at = Instant::now();
    let mut w = world();
    for n in 1..=13 {
        w.apply(&session_start(&session_name(n), at));
    }
    let layers: Vec<u16> = (1..=13)
        .map(|n| w.thread(&thread(&session_name(n))).expect("thread").layer)
        .collect();
    let mut sorted = layers.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        13,
        "two threads on one layer: their territories will be summed as one — {layers:?}"
    );
    assert!(
        w.health.identity_hues_exhausted > 0,
        "this test is only about the case where the hue has already run out"
    );
}

/// A thread keeps its layer across a retirement, which is what keeps the two
/// renderers in step.
///
/// The argument is `HueRing`'s in full: retirement is tick-driven, the window
/// and `run_to_end` tick differently, so a thread is created twice in one and
/// once in the other. A monotonic counter would hand the rebuilt thread a second
/// layer in the window only — and the layer is what the tween matches on, so the
/// window would ease the returning cloud out of nothing while the recorded frame
/// carried it straight through.
#[test]
fn a_session_that_comes_back_keeps_its_layer() {
    let at = Instant::now();
    let mut w = world();
    w.apply(&session_start("a", at));
    w.apply(&session_start("polis", at + Duration::from_secs(1)));
    let a = w.thread(&thread("a")).expect("a").layer;
    let p = w.thread(&thread("polis")).expect("polis").layer;
    assert_ne!(a, p, "two live threads on one layer");

    w.dismiss_thread(&thread("a"));
    assert_eq!(
        w.thread(&thread("polis")).expect("polis").layer,
        p,
        "an unrelated thread ending moved a live one to another layer"
    );

    w.apply(&session_start("a", at + Duration::from_secs(3)));
    assert_eq!(
        w.thread(&thread("a")).expect("a returns").layer,
        a,
        "a session that came back was put on a different layer"
    );
}

/// A reset world hands out layers as a fresh one does.
///
/// Same argument as the hue's reset test, and the same failure if it is skipped:
/// the table would keep growing across a backwards seek, so the ids a replay
/// hands the renderer would depend on how many times it had been scrubbed.
#[test]
fn a_reset_world_hands_out_layers_as_a_fresh_one_does() {
    let at = Instant::now();
    let mut w = world();
    for n in 1..=12 {
        w.apply(&session_start(&session_name(n), at));
    }
    let before: Vec<u16> = (1..=12)
        .map(|n| w.thread(&thread(&session_name(n))).expect("thread").layer)
        .collect();

    w.reset(at);
    for n in 1..=12 {
        w.apply(&session_start(&session_name(n), at));
    }
    let after: Vec<u16> = (1..=12)
        .map(|n| w.thread(&thread(&session_name(n))).expect("thread").layer)
        .collect();
    assert_eq!(
        before, after,
        "a reset world carried the previous city's layer table"
    );
}
