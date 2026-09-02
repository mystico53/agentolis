//! The overlay's colours, and the contrast budget they are held to (PRD §10.3).
//!
//! > **Dynamic range, not omission.** Draw everything, but keep layers 1–2
//! > inside roughly the bottom fifth of the contrast range and spend the rest on
//! > layers 4–5.
//!
//! `polis-render` enforces that on the base map with a `MapInk` clamp, and it
//! reserves the bands above it: [`plan::CLOUD_BAND`] `49–84`,
//! [`plan::TYPE_BAND`] `85–96`, [`plan::AGENT_BAND`] `97–168`,
//! [`plan::ATTENTION_BAND`] `169–255`. The window draws into three of those
//! bands, so it needs the same enforcement, and for the same reason: a
//! convention is one careless constant away from the render that lost the range.
//!
//! Hence [`Ink`]. Every colour the overlay paints is produced by one of its
//! constructors, each of which clamps into its band; `every_ink_stays_in_its_band`
//! asserts it over every constant in this module. Nothing here can brighten the
//! base map, and nothing but the attention layer can enter the top band.
//!
//! # There is one visual language, not two
//!
//! `polis_render::live` owns the notation — *"a visual language with two
//! definitions has none"* — so every live colour below is that module's
//! constant, re-clamped by [`Ink`] rather than restated. The window and the
//! headless frame renderer are the same picture on two output devices, and a
//! change to the palette moves both.
//!
//! [`Ink::aged`] is the same module's finding, adopted: fading a mark by
//! **alpha** over a near-black map composites it down into the cloud band, so a
//! two-minute-old trail step ends up dimmer than a district label and the
//! layering claim stops being true. Fading by **tone** — walking the colour down
//! toward its own band's floor and stopping there — keeps it inside its
//! allocation for its whole life, and a mark that has aged out is removed rather
//! than dimmed into the map.
//!
//! # Colour is never the only channel
//!
//! PRD §11.4: *"Colour alone is never the sole channel for any state."* So the
//! palette is deliberately thin — the shape of a glyph carries the operation
//! (§10.1), its position carries the subject, and colour carries only the
//! outcome (§10.2) and the layer. Where a state matters it also has a distinct
//! shape: a decision is a pin, contention is a link between two places, done is
//! a ring.

use eframe::egui::Color32;
use polis_events::Outcome;
use polis_render::{live, plan};
use polis_world::ThreadStatus;

/// A colour that has been clamped into one of PRD §10.3's bands.
///
/// The type exists so that the clamp is unavoidable: there is no way to make an
/// `Ink` except through a constructor that names a band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink(Color32);

impl Ink {
    /// The colour, for `egui`.
    pub fn color(self) -> Color32 {
        self.0
    }

    /// The colour at a given opacity.
    ///
    /// Alpha is a blend parameter and not a channel, so it cannot lift a colour
    /// out of its band — a fading trail gets quieter, which is the point, and a
    /// mark at full strength is exactly the band constant.
    pub fn alpha(self, a: f32) -> Color32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        Color32::from_rgba_unmultiplied(self.0.r(), self.0.g(), self.0.b(), a)
    }

    /// Layers 1–2: terrain, city, roads, building footprints. Never brighter
    /// than [`plan::BASE_MAP_CEILING`].
    pub fn base(rgb: [u8; 3]) -> Self {
        Self(rgb_to_color(clamp_to(rgb, 0, plan::BASE_MAP_CEILING)))
    }

    /// Layer 3: territory density (PRD §10.4).
    pub fn cloud(rgb: [u8; 3]) -> Self {
        Self(rgb_to_color(clamp_to(
            rgb,
            plan::CLOUD_BAND.0,
            plan::CLOUD_BAND.1,
        )))
    }

    /// Layer 3t: in-map typography. Brighter than every cloud, dimmer than every
    /// agent mark.
    pub fn typography(rgb: [u8; 3]) -> Self {
        Self(rgb_to_color(clamp_to(
            rgb,
            plan::TYPE_BAND.0,
            plan::TYPE_BAND.1,
        )))
    }

    /// Layer 4: workers, trails, tethers, operation glyphs.
    pub fn agent(rgb: [u8; 3]) -> Self {
        Self(rgb_to_color(clamp_to(
            rgb,
            plan::AGENT_BAND.0,
            plan::AGENT_BAND.1,
        )))
    }

    /// Layer 5: the three attention states. Owns the top of the range.
    pub fn attention(rgb: [u8; 3]) -> Self {
        Self(rgb_to_color(clamp_to(
            rgb,
            plan::ATTENTION_BAND.0,
            plan::ATTENTION_BAND.1,
        )))
    }

    /// Ages a mark **by tone**, toward `floor`, never by alpha.
    ///
    /// `t = 1` is the ink as given; `t = 0` is the same hue sitting exactly on
    /// the floor of its band. This is `polis_render::live::fade`, so the window
    /// and the headless renderer age a trail identically.
    pub fn faded(self, floor: u8, t: f32) -> Color32 {
        rgb_to_color(live::fade(
            [self.0.r(), self.0.g(), self.0.b()],
            floor,
            f64::from(t),
        ))
    }

    /// Ages an agent-band mark: `168 → 97`, then the caller removes it.
    pub fn aged(self, t: f32) -> Color32 {
        self.faded(live::AGENT_FLOOR, t)
    }

    /// Ages an attention-band mark: `255 → 169`, then the caller removes it.
    pub fn aged_attention(self, t: f32) -> Color32 {
        self.faded(live::ATTENTION_FLOOR, t)
    }
}

/// Puts a colour's **peak channel** inside `[lo, hi]`, keeping its hue.
///
/// The band is a claim about a mark's brightness, and brightness is bounded by
/// the largest channel: a colour whose peak is `v` can never be lighter than the
/// grey at `v`, which is the argument `polis_render::plan` makes for a channel
/// ceiling being a sound way to enforce a luminance ceiling. So the whole colour
/// is scaled by one factor rather than each channel being clamped on its own.
///
/// Clamping channel by channel is the bug this replaced: amber
/// `[255, 188, 62]` lifted its blue to the attention floor and came out
/// `[255, 188, 169]`, a pale cream. The floor matters — a colour that sinks
/// below its band reads as belonging to a quieter layer than it does — but it is
/// a floor on the peak, not on every channel.
fn clamp_to(rgb: [u8; 3], lo: u8, hi: u8) -> [u8; 3] {
    let peak = rgb.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return [lo, lo, lo];
    }
    let target = f32::from(peak.clamp(lo.min(hi), hi));
    let scale = target / f32::from(peak);
    let mut out = [0u8; 3];
    for (o, c) in out.iter_mut().zip(rgb) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = (f32::from(c) * scale).round() as u8;
        *o = v.min(hi);
    }
    out
}

fn rgb_to_color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

// ---------------------------------------------------------------------------
// Layers 1–2 — the vector city drawn over the raster at District tier and up
// ---------------------------------------------------------------------------

/// Sea, and the window's background outside the city.
pub fn sea() -> Ink {
    Ink::base([8, 12, 18])
}

/// A building's roof at rest.
pub fn roof() -> Ink {
    Ink::base([44, 44, 46])
}

/// A building's outline.
pub fn roof_edge() -> Ink {
    Ink::base([26, 27, 30])
}

/// The road hierarchy.
pub fn road() -> Ink {
    Ink::base([40, 41, 44])
}

/// A district's boundary — part of PRD §8's wayfinding skeleton, so it is the
/// brightest thing the base band is allowed to hold.
pub fn district_edge() -> Ink {
    Ink::base([48, 48, 48])
}

/// An industrial mass (PRD §8): *"Large, uniform, deliberately dull. Making
/// these boring is the feature — the eye should slide off them."*
pub fn industrial() -> Ink {
    Ink::base([22, 22, 24])
}

/// PRD §9's import streets.
pub fn street() -> Ink {
    Ink::base([34, 38, 46])
}

// ---------------------------------------------------------------------------
// Layer 3 — clouds and typography
// ---------------------------------------------------------------------------

/// The three iso-bands of PRD §10.4, fringe to core.
pub fn cloud_bands() -> [Ink; 3] {
    live::CLOUD_TONES.map(Ink::cloud)
}

/// A district name.
pub fn district_label() -> Ink {
    Ink::typography([92, 92, 96])
}

/// A monument name (PRD §8) — always drawn, so it sits at the top of the type
/// band.
pub fn monument_label() -> Ink {
    Ink::typography([96, 96, 96])
}

/// A file name at [`polis_render::camera::ZoomTier::Building`].
pub fn file_label() -> Ink {
    Ink::typography([84, 86, 90])
}

// ---------------------------------------------------------------------------
// Layer 4 — agents
// ---------------------------------------------------------------------------

/// A trail (PRD §12), at full strength; it is faded by age at draw time.
pub fn trail() -> Ink {
    Ink::agent(live::AGENT_TRAIL)
}

/// A worker's mark.
pub fn worker() -> Ink {
    Ink::agent(live::AGENT_BODY)
}

/// The line from a thread's territory to one of its workers.
pub fn tether() -> Ink {
    Ink::agent(live::AGENT_TETHER)
}

/// PRD §10.2's outcome colour, in the agent band.
///
/// Pending is neutral, done is teal, failed is red. Always paired with a
/// [`polis_events::Glyph`], never used alone (PRD §11.4).
pub fn outcome(outcome: Outcome) -> Ink {
    Ink::agent(live::outcome_ink(outcome))
}

/// PRD §8's scaffolding — a building currently under edit.
pub fn scaffold() -> Ink {
    Ink::agent(live::AGENT_SCAFFOLD)
}

/// The mark at a thread own anchor.
pub fn anchor() -> Ink {
    Ink::agent(live::AGENT_ANCHOR)
}

/// The rail colour for a thread's status. Paired with a text label in the rail,
/// so colour is never the only channel.
pub fn status(status: ThreadStatus) -> Ink {
    match status {
        ThreadStatus::Waiting => Ink::agent([168, 146, 72]),
        ThreadStatus::Working => Ink::agent([120, 150, 168]),
        ThreadStatus::Idle => Ink::agent([112, 112, 116]),
        ThreadStatus::Done => Ink::agent([96, 152, 144]),
    }
}

// ---------------------------------------------------------------------------
// Layer 5 — attention
// ---------------------------------------------------------------------------

/// **(a) Needs decision** — amber, persistent, drawn as a standing pin.
pub fn needs_decision() -> Ink {
    Ink::attention(live::ATTN_DECISION)
}

/// **(b) Done, verified** — teal, decaying.
pub fn done_verified() -> Ink {
    Ink::attention(live::ATTN_DONE)
}

/// **(b) Done, unverified** — really "needs review", and it persists.
pub fn done_unverified() -> Ink {
    Ink::attention([200, 206, 176])
}

/// **(c) Contention** — red, and drawn as a link between two threads.
pub fn contention() -> Ink {
    Ink::attention(live::ATTN_CONTENTION)
}

/// The selection highlight. In the attention band because a selection is the
/// operator's own attention, and it must win against everything under it.
pub fn selection() -> Ink {
    Ink::attention([232, 232, 236])
}

/// The hover highlight, one step below the selection.
pub fn hover() -> Ink {
    Ink::attention([186, 196, 208])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(ink: Ink) -> [u8; 3] {
        let c = ink.color();
        [c.r(), c.g(), c.b()]
    }

    /// A band is a bound on **brightness**, and brightness is the peak channel:
    /// a colour whose largest channel is `v` can never be lighter than the grey
    /// at `v`. So the assertion is "no channel is above the ceiling, and the
    /// peak is at or above the floor" — not "every channel is inside the band",
    /// which would only be satisfiable by greys.
    fn assert_in(ink: Ink, lo: u8, hi: u8, name: &str) {
        let rgb = channels(ink);
        let peak = rgb.iter().copied().max().unwrap_or(0);
        for c in rgb {
            assert!(c <= hi, "{name}: channel {c} is above the ceiling {hi}");
        }
        assert!(
            peak >= lo,
            "{name}: peak {peak} is below the floor {lo} — it would read as a quieter layer"
        );
    }

    /// The whole point of the [`Ink`] type. Every constant in this module, in
    /// the band PRD §10.3 allocates to its layer.
    #[test]
    fn every_ink_stays_in_its_band() {
        for (ink, name) in [
            (sea(), "sea"),
            (roof(), "roof"),
            (roof_edge(), "roof_edge"),
            (road(), "road"),
            (district_edge(), "district_edge"),
            (industrial(), "industrial"),
            (street(), "street"),
        ] {
            assert_in(ink, 0, plan::BASE_MAP_CEILING, name);
        }
        for (i, ink) in cloud_bands().into_iter().enumerate() {
            assert_in(
                ink,
                plan::CLOUD_BAND.0,
                plan::CLOUD_BAND.1,
                &format!("cloud {i}"),
            );
        }
        for (ink, name) in [
            (district_label(), "district_label"),
            (monument_label(), "monument_label"),
            (file_label(), "file_label"),
        ] {
            assert_in(ink, plan::TYPE_BAND.0, plan::TYPE_BAND.1, name);
        }
        for (ink, name) in [
            (trail(), "trail"),
            (worker(), "worker"),
            (tether(), "tether"),
            (scaffold(), "scaffold"),
            (anchor(), "anchor"),
            (outcome(Outcome::Pending), "pending"),
            (outcome(Outcome::Done), "done"),
            (outcome(Outcome::Failed), "failed"),
            (status(ThreadStatus::Waiting), "waiting"),
            (status(ThreadStatus::Working), "working"),
            (status(ThreadStatus::Idle), "idle"),
            (status(ThreadStatus::Done), "thread done"),
        ] {
            assert_in(ink, plan::AGENT_BAND.0, plan::AGENT_BAND.1, name);
        }
        for (ink, name) in [
            (needs_decision(), "needs_decision"),
            (done_verified(), "done_verified"),
            (done_unverified(), "done_unverified"),
            (contention(), "contention"),
            (selection(), "selection"),
            (hover(), "hover"),
        ] {
            assert_in(ink, plan::ATTENTION_BAND.0, plan::ATTENTION_BAND.1, name);
        }
    }

    /// A careless constant cannot walk a layer into the band above it.
    #[test]
    fn the_clamp_catches_a_colour_that_is_too_bright_for_its_layer() {
        assert_in(
            Ink::base([255, 255, 255]),
            0,
            plan::BASE_MAP_CEILING,
            "white base",
        );
        assert_in(
            Ink::agent([255, 0, 0]),
            plan::AGENT_BAND.0,
            plan::AGENT_BAND.1,
            "saturated agent red",
        );
    }

    /// The window and the headless frame renderer must not be two visual
    /// languages. Every live colour here is `polis_render::live`'s constant.
    #[test]
    fn the_live_palette_is_the_renderers_palette_and_not_a_second_one() {
        assert_eq!(channels(trail()), live::AGENT_TRAIL);
        assert_eq!(channels(worker()), live::AGENT_BODY);
        assert_eq!(channels(tether()), live::AGENT_TETHER);
        assert_eq!(channels(scaffold()), live::AGENT_SCAFFOLD);
        assert_eq!(channels(needs_decision()), live::ATTN_DECISION);
        assert_eq!(channels(done_verified()), live::ATTN_DONE);
        assert_eq!(channels(contention()), live::ATTN_CONTENTION);
        for outcome in [Outcome::Pending, Outcome::Done, Outcome::Failed] {
            assert_eq!(
                channels(super::outcome(outcome)),
                live::outcome_ink(outcome)
            );
        }
        assert_eq!(
            cloud_bands().map(channels).to_vec(),
            live::CLOUD_TONES.to_vec()
        );
    }

    /// Fading by tone, not by alpha: an aged mark stays inside its own band for
    /// its whole life instead of compositing down into the one below it.
    #[test]
    fn an_aged_mark_never_leaves_its_band() {
        for t in [1.0f32, 0.75, 0.5, 0.25, 0.0] {
            let c = trail().aged(t);
            let peak = c.r().max(c.g()).max(c.b());
            assert!(
                peak >= plan::AGENT_BAND.0,
                "t={t}: peak {peak} fell out of the agent band"
            );
            assert_eq!(c.a(), 255, "a tone ramp is opaque; an alpha ramp is not");
        }
        let full = trail().aged(1.0);
        assert_eq!([full.r(), full.g(), full.b()], live::AGENT_TRAIL);
    }

    /// And cannot sink one into the band below it either: a band is an
    /// allocation, not just a ceiling.
    #[test]
    fn the_clamp_catches_a_colour_that_is_too_dark_for_its_layer() {
        assert_in(
            Ink::attention([12, 4, 4]),
            plan::ATTENTION_BAND.0,
            plan::ATTENTION_BAND.1,
            "near-black attention",
        );
        assert_in(
            Ink::cloud([0, 0, 0]),
            plan::CLOUD_BAND.0,
            plan::CLOUD_BAND.1,
            "black cloud",
        );
    }

    /// Bands do not overlap, so a mark's layer is readable from its luminance
    /// alone — which is what makes the budget checkable on rendered pixels.
    #[test]
    fn the_bands_are_disjoint_and_ordered() {
        assert!(plan::BASE_MAP_CEILING < plan::CLOUD_BAND.0);
        assert!(plan::CLOUD_BAND.1 < plan::TYPE_BAND.0);
        assert!(plan::TYPE_BAND.1 < plan::AGENT_BAND.0);
        assert!(plan::AGENT_BAND.1 < plan::ATTENTION_BAND.0);
    }
}
