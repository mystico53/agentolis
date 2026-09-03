//! The GPU cloud layer, run on a real adapter (PRD §10.4, §13).
//!
//! `docs/verified/gpu-stack.md` §5 records the finding that only a numeric
//! readback catches, and it is the one this file exists to keep caught:
//!
//! > Before the hot cluster existed, the GPU field maxed at `0.9888` —
//! > suspiciously equal to one lone splat's peak. Rather than assume, the same
//! > field was modelled in Python on the CPU: it also produced `0.9888`. GPU and
//! > CPU agreeing to four decimals proves the blend sums correctly and that
//! > nothing clamps; the scene simply had no real overlap yet. **Keep that CPU
//! > cross-check in `polis-render`'s test suite — it is the only way to
//! > distinguish "additive blending is broken" from "your kernels do not
//! > overlap."**
//!
//! Here the CPU model is not Python, it is [`polis_render::live::CloudField`],
//! which is the layer the window actually draws today. So the cross-check does
//! double duty: it proves the blend, and it proves the two implementations of
//! PRD §10.4 have not drifted apart.
//!
//! Every test skips, loudly, when no adapter can be had — a build machine with
//! no GPU is not a failing renderer.

// The readback and the histogram are one linear script each, on purpose: the
// order the passes are encoded in is part of what is being tested.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use std::future::Future;
use std::task::{Context, Poll};

use eframe::wgpu;
use polis_render::density::{DensityField, IsoParams, SplatInstance, GRID};
use polis_render::live::{CloudField, CloudKernel, CLOUD_CROWD, CLOUD_ISO};

/// A futures executor in twelve lines, because `polis-render` declares no async
/// runtime and a `Cargo.toml` is not this agent's to edit.
///
/// Every wgpu future here is ready on the first poll on native; the yield is
/// there so a driver that genuinely defers cannot spin a core.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// An adapter, a device and a queue, or `None` on a machine with no GPU.
fn gpu() -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    // `Features::empty()` and the downlevel defaults on purpose: the cloud
    // renderer must need no optional capability at all, and asking for one here
    // would hide the day it started to.
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("polis-density-test"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        ..Default::default()
    }))
    .ok()?;
    Some((adapter, device, queue))
}

/// IEEE-754 binary16 to `f32`. The accumulator is `Rgba16Float` and this crate
/// has no half-float dependency.
fn half(bits: u16) -> f32 {
    let sign = f32::from(bits >> 15);
    let exp = i32::from((bits >> 10) & 0x1f);
    let frac = f32::from(bits & 0x3ff);
    let magnitude = match exp {
        0 => frac * 2.0f32.powi(-24),
        0x1f => f32::INFINITY,
        _ => (1.0 + frac / 1024.0) * 2.0f32.powi(exp - 15),
    };
    if sign > 0.0 {
        -magnitude
    } else {
        magnitude
    }
}

/// Reads a whole texture back as f32 quadruples.
fn read_rgba16f(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    size: u32,
) -> Vec<[f32; 4]> {
    let row = u64::from(size) * 8;
    assert_eq!(row % 256, 0, "the readback row must be aligned");
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: row * u64::from(size),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row as u32),
                rows_per_image: Some(size),
            },
        },
        wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| {
        r.expect("map the readback buffer");
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().expect("mapped range");
    let out = data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            let v = |i: usize| half(u16::from_le_bytes([c[i * 2], c[i * 2 + 1]]));
            [v(0), v(1), v(2), v(3)]
        })
        .collect();
    drop(data);
    buffer.unmap();
    out
}

/// The iso pass's target edge, in pixels.
const SIDE: u32 = 512;

/// City space is the unit square; the field covers it exactly.
const CAMERA: [[f32; 4]; 4] = [
    [2.0, 0.0, 0.0, 0.0],
    [0.0, -2.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    // Column-major: the translation is the last **row** of the Rust literal.
    [-1.0, 1.0, 0.0, 1.0],
];

/// Two territories, the second overlapping the first.
fn scene() -> Vec<SplatInstance> {
    let mut out = Vec::new();
    for (thread, cx) in [(0u16, 0.42f32), (1, 0.58)] {
        for i in 0..6i32 {
            let a = i as f32 * 0.618_034;
            out.push(SplatInstance {
                centre: [(a.fract() - 0.5).mul_add(0.06, cx), 0.5 + a.fract() * 0.04],
                radius: 0.16,
                weight: 1.0,
                thread,
            });
        }
    }
    out
}

/// The same scene through [`CloudField`], on a pixel lattice the size of the
/// GPU's texture, so the two can be compared texel for texel.
fn cpu_field(splats: &[SplatInstance]) -> CloudField {
    let n = f64::from(GRID);
    let kernels: Vec<CloudKernel> = splats
        .iter()
        .map(|s| CloudKernel {
            at: [f64::from(s.centre[0]) * n, f64::from(s.centre[1]) * n],
            radius: f64::from(s.radius) * n,
            weight: f64::from(s.weight),
            thread: s.thread,
        })
        .collect();
    CloudField::sample(&kernels, GRID as usize, GRID as usize).expect("a field")
}

/// `docs/verified/gpu-stack.md` §5's cross-check, and the reason it is a test
/// and not a screenshot: a picture cannot tell a broken blend from a scene with
/// no overlap in it.
#[test]
fn the_gpu_field_is_the_cpu_field_and_the_blend_really_adds() {
    let Some((adapter, device, queue)) = gpu() else {
        eprintln!("skipped: no GPU adapter on this machine");
        return;
    };
    eprintln!("adapter: {:?}", adapter.get_info());

    let splats = scene();
    let mut field = DensityField::new(&device, &adapter, wgpu::TextureFormat::Rgba8UnormSrgb);
    eprintln!("density format: {:?}", field.report());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    field.encode(
        &device,
        &queue,
        &mut encoder,
        CAMERA,
        &splats,
        IsoParams::default(),
    );
    queue.submit([encoder.finish()]);

    let texels = read_rgba16f(&device, &queue, field.accumulator_texture(), GRID);
    let peak = texels.iter().fold(0.0f32, |m, t| m.max(t[0]));
    let crowd = texels.iter().fold(0.0f32, |m, t| m.max(t[1]));
    eprintln!("gpu peak {peak:.4}, crowd {crowd}");

    // The field is unbounded: six coincident full-weight kernels sum to about
    // six, and nothing on the way clamps them to one.
    assert!(
        peak > 2.0,
        "the additive blend did not sum: peak {peak} over a cluster of six"
    );

    // And it is the *same* field the window draws: 6.5703 on the GPU against
    // 6.5783 on the CPU, 0.12 % apart, which is f16 accumulation and the CPU
    // path's coarse lattice and nothing else. Two percent is the tolerance
    // because the two agree on the *field*, not on the interpolation of it.
    let cpu = cpu_field(&splats);
    let cpu_peak = cpu.peak();
    eprintln!(
        "cpu peak {cpu_peak:.4} against gpu {peak:.4} — {:.2}%",
        100.0 * (peak - cpu_peak).abs() / cpu_peak
    );
    assert!(
        (peak - cpu_peak).abs() < cpu_peak * 0.02,
        "GPU and CPU disagree about the field: {peak} against {cpu_peak}"
    );

    // The crowd channel: two territories claim the middle, and neither claims
    // the far edges.
    assert!(
        crowd >= f32::from(CLOUD_CROWD),
        "the crowd channel never reached {CLOUD_CROWD}: got {crowd}"
    );
    let contested = texels
        .iter()
        .filter(|t| t[1] >= f32::from(CLOUD_CROWD) - 0.5)
        .count();
    let banded = texels
        .iter()
        .filter(|t| t[0] >= CLOUD_ISO[0] as f32)
        .count();
    eprintln!("banded {banded} texels, contested {contested}");
    assert!(contested > 0 && contested < banded, "{contested}/{banded}");
}

/// PRD §10.4: *"2–3 levels, never a continuous blur"*, and PRD §10.3's rule that
/// a cloud is a set of marks the city shows through.
///
/// Both are measured on the pixels the GPU produced, which is the only place
/// either claim can be checked: a `smoothstep` in place of the `if` chain would
/// produce thousands of distinct colours, and a filled band would leave no
/// background at all.
#[test]
fn the_gpu_bands_are_discrete_marks_and_not_a_wash() {
    let Some((adapter, device, queue)) = gpu() else {
        eprintln!("skipped: no GPU adapter on this machine");
        return;
    };
    let splats = scene();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut field = DensityField::new(&device, &adapter, format);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("iso-target"),
        size: wgpu::Extent3d {
            width: SIDE,
            height: SIDE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    field.encode(
        &device,
        &queue,
        &mut encoder,
        CAMERA,
        &splats,
        IsoParams {
            contour: [2.0, 1.0, 1.0],
            viewport: [SIDE as f32, SIDE as f32],
            ..IsoParams::default()
        },
    );
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("iso"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The stand-in base map: one flat tone, so anything that
                        // is not it came from the cloud layer.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        field.draw_bands(&mut pass);
    }
    queue.submit([encoder.finish()]);

    // Readback: 512 × 4 bytes is 2048, already aligned.
    let row = u64::from(SIDE) * 4;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: row * u64::from(SIDE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row as u32),
                rows_per_image: Some(SIDE),
            },
        },
        wgpu::Extent3d {
            width: SIDE,
            height: SIDE,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().expect("mapped range");
    let mut histogram: std::collections::BTreeMap<[u8; 3], usize> =
        std::collections::BTreeMap::new();
    for px in data.as_chunks::<4>().0 {
        *histogram.entry([px[0], px[1], px[2]]).or_default() += 1;
    }
    drop(data);
    buffer.unmap();

    let background = *histogram
        .iter()
        .max_by_key(|(_, n)| **n)
        .expect("a histogram")
        .0;
    let inked: usize = histogram
        .iter()
        .filter(|(c, _)| **c != background)
        .map(|(_, n)| *n)
        .sum();
    eprintln!(
        "{} distinct colours, {inked} inked of {}",
        histogram.len(),
        SIDE * SIDE
    );
    assert!(inked > 500, "the GPU cloud layer drew almost nothing");
    // Background plus at most three iso tones, with a little slack for the
    // rounding a non-sRGB conversion can introduce. A `smoothstep` would put
    // thousands here — this is the regression test §10.4 asks for.
    assert!(
        histogram.len() <= 8,
        "the bands are not discrete: {} distinct colours",
        histogram.len()
    );
    // And the city shows through: the marks are a minority of everything they
    // are drawn among.
    assert!(
        inked * 4 < (SIDE * SIDE) as usize,
        "the cloud layer filled a quarter of the frame"
    );
}
