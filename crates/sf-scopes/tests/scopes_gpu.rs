//! Real headless-GPU tests for the scope passes (lavapipe, no mocks).
//!
//! Every test runs the actual WGSL scope shaders and compares against the
//! CPU reference in `sf_scopes::cpu`, which is the exact math contract both
//! sides implement.

use sf_render::RenderContext;
use sf_scopes::cpu;
use sf_scopes::{ScopeKind, ScopeParams, ScopePass, SCOPE_H, SCOPE_W};

/// Headless context + scope pass. Forces the lavapipe Vulkan ICD so the
/// tests run on any machine without a physical GPU.
fn ctx() -> (RenderContext, ScopePass) {
    std::env::set_var("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json");
    let rc = RenderContext::new_headless().expect("headless render context");
    let sp = ScopePass::new(&rc).expect("scope pass");
    (rc, sp)
}

/// Quantizes an f32 RGB frame to RGBA8 bytes (what the GPU actually sees).
fn to_bytes(px: &[[f32; 3]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(px.len() * 4);
    for p in px {
        bytes.push((p[0] * 255.0).round().clamp(0.0, 255.0) as u8);
        bytes.push((p[1] * 255.0).round().clamp(0.0, 255.0) as u8);
        bytes.push((p[2] * 255.0).round().clamp(0.0, 255.0) as u8);
        bytes.push(255);
    }
    bytes
}

/// Uploads an f32 RGB frame as RGBA8 (quantized, like a real graded frame).
fn upload_f32(rc: &RenderContext, px: &[[f32; 3]], w: u32, h: u32) -> wgpu::Texture {
    assert_eq!(px.len(), (w * h) as usize);
    rc.upload_rgba8(w, h, &to_bytes(px)).unwrap()
}

fn to_f32(px: &[u8]) -> Vec<[f32; 3]> {
    px.chunks_exact(4)
        .map(|c| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
            ]
        })
        .collect()
}

fn max_abs_diff_f32(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    a.iter()
        .zip(b.iter())
        .flat_map(|(x, y)| {
            [
                (x[0] - y[0]).abs(),
                (x[1] - y[1]).abs(),
                (x[2] - y[2]).abs(),
            ]
        })
        .fold(0.0f32, f32::max)
}

/// 8 vertical color bars (white, yellow, cyan, green, magenta, red, blue,
/// black) at 256x144 — 32 px per bar.
fn color_bars_256x144() -> Vec<[f32; 3]> {
    let bars: [[f32; 3]; 8] = [
        [1.0, 1.0, 1.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [0.0, 1.0, 0.0],
        [1.0, 0.0, 1.0],
        [1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, 0.0],
    ];
    let mut px = Vec::with_capacity((SCOPE_W * SCOPE_H) as usize);
    for _ in 0..SCOPE_H {
        for x in 0..SCOPE_W {
            px.push(bars[(x / 32) as usize]);
        }
    }
    px
}

#[test]
fn downscale_agrees_with_cpu() {
    let (rc, sp) = ctx();
    // 512x288 -> 256x144: a genuine 2x box downscale.
    let mut px = Vec::with_capacity(512 * 288);
    for y in 0..288u32 {
        for x in 0..512u32 {
            px.push([
                x as f32 / 511.0,
                y as f32 / 287.0,
                ((x + y) % 256) as f32 / 255.0,
            ]);
        }
    }
    let src = upload_f32(&rc, &px, 512, 288);
    let small = sp.downscale(&rc, &src).unwrap();
    assert_eq!((small.size().width, small.size().height), (256, 144));
    let back = rc.download_rgba8(&small).unwrap();

    let cpu = cpu::downscale_box(&to_f32(&to_bytes(&px)), 512, 288);
    let gpu = to_f32(&back);
    let max = max_abs_diff_f32(&gpu, &cpu);
    println!("downscale_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 1.0 / 255.0 + 1e-6, "downscale diverged: {max}");
}

#[test]
fn waveform_agrees_with_cpu() {
    let (rc, sp) = ctx();
    let px = color_bars_256x144();
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.waveform(&rc, &small).unwrap();
    let cpu = cpu::waveform(&cpu::downscale_box(&to_f32(&to_bytes(&px)), 256, 144));

    let mut max = 0.0f32;
    for x in 0..256 {
        for (a, b) in [
            (gpu.luma_min[x], cpu.luma_min[x]),
            (gpu.luma_max[x], cpu.luma_max[x]),
            (gpu.r_min[x], cpu.r_min[x]),
            (gpu.r_max[x], cpu.r_max[x]),
            (gpu.g_min[x], cpu.g_min[x]),
            (gpu.g_max[x], cpu.g_max[x]),
            (gpu.b_min[x], cpu.b_min[x]),
            (gpu.b_max[x], cpu.b_max[x]),
        ] {
            max = max.max((a - b).abs());
        }
    }
    println!("waveform_agrees_with_cpu: max abs diff = {max}");
    assert!(max <= 1e-6, "waveform diverged: {max}");
    // Sanity: the white bar (columns 0..32) must read full-scale luma.
    assert!((gpu.luma_max[0] - 1.0).abs() < 1e-6);
    // The black bar (columns 224..256) must read zero.
    assert!(gpu.luma_max[255] < 1e-6);
}

#[test]
fn vectorscope_agrees_with_cpu() {
    let (rc, sp) = ctx();
    let px = color_bars_256x144();
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.vectorscope(&rc, &small).unwrap();
    let cpu = cpu::vectorscope(&to_f32(&to_bytes(&px)));
    assert_eq!(gpu.bins, cpu.bins, "vectorscope bins must match exactly");
    let total: u32 = gpu.bins.iter().sum();
    assert_eq!(
        total,
        SCOPE_W * SCOPE_H,
        "every pixel lands in exactly one bin"
    );
}

#[test]
fn vectorscope_skin_tone_lands_near_guide() {
    let (rc, sp) = ctx();
    // Solid skin-tone patch.
    let px = vec![[200.0 / 255.0, 150.0 / 255.0, 120.0 / 255.0]; (256 * 144) as usize];
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.vectorscope(&rc, &small).unwrap();
    let peak = gpu
        .bins
        .iter()
        .enumerate()
        .max_by_key(|&(_, &b)| b)
        .map(|(i, _)| i)
        .unwrap();
    // Expected from the CPU formula: bx=53, by=77 for this patch.
    let cpu = cpu::vectorscope(&to_f32(&to_bytes(&px)));
    let cpu_peak = cpu
        .bins
        .iter()
        .enumerate()
        .max_by_key(|&(_, &b)| b)
        .map(|(i, _)| i)
        .unwrap();
    assert_eq!(peak, cpu_peak, "GPU and CPU must agree on the peak bin");
    assert_eq!(peak, 77 * 128 + 53, "skin patch peak bin mismatch");
    println!("skin-tone peak bin = {peak} (bx=53, by=77); guide = 123°");
}

#[test]
fn histogram_agrees_with_cpu() {
    let (rc, sp) = ctx();
    // 256x144 gradient: r = x/255, g = y/143, b = 0.5.
    let mut px = Vec::with_capacity((SCOPE_W * SCOPE_H) as usize);
    for y in 0..SCOPE_H {
        for x in 0..SCOPE_W {
            px.push([x as f32 / 255.0, y as f32 / 143.0, 0.5]);
        }
    }
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.histogram(&rc, &small).unwrap();
    let cpu = cpu::histogram(&to_f32(&to_bytes(&px)));
    assert_eq!(gpu.r, cpu.r, "red bins must match exactly");
    assert_eq!(gpu.g, cpu.g);
    assert_eq!(gpu.b, cpu.b);
    assert_luma_histogram_close(&gpu.luma, &cpu.luma);
    assert_eq!(gpu.r.iter().sum::<u32>(), SCOPE_W * SCOPE_H);
}

/// Luma histogram agreement with an honest tolerance.
///
/// Luma is the only scope input computed with arithmetic (everything else
/// is a direct channel read), so a 1-ulp difference between the GPU and CPU
/// evaluation order — e.g. a backend fusing the multiply-adds — can move an
/// isolated pixel sitting exactly on a bin boundary into the neighboring
/// bin. Each moved pixel contributes 2 to the total absolute bin-count
/// difference; the limit below allows at most 2 such pixels out of 36864.
fn assert_luma_histogram_close(gpu: &[u32; 256], cpu: &[u32; 256]) {
    let mut moved = 0u32;
    for i in 0..256 {
        moved += (gpu[i] as i32 - cpu[i] as i32).unsigned_abs();
    }
    println!("assert_luma_histogram_close: moved-pixel mass = {moved} (limit 4)");
    assert!(
        moved <= 4,
        "luma histogram moved {moved} pixels of mass (limit 4)"
    );
    assert_eq!(
        gpu.iter().sum::<u32>(),
        cpu.iter().sum::<u32>(),
        "luma histogram totals must agree"
    );
}

#[test]
fn false_color_bands_agree_with_cpu() {
    let (rc, sp) = ctx();
    // Six horizontal bands, one per false-color band (interior values).
    let bands = [1.0f32, 0.9, 0.36, 0.18, 0.10, 0.01];
    let mut px = Vec::with_capacity((SCOPE_W * SCOPE_H) as usize);
    for y in 0..SCOPE_H {
        let v = bands[(y / 24) as usize];
        for _ in 0..SCOPE_W {
            px.push([v, v, v]);
        }
    }
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.false_color(&rc, &small).unwrap();
    let cpu = cpu::false_color(&to_f32(&to_bytes(&px)));
    assert_eq!(gpu.width, 256);
    assert_eq!(gpu.height, 144);
    assert_eq!(gpu.rgb, cpu.rgb, "false-color image must match exactly");
    // Spot-check the band colors top to bottom.
    let at = |y: u32| {
        [
            gpu.rgb[(y * 256 * 3) as usize],
            gpu.rgb[(y * 256 * 3 + 1) as usize],
            gpu.rgb[(y * 256 * 3 + 2) as usize],
        ]
    };
    assert_eq!(at(0), [255, 0, 0]);
    assert_eq!(at(24), [255, 255, 0]);
    assert_eq!(at(48), [0, 255, 0]);
    assert_eq!(at(72), [128, 128, 128]);
    assert_eq!(at(96), [0, 255, 255]);
    assert_eq!(at(120), [0, 0, 255]);
}

#[test]
fn zebra_thresholds_agree_with_cpu() {
    let (rc, sp) = ctx();
    let params = ScopeParams::default(); // hi 0.95, lo 0.05
                                         // Three bands: clipped white, clean gray, crushed black.
    let mut px = Vec::with_capacity((SCOPE_W * SCOPE_H) as usize);
    for y in 0..SCOPE_H {
        let v = if y < 48 {
            1.0
        } else if y < 96 {
            0.5
        } else {
            0.02
        };
        for _ in 0..SCOPE_W {
            px.push([v, v, v]);
        }
    }
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let gpu = sp.zebra(&rc, &small, &params).unwrap();
    let cpu = cpu::zebra(&to_f32(&to_bytes(&px)), params.zebra_hi, params.zebra_lo);
    assert_eq!(gpu.rgba, cpu.rgba, "zebra overlay must match exactly");
    // Spot-checks: (0,0) stripe cell white; (8,0) gap black; mid band clear.
    assert_eq!(&gpu.rgba[0..4], &[255, 255, 255, 191]);
    assert_eq!(&gpu.rgba[8 * 4..8 * 4 + 4], &[0, 0, 0, 191]);
    let mid = (72 * 256 * 4) as usize;
    assert_eq!(&gpu.rgba[mid..mid + 4], &[0, 0, 0, 0]);
}

#[test]
fn analyze_dispatches_all_kinds() {
    let (rc, sp) = ctx();
    let px = color_bars_256x144();
    let src = upload_f32(&rc, &px, SCOPE_W, SCOPE_H);
    let small = sp.downscale(&rc, &src).unwrap();
    let params = ScopeParams::default();
    for kind in ScopeKind::all() {
        let payload = sp.analyze(&rc, kind, &small, &params).unwrap();
        assert_eq!(
            payload.kind(),
            kind,
            "analyze must return the requested kind"
        );
    }
}
