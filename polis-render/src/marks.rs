//! Layer 5 — the attention layer (PRD §10.3, §11.2, §11.4).
//!
//! > **Attention** — the three states. Owns the top of the contrast range.
//!
//! Three rendering rules, all from PRD §11:
//!
//! * **Needs decision** is a standing pin above the building or district, amber,
//!   persistent.
//! * **Done** is teal and decaying — main agents only — with the unverified
//!   variant persisting because it is really "needs review".
//! * **Contention** is a **link joining two threads across the map**, not a badge
//!   on a dot. It is the only state that can pull the eye to two places at once,
//!   which is exactly why it must not be drawn as a point.
//!
//! # Peripheral perception (PRD §11.4)
//!
//! > Peripheral vision is poor at colour and good at motion onset.
//!
//! * The **arrival** of a mark is a brief pulse, at most 400 ms. That is what
//!   catches the eye when the operator is not looking at the screen.
//! * Its **steady state** is shape and position, decoded once they turn their
//!   head.
//! * **Colour alone is never the sole channel for any state.**

use eframe::wgpu;
use polis_layout::Point;
use polis_world::attention::Attention;

/// The attention layer.
#[derive(Debug)]
pub struct MarkLayer {
    _private: (),
}

impl MarkLayer {
    /// Uploads this frame's marks, computing pulse phase from arrival time.
    pub fn upload(&mut self, queue: &wgpu::Queue, marks: &[Attention], now_secs: f64) {
        let _ = (queue, marks, now_secs);
        todo!("PRD §11.4 — pulse on arrival, at most 400 ms, then shape and position")
    }

    /// Draws pins and links.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'static>) {
        let _ = pass;
        todo!("PRD §10.3 — layer 5, top of the contrast range")
    }
}

/// A contention link, drawn across the map between two threads.
#[derive(Debug, Clone, Copy)]
pub struct ContentionLink {
    /// One thread's position.
    pub a: Point,
    /// The other's.
    pub b: Point,
    /// Severity, driving weight and not only colour.
    pub severity: polis_world::contention::Severity,
}

/// The drift mark (PRD §10.4).
///
/// > Compute a drift vector from the centre of mass over the last 60s and render
/// > a leading-edge mark when its magnitude exceeds a threshold. **This is the
/// > redirect signal, and it is the one thing here that no existing tool
/// > provides.**
#[derive(Debug, Clone, Copy)]
pub struct DriftMark {
    /// Where the territory's leading edge currently is.
    pub at: Point,
    /// Direction and magnitude of the migration.
    pub vector: Point,
}
