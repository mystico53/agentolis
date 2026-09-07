//! The explanation of the map, in one place (PRD §10, §12).
//!
//! Polis has four ways in — bare `polis`, `polis map`, `polis watch`, a
//! double-clicked `polis.exe` — and until this module existed the four sentences
//! that say what a building is were printed by exactly one of them, on stdout,
//! which is the surface the other three never show. A first-run review put it
//! plainly: *"you can pan and zoom a dark diagram and that is all"*.
//!
//! So the strings live here, once, and both the terminal screen
//! ([`crate::setup::orientation`]) and the window ([`crate::ui::help`] and the
//! first-run overlay) render the same words. Changing the metaphor means
//! changing it here and nowhere else.
//!
//! # The dismissal is remembered beside the first-run marker
//!
//! [`remember_dismissed`] writes `map-explained` into the state directory that
//! already holds `first-run` (`%LOCALAPPDATA%\polis` on Windows). A failed write
//! is silent and costs one extra overlay, which is not worth an error message on
//! the way to a window.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the city is, in the fewest sentences that still answer the question.
///
/// Wrapped for a terminal at 78 columns, which is also a comfortable measure in
/// the window, so one array serves both.
pub const MAP: &[&str] = &[
    "Every building is a file. Every district is a directory.",
    "A building's height is its uncommitted work, so the tallest tower",
    "is the biggest unreviewed pile — the skyline points at what needs you.",
    "The old, dense core is the code you wrote first; the loose outskirts",
    "are last month's.",
];

/// [`MAP`] as one paragraph, for a surface that wraps text itself.
///
/// The array is wrapped for a terminal; a window is a different width every
/// time, so it re-wraps rather than inheriting a 78-column ragged edge.
pub fn paragraph() -> String {
    MAP.join(" ")
}

/// The same thing as a table, for the surfaces that have room for one.
///
/// The map view uses it as a legend; the terminal does not print it, because
/// [`MAP`] already says it in prose and saying it twice on one screen is noise.
pub const READING: &[(&str, &str)] = &[
    ("a building", "one file"),
    (
        "a district",
        "one folder — its buildings are the files in it",
    ),
    (
        "height",
        "uncommitted work: the tallest tower is the biggest unreviewed pile",
    ),
    (
        "the dense core",
        "the code written first; the loose outskirts are the newest",
    ),
    (
        "a cloud",
        "an agent, over the part of the tree it is working in",
    ),
];

/// The one line that says what the operator can do about the overlay.
pub const DISMISS: &str = "press any key or click to dismiss · h brings it back";

/// The name of the marker file, beside `first-run`.
const MARKER: &str = "map-explained";

/// The state directory's marker path, when there is a state directory.
fn marker() -> Option<PathBuf> {
    crate::config::Config::default_state_dir().map(|dir| dir.join(MARKER))
}

/// Whether the map explainer has already been dismissed on this machine.
pub fn dismissed() -> bool {
    marker().is_some_and(|p| p.exists())
}

/// Records that it has. Failure is silent — see the module docs.
pub fn remember_dismissed() {
    let Some(path) = marker() else { return };
    let Some(parent) = path.parent() else { return };
    remember_dismissed_in(parent);
}

/// [`dismissed`] against an explicit state directory, for tests.
pub fn dismissed_in(state_dir: &Path) -> bool {
    state_dir.join(MARKER).exists()
}

/// [`remember_dismissed`] against an explicit state directory, for tests.
pub fn remember_dismissed_in(state_dir: &Path) {
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(
        state_dir.join(MARKER),
        "the map explainer has been shown and dismissed\n",
    );
}

// ---------------------------------------------------------------------------
// The live half: why *this* thread looks the way it does, right now.
// ---------------------------------------------------------------------------

/// One mark on the map, what it is doing for one thread **now**, and the rule
/// that makes it do that.
///
/// [`MAP`] and [`READING`] say what the city is, once, for everybody. They
/// cannot answer the question an operator actually asks, which is about one
/// thread and this minute: *why is that cloud one flat shade, why is a circle
/// crawling across it and slowing down, what is the arrow, why is it raining
/// where there is no cloud.* Those are four separate mechanisms with four exact
/// answers, every one of them a number the world is already holding — and until
/// this existed none of them was on any surface. The operator's report was
/// *"this is all a bit cryptic"*, and it was a report about four things at once.
///
/// So a reading is three parts on purpose:
///
/// * [`Reading::mark`] — the thing, named the way somebody pointing at the
///   screen would name it. Not "the anchor": *the ring in the middle*.
/// * [`Reading::says`] — what it reads for **this** thread, this frame. That is
///   the part which makes the panel worth opening twice.
/// * [`Reading::because`] — the rule, with its constants in it. Numbers, because
///   a number is checkable against the map and an adjective is not: *"the middle
///   shade starts at 1.60 and this thread is at 0.92"* is a claim the operator
///   can go and confirm, and *"the cloud is faint"* is not.
#[derive(Debug)]
pub struct Reading {
    /// The mark, in the words somebody pointing at the screen would use.
    pub mark: &'static str,
    /// What it currently reads for this thread.
    pub says: String,
    /// The rule behind it, with the constants that decide it.
    pub because: String,
}

/// Every mark this thread is putting on the map, with the reason for each.
///
/// Ordered the way the eye arrives: the cloud first, because it is the biggest
/// thing and the slowest, then the two marks inside it that move, then the
/// weather, then the hands, then what has been left behind. A mark this thread
/// is **not** drawing is included only where its absence is itself the question
/// — most threads have no arrow, and "there is no arrow" is the answer to "what
/// is the arrow", so that row stays.
///
/// Every number here is read from the world rather than restated from memory:
/// the shade thresholds are [`polis_render::live::CLOUD_ISO`], the half-life is
/// [`polis_world::territory::DECAY_HALF_LIFE`], the ripple's life is
/// [`crate::mapview::RAIN_LIFE`]. Retuning any of them rewrites this panel in
/// the same commit, which is the only way an explanation stays true.
#[must_use]
// One ordered list of readings, in the order the eye arrives at the marks.
// Splitting it would produce four helpers called once each, in a fixed order,
// and the order is the only thing holding them together — the same reasoning
// `crate::mapview` states for its layer stack.
#[allow(clippy::too_many_lines)]
pub fn thread_reading(thread: &polis_world::Thread, now: Instant) -> Vec<Reading> {
    use polis_render::live::CLOUD_ISO;
    use polis_world::territory::{
        Placement, ANCHOR_GLIDE, DECAY_HALF_LIFE, DORMANT_AFTER, DRIFT_WINDOW,
    };

    let mut out = Vec::new();
    let t = &thread.territory;
    let secs = |d: Duration| format!("{} seconds", d.as_secs());

    // --- The cloud, and why it is one flat shade ------------------------
    //
    // The report — "clouds build up very slow, no movement inside, just a
    // single layer" — is an accurate reading of a real mechanism, and the
    // mechanism is `RESTING_WEIGHT`: a converged territory that goes quiet is
    // held at a peak of 0.9, which is above the 0.55 fringe and below the 1.60
    // middle. So *every* resting cloud is one shade, by construction, and no
    // amount of looking at it will say so. The number says it.
    let peak = f64::from(t.field_peak());
    let shades = CLOUD_ISO.iter().filter(|iso| peak >= **iso).count();
    // A converged territory that has gone quiet is *held* at a drawable shade
    // rather than fading out, and an operator who does not know that reads a
    // resting cloud as a working one. Only a converged one is held, so an
    // unplaced thread is not told a rule that is not being applied to it.
    let resting = t
        .placement()
        .is_somewhere()
        .then(|| t.quiet_for().filter(|q| *q > Duration::from_secs(30)))
        .flatten()
        .map_or_else(String::new, |quiet| {
            format!(
                " Nothing has landed for {}: the cloud is being held at the faintest shade \
                 instead of fading away, and it leaves the map after {} minutes of silence.",
                crate::format::duration(quiet),
                DORMANT_AFTER.as_secs() / 60,
            )
        });
    let because = format!(
        "Shade is how much work has piled onto the same few files. One call counts 1.0 where it \
         lands and spreads over its neighbours; the outer shade starts at {:.2}, the middle at \
         {:.2}, the core at {:.2}. This thread is at {peak:.2} at its busiest point, and that \
         halves every {} of quiet — which is why a cloud takes a while to build, and why most of \
         them never get past the outer shade.{resting}",
        CLOUD_ISO[0],
        CLOUD_ISO[1],
        CLOUD_ISO[2],
        secs(DECAY_HALF_LIFE),
    );
    out.push(Reading {
        mark: "the cloud",
        says: match shades {
            0 => "not drawn".to_owned(),
            1 => "one shade of three, the faintest".to_owned(),
            2 => "two shades of three".to_owned(),
            _ => "all three shades".to_owned(),
        },
        because,
    });

    // --- Its size, which is the question "why is it expanding" ----------
    out.push(Reading {
        mark: "its size",
        says: match t.placement() {
            Placement::Claim(claim) => format!("over {}", claim.as_str()),
            Placement::Lobes(lobes) => format!("over {} places at once", lobes.len()),
            Placement::Nowhere => "nowhere yet".to_owned(),
        },
        because: "The outline is the spread of the files this thread has touched, not an \
                  animation playing. It widens when a call lands on a file further out and pulls \
                  back as the older ones fade, so a cloud creeping outwards is a thread whose \
                  work is spreading."
            .to_owned(),
    });

    // --- The ring in the middle: the circle that crosses, and slows -----
    //
    // `ANCHOR_GLIDE` is an exponential relaxation, so the ring covers 63 % of
    // the distance in one time constant and then creeps. That is exactly what
    // was reported — "a circle moving inside the cloud, then slowing down" —
    // and it is the notation working rather than a stutter.
    let glide = match (t.anchor(), t.drawn_anchor()) {
        (Some(target), Some(drawn)) => drawn.distance(target),
        _ => 0.0,
    };
    let sliding = glide > t.bandwidth() * 0.05;
    out.push(Reading {
        mark: "the ring in the middle",
        says: if sliding {
            "sliding to a new centre".to_owned()
        } else if thread.status == polis_world::ThreadStatus::Working {
            "still, and the dot inside means working".to_owned()
        } else {
            "still".to_owned()
        },
        because: format!(
            "The ring sits on the busiest single point of the work, never the average of it. \
             When that point moves the ring is not allowed to jump: it slides about two thirds \
             of the way in {}, then creeps the rest. That is the circle crossing the cloud and \
             slowing down.",
            secs(ANCHOR_GLIDE),
        ),
    });

    // --- The arrow ------------------------------------------------------
    out.push(Reading {
        mark: "the arrow",
        says: t.drift_mark().map_or_else(
            || "not drawn, so its work is staying put".to_owned(),
            |m| format!("drawn, {:.1}x past the threshold", m.ratio),
        ),
        because: format!(
            "It appears when the middle of the work has been sliding one way for the last {}, \
             and it points where that is heading — heavier the further past the threshold it \
             goes. This is the redirect signal: a thread walking out of one folder into another, \
             visible before anything collides.",
            secs(DRIFT_WINDOW),
        ),
    });

    // --- Rain, and why it lands where there is no cloud ------------------
    //
    // The second half of this is the whole answer to "even outside clouds":
    // rungs 2 and 3 of `place`'s chain are the majority of calls, and neither
    // one of them lands on a file.
    let open = thread
        .ops
        .iter()
        .rev()
        .take_while(|op| now.saturating_duration_since(op.at) < crate::mapview::RAIN_LIFE)
        .count();
    let minute = thread
        .ops
        .iter()
        .rev()
        .take_while(|op| now.saturating_duration_since(op.at) < Duration::from_secs(60))
        .count();
    let recent = thread.ops.len().min(RECENT_CALLS);
    let unnamed = thread
        .ops
        .iter()
        .rev()
        .take(recent)
        .filter(|op| op.placement.rung() > 1)
        .count();
    out.push(Reading {
        mark: "the rings that open and fade",
        says: format!("{open} on screen · {minute} calls in the last minute"),
        because: format!(
            "One ring per tool call, at the place that call landed, opening over {:.1} seconds. \
             Only the rate means anything: heavy rain is a busy thread and none at all is a \
             thread calling nothing. {unnamed} of its last {recent} calls named no file — a \
             shell command, a search, a subagent — so they are drawn on the folder the command \
             ran in, or on the agent itself. That is why rings land where there is no cloud.",
            crate::mapview::RAIN_LIFE.as_secs_f32(),
        ),
    });

    // --- The hands ------------------------------------------------------
    if !thread.workers.is_empty() {
        out.push(Reading {
            mark: "the triangles",
            says: format!(
                "{} of {} subagents running",
                thread.running_workers(),
                thread.workers.len()
            ),
            because: "One triangle per subagent, bigger and brighter while it is running. Each \
                      one slides to its next file rather than jumping. Hover or click this row \
                      and a line joins every one of them back to the ring."
                .to_owned(),
        });
    }

    // --- What it has left behind ----------------------------------------
    let written = thread.visits.values().filter(|v| v.writes > 0).count();
    if written > 0 {
        out.push(Reading {
            mark: "the small rings that stay",
            says: format!("{written} files changed"),
            because: format!(
                "A read leaves nothing behind; a write leaves a ring on that file's own building \
                 for as long as the thread lives. Up to {} of them are drawn, and the count here \
                 is all of them.",
                crate::mapview::WRITE_RINGS_PER_THREAD,
            ),
        });
    }

    if thread.failures > 0 {
        out.push(Reading {
            mark: "the red marks",
            says: format!("{} of {} calls failed", thread.failures, thread.tool_calls),
            because: "Red is a failure or a refusal, and it carries a ring wide enough to read \
                      from across the room. A failure that named no file still shows: it lands \
                      on the folder the command ran in, or on the agent."
                .to_owned(),
        });
    }

    out
}

/// How many of a thread's latest calls the rain reading counts to say how many
/// of them named no file.
///
/// Twenty. The claim it supports is *"this is why rings land away from the
/// cloud"*, which is about the rain currently on screen — a lifetime ratio over
/// six hundred calls would be a different, duller statement, and one that stops
/// moving.
const RECENT_CALLS: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    use polis_events::{Glyph, LogicalPath, Outcome, SessionId, ThreadId, ToolKind};
    use polis_layout::Point;
    use polis_world::place::OpPlacement;
    use polis_world::territory::Territory;
    use polis_world::{Observation, Operation, PathScope, Thread, ThreadStatus};

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    /// A thread working in `src/auth`, with three calls behind it of which two
    /// named no file — the ordinary shape, and the one the questions were asked
    /// about.
    fn working(now: Instant) -> Thread {
        let session = SessionId::new("reading");
        let mut thread = Thread::new(ThreadId::of_session(session.clone()), session.clone(), now);
        thread.status = ThreadStatus::Working;

        let mut territory = Territory::for_extent(1000.0);
        for (i, path) in ["src/auth/token.rs", "src/auth/session.rs"]
            .into_iter()
            .enumerate()
        {
            let tool = ToolKind::Edit;
            territory.observe(
                &Observation {
                    thread: ThreadId::of_session(session.clone()),
                    worker: None,
                    path: lp(path),
                    scope: PathScope::for_tool(&tool),
                    tool,
                    at: now,
                    weight: None,
                },
                1.0,
                #[allow(clippy::cast_precision_loss)] // two small test indices
                Some(Point::new(i as f32, 0.0)),
            );
        }
        thread.territory = territory;

        for placement in [
            OpPlacement::Path(lp("src/auth/token.rs")),
            OpPlacement::Cwd(lp("src/auth")),
            OpPlacement::Agent,
        ] {
            thread.ops.push_back(Operation {
                path: placement.path().cloned(),
                placement,
                tool: ToolKind::Bash,
                glyph: Glyph::FilledSquare,
                outcome: Outcome::Done,
                worker: None,
                at: now,
                tool_use: None,
            });
        }
        thread.tool_calls = 3;
        thread
    }

    /// The four questions, verbatim from the report that caused this panel:
    /// *"why is the cloud slowly expanding"*, *"there is a circle moving inside
    /// the cloud, then slowing down. what does that mean?"*, *"what does the
    /// arrow mean?"*, and rain *"even outside clouds"*.
    ///
    /// A reading that stops naming one of them has stopped answering it, and
    /// the operator is back to guessing.
    #[test]
    fn every_mark_that_was_asked_about_has_a_row() {
        let now = Instant::now();
        let marks: Vec<&str> = thread_reading(&working(now), now)
            .iter()
            .map(|r| r.mark)
            .collect();
        for wanted in [
            "the cloud",
            "its size",
            "the ring in the middle",
            "the arrow",
            "the rings that open and fade",
        ] {
            assert!(marks.contains(&wanted), "no row for {wanted:?}: {marks:?}");
        }
    }

    /// The cloud row exists to answer *"just a single layer"*, and it can only
    /// answer it with numbers: the three thresholds, and where this thread sits
    /// against them. Prose alone would be the caption the panel replaced.
    #[test]
    fn the_cloud_row_gives_the_thresholds_and_this_threads_own_value() {
        let now = Instant::now();
        let readings = thread_reading(&working(now), now);
        let cloud = readings
            .iter()
            .find(|r| r.mark == "the cloud")
            .expect("a cloud row");
        for level in ["0.55", "1.60", "3.20"] {
            assert!(
                cloud.because.contains(level),
                "the {level} shade is not named: {}",
                cloud.because
            );
        }
        let peak = format!("{:.2}", working(now).territory.field_peak());
        assert!(
            cloud.because.contains(&peak),
            "the thread's own value {peak} is missing: {}",
            cloud.because
        );
    }

    /// *"the rain drops jump very fast. even outside clouds"* — the second half
    /// of that is `place`'s fallback chain, and this row is the only surface
    /// that says so. It has to count **this** thread's calls, or it is a
    /// generality.
    #[test]
    fn the_rain_row_counts_the_calls_that_named_no_file() {
        let now = Instant::now();
        let readings = thread_reading(&working(now), now);
        let rain = readings
            .iter()
            .find(|r| r.mark == "the rings that open and fade")
            .expect("a rain row");
        assert!(
            rain.because.contains("2 of its last 3 calls named no file"),
            "the pathless calls are not counted: {}",
            rain.because
        );
        assert_eq!(
            rain.says, "3 on screen · 3 calls in the last minute",
            "three calls, three ripples, and the rate beside them"
        );
    }

    /// The same rule [`MAP`] is held to, for the same reason: this is read by
    /// somebody who has just asked what a circle is, and a word they would have
    /// to look up is a second unanswered question.
    #[test]
    fn the_readings_carry_no_jargon() {
        let now = Instant::now();
        let text = thread_reading(&working(now), now)
            .iter()
            .map(|r| format!("{} {} {}", r.mark, r.says, r.because))
            .collect::<Vec<_>>()
            .join(" ");
        for jargon in [
            "territory",
            "kernel",
            "iso-contour",
            "anchor",
            "PRD",
            "OTLP",
            "rung",
        ] {
            assert!(
                !text.to_lowercase().contains(jargon.to_lowercase().as_str()),
                "jargon {jargon:?} in:\n{text}"
            );
        }
    }

    /// The reviewer's actual complaint was that the words exist and are good and
    /// nothing shows them. Whatever else changes, these four claims are the
    /// product's whole metaphor and must survive editing.
    #[test]
    fn the_explanation_says_what_the_three_things_are() {
        let text = MAP.join(" ");
        for claim in [
            "building is a file",
            "district is a directory",
            "uncommitted work",
            "core is the code you wrote first",
        ] {
            assert!(text.contains(claim), "missing {claim:?} in:\n{text}");
        }
        // A first-run overlay is read in one glance or not at all.
        assert!(MAP.len() <= 6, "{} lines is not a glance", MAP.len());
        for line in MAP {
            assert!(line.chars().count() <= 78, "too wide to wrap well: {line}");
        }
        // No jargon: this is the first English a new operator reads.
        for jargon in ["OTLP", "territory", "PRD", "kernel", "iso-contour"] {
            assert!(!text.contains(jargon), "jargon {jargon:?} in:\n{text}");
        }
    }

    #[test]
    fn the_dismissal_is_remembered_in_the_state_directory() {
        let dir = crate::testutil::scratch("explain-marker").join("state");
        assert!(!dismissed_in(&dir), "a fresh machine has not seen it");
        remember_dismissed_in(&dir);
        assert!(dismissed_in(&dir), "and afterwards it has");
        // Beside the first-run marker, not somewhere else.
        assert!(dir.join(MARKER).is_file());
    }

    /// A state directory that cannot be created must not stop the window
    /// opening, and must not leave the marker somewhere else instead — an
    /// earlier version of this test wrote `map-explained` into the crate root,
    /// where the next `polis map` grew a building for it.
    #[test]
    fn a_state_directory_that_cannot_be_written_is_silent() {
        let root = crate::testutil::scratch("explain-readonly");
        let blocked = root.join("a-file");
        std::fs::write(&blocked, "not a directory\n").expect("write");
        let inside = blocked.join("state");
        remember_dismissed_in(&inside);
        assert!(
            !dismissed_in(&inside),
            "nothing was written, and that is fine"
        );
        assert!(blocked.is_file(), "and the file in the way is untouched");
    }
}
