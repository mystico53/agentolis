//! `polis-render` — the renderer (PRD §10, §13, §14).
//!
//! # This crate draws on the CPU, and depends on no GPU stack at all
//!
//! PRD §13 specifies "Rust + `wgpu`, native, with `winit`", and ADR-0012 worked
//! out how this crate would take both through eframe. That path was declared —
//! a `PolisRenderer` behind `egui_wgpu`'s callback contract, a base-map pass, an
//! offscreen density pass, an agent layer and an attention layer — and it was
//! never implemented. Every body in it was a `todo!()`. What ships instead, and
//! has shipped since M2, is [`plan`] rasterising the base map into one texture
//! and [`live`] painting layers 3–5 over it, both onto a [`raster::Canvas`] of
//! plain sRGB bytes. That is the whole renderer. The declarations were removed
//! rather than left standing, because a public type whose every method panics
//! reads as a component that exists (ADR-0110).
//!
//! So this crate now has no `eframe`, `wgpu`, `winit`, `lyon` or `bytemuck`
//! dependency. The GPU stack enters the workspace exactly once, in `polis-app`,
//! which is the crate that opens a window.
//!
//! # PRD §10.3's contrast budget, and where it is decided
//!
//! The budget — layers 1–2 inside the bottom fifth of the contrast range, the
//! rest reserved for layers 4–5 — is **defined and enforced in [`plan`]**, in
//! 8-bit sRGB channel space, as [`plan::BASE_MAP_CEILING`] plus the reserved
//! bands [`plan::CLOUD_BAND`], [`plan::TYPE_BAND`], [`plan::AGENT_BAND`] and
//! [`plan::ATTENTION_BAND`]. Read that module before adding any colour to this
//! crate; the ceiling is a channel bound precisely so that it survives blending
//! and downsampling, which is what makes it enforceable rather than aspirational.
//!
//! [`plan::TYPE_BAND`] is the one that is easy to skip past and is the one a
//! review caught: text is not one of §10.3's five layers, and the first version
//! read that as an exemption and drew district labels at `L 142` — inside the
//! band reserved for live agents, on the image built to validate the budget. It
//! now has an allocation of its own between the clouds and the agents, and
//! `plan`'s `nothing_in_the_map_frame_enters_the_agent_band` asserts it on
//! rendered pixels.
//!
//! ADR-0021 asked for the colour-space convention to be settled before that
//! tuning, and on the one path that exists it is: the rasteriser writes sRGB
//! bytes straight into the file, with no surface format in the way. The whole
//! class of bug that ADR needed guarding against — an offscreen
//! `Rgba8UnormSrgb` target encoding differently from eframe's backend-dependent
//! non-sRGB surface, so identical output looks darker in the window than
//! headless — cannot arise, because there is one encoder and it is this one.
//!
//! # The live layer, and where to find it
//!
//! Layers 3–5 — clouds, agents and attention — are drawn by [`live`], which
//! owns the whole of PRD §10's notation: the six operation glyphs, the three
//! outcome colours, the trail notations, the tether, the revisit rosette, the
//! scaffolding and the three attention states. It draws onto a [`raster::Canvas`]
//! and knows nothing about any GPU API, which is deliberate: the notation had to
//! be decidable in M2 without a window (PRD §15), and a notation that only
//! exists inside a GPU pipeline cannot be diffed, tested on pixels, or recorded.
//!
//! [`frame`] is the headless frame renderer — a world snapshot and a time in, an
//! image out — and [`gif`] records a sequence of them. Together they are the M2
//! iteration loop:
//!
//! ```no_run
//! use polis_render::{frame::{FrameOptions, FrameRenderer}, gif};
//! # fn demo(city: &polis_layout::city::City, reader: &polis_world::snapshot::SnapshotReader) {
//! let mut renderer = FrameRenderer::new(city, FrameOptions::default());
//! let dt = std::time::Duration::from_millis(42);
//! let frames: Vec<_> = (0..96)
//!     .map(|_| renderer.render_owned(&reader.load(), dt))
//!     .collect();
//! gif::write(std::path::Path::new("replay.gif"), &frames, 4).unwrap();
//! # }
//! ```
//!
//! **The one rule that is easy to get wrong, on any path**: fading is a **tone**
//! ramp toward the band floor, never an alpha ramp. Alpha-blending a live mark
//! over Polis's near-black base map composites it *into* the map's own band —
//! measured, a trail at `α = 0.55` lands at channel 69, dimmer than a district
//! label. See [`live`]'s module docs.

pub mod frame;
pub mod gif;
pub mod live;
pub mod pacing;
pub mod plan;
pub mod raster;
pub mod salience;
pub mod zoom;
