//! End-to-end tests of waterfall loops over per-lane descriptors: the
//! synthetic GFX10 programs of `resource::tests` lowered to DXIL, validated
//! by `dxil.dll` when present, and run on the host GPU and WARP, each lane
//! reading the resource its own descriptor names.

use windows::Win32::Graphics::Direct3D12::*;

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// Lowers a wave32 compute program of 32 threads with the table of views at
/// s0 (textures, or with `buffers` structured buffers of dwords, at entries
/// 0 and 1, a raw UAV at entry 2) and the table of samplers at s1.
fn build(words: &[u32], buffers: bool) -> HostShader {
    let (shader, lowered) = build_any(words, buffers);
    // One access with each lane's own heap index, no wave operation.
    assert_eq!(lowered.nonuniform_handles, 1);
    assert!(!lowered.wave_ops);
    shader
}

/// [`build`] without its checks: the shader (validated by `dxil.dll` when
/// present) and what lowering reported.
fn build_any(words: &[u32], buffers: bool) -> (HostShader, lower::Lowered) {
    build_with_view(words, buffers.then_some(4))
}

fn build_with_view(words: &[u32], buffer_stride: Option<u32>) -> (HostShader, lower::Lowered) {
    build_compute(
        words,
        buffer_stride,
        false,
        super::ComputeWaves::with_wave32(true),
        ComputeIo {
            rsrc2: 2 << 1,
            threads: [32, 1, 1],
        },
    )
}

fn build_compute(
    words: &[u32],
    buffer_stride: Option<u32>,
    wave64: bool,
    waves: super::ComputeWaves,
    cs: ComputeIo,
) -> (HostShader, lower::Lowered) {
    let table = |user_data: u8| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    };
    let range = |kind, table_offset_dwords, dwords, stride, user_data| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords,
        user_data,
        slot: user_data,
    };
    let views = if let Some(stride) = buffer_stride {
        range(ResourceType::Srv, 0, 4, stride, 0)
    } else {
        range(ResourceType::Srv, 0, 8, 0, 0)
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 32,
        layout: vec![table(0), table(1)],
        used: vec![
            views,
            range(ResourceType::Uav, 16, 4, 0, 0),
            range(ResourceType::Sampler, 0, 4, 0, 1),
        ],
        io: StageIo::Cs(cs),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower_with_waves(
        &lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        },
        waves,
        None,
        None,
    )
    .expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil.clone(),
        user_data: lowered.user_data.clone(),
        samplers: lowered.samplers.clone(),
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs.clone(),
        root_srvs: lowered.root_srvs.clone(),
        root_uavs: lowered.root_uavs.clone(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    (shader, lowered)
}

/// Runs `shader` with `views` at heap indices 0.. (the UAV last) on the
/// hardware GPU and on WARP (each skipped without a device) and returns,
/// per one that ran, the UAV's 32 dwords. Empty when no SM 6.6 device is
/// available: callers return early (`docs/code-standards.md`).
fn run(shader: &HostShader, views: &[View]) -> Vec<(&'static str, Vec<u32>)> {
    let samplers = [D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        MaxLOD: f32::MAX,
        ..Default::default()
    }];
    let mut out = Vec::new();
    // One GPU test at a time, with the VALU tests: in parallel on the
    // hardware adapter and WARP the test process sometimes died of an
    // access violation (7 of 24 runs of these four tests).
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
    for &(label, warp) in super::valu_tests::backends(force_warp) {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = match host.compute_pipeline(shader) {
            Ok(p) => p,
            Err(e) => panic!("{label}: compute_pipeline failed: {e}"),
        };
        // Both tables start at heap index 0.
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            assert_eq!(u.root, RootType::Table, "{u:?}");
            ud[u.index as usize] = 0;
        }
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views,
                    samplers: &samplers,
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words[0].clone()));
    }
    out
}

/// False when no SM 6.6 hardware GPU or WARP ran; the caller returns early.
fn device_ran<T>(results: &[T]) -> bool {
    if results.is_empty() {
        eprintln!("neither hardware nor WARP SM 6.6 device available; skipped");
        false
    } else {
        true
    }
}

/// The red channel each lane sampled from two 1x1 textures, red 1.0 at
/// heap index 0 and 2.0 at 1.
fn sampled(shader: &HostShader) -> Vec<(&'static str, Vec<f32>)> {
    let first = [[1.0f32, 0.0, 0.0, 1.0]];
    let second = [[2.0f32, 0.0, 0.0, 1.0]];
    let views = [
        View::Texture(1, 1, &first),
        View::Texture(1, 1, &second),
        View::RawUav(32),
    ];
    run(shader, &views)
        .into_iter()
        .map(|(label, words)| (label, words.into_iter().map(f32::from_bits).collect()))
        .collect()
}

#[test]
fn lanes_sample_the_texture_their_if_else_arm_chose() {
    use crate::resource::tests::WATERFALL_JOINED_WAVE32;
    let shader = build(WATERFALL_JOINED_WAVE32, false);
    let results = sampled(&shader);
    if !device_ran(&results) {
        return;
    }
    for (label, red) in results {
        // Lanes below 16 took the first texture, the others the second.
        let want: Vec<f32> = (0..32).map(|l| if l < 16 { 1.0 } else { 2.0 }).collect();
        assert_eq!(red, want, "{label}");
    }
}

#[test]
fn lanes_sample_the_texture_their_arm_loaded_or_picked() {
    use crate::resource::tests::WATERFALL_JOINED_MIXED_WAVE32;
    let shader = build(WATERFALL_JOINED_MIXED_WAVE32, false);
    let results = sampled(&shader);
    if !device_ran(&results) {
        return;
    }
    for (label, red) in results {
        // Lanes below 16 loaded entry (lane & 1); of the others, lanes
        // below 24 picked the first, the rest the second.
        let want: Vec<f32> = (0..32)
            .map(|l| match l {
                0..16 => [1.0, 2.0][l & 1],
                16..24 => 1.0,
                _ => 2.0,
            })
            .collect();
        assert_eq!(red, want, "{label}");
    }
}

#[test]
fn lanes_load_from_the_buffer_their_nested_arms_chose() {
    use crate::resource::tests::WATERFALL_JOINED_BUFFER_WAVE32;
    let shader = build(WATERFALL_JOINED_BUFFER_WAVE32, true);
    let first: Vec<u32> = (0..32).map(|i| 100 + i).collect();
    let second: Vec<u32> = (0..32).map(|i| 200 + i).collect();
    let views = [
        View::Buffer {
            data: &first,
            stride: Stride::Structured(4),
        },
        View::Buffer {
            data: &second,
            stride: Stride::Structured(4),
        },
        View::RawUav(32),
    ];
    let results = run(&shader, &views);
    if !device_ran(&results) {
        return;
    }
    for (label, got) in results {
        // Dword `lane` of the first buffer below 16, of the second from 16
        // to 23; lanes from 24 on load and store nothing.
        let want: Vec<u32> = (0..32)
            .map(|l| match l {
                0..16 => 100 + l,
                16..24 => 200 + l,
                _ => 0,
            })
            .collect();
        assert_eq!(got, want, "{label}");
    }
}

#[test]
fn lanes_sample_the_entry_their_offset_names_in_the_console_waterfall() {
    use crate::resource::tests::WATERFALL_INDEXED_WAVE32;
    let shader = build(WATERFALL_INDEXED_WAVE32, false);
    let results = sampled(&shader);
    if !device_ran(&results) {
        return;
    }
    for (label, red) in results {
        // Even lanes hold entry 0's offset, odd lanes entry 1's.
        let want: Vec<f32> = (0..32).map(|l| [1.0, 2.0][l & 1]).collect();
        assert_eq!(red, want, "{label}");
    }
}

#[test]
fn lanes_sample_the_entry_their_index_names_when_scaled_in_the_loop() {
    use crate::resource::tests::{
        WATERFALL_ADDED_BASE_INDEX_WAVE32, WATERFALL_SCALED_INDEX_WAVE32,
    };
    let want: Vec<f32> = (0..32).map(|l| [1.0, 2.0][l & 1]).collect();
    for words in [
        WATERFALL_SCALED_INDEX_WAVE32,
        WATERFALL_ADDED_BASE_INDEX_WAVE32,
    ] {
        let shader = build(words, false);
        let results = sampled(&shader);
        if !device_ran(&results) {
            return;
        }
        for (label, red) in results {
            assert_eq!(red, want, "{label}");
        }
    }
}

#[test]
fn lanes_sample_their_own_texture_in_a_waterfall_narrowed_by_v_cmpx() {
    use crate::resource::tests::cmpx_waterfall;
    // Lanes below 16 hold the first descriptor, the others the second.
    let want: Vec<f32> = (0..32).map(|l| if l < 16 { 1.0 } else { 2.0 }).collect();
    for folded in [false, true] {
        let shader = build(&cmpx_waterfall(false, folded), false);
        let results = sampled(&shader);
        if !device_ran(&results) {
            return;
        }
        for (label, red) in results {
            assert_eq!(red, want, "{label}, folded {folded}");
        }
    }
}

#[test]
fn lanes_sample_their_arm_after_a_redundant_masked_write() {
    use crate::resource::tests::WATERFALL_JOINED_WAVE32;
    let mut words = WATERFALL_JOINED_WAVE32.to_vec();
    let at = words.iter().position(|&w| w == 0x887E_147E).unwrap() + 1;
    words.splice(
        at..at,
        [
            0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0 (lane < 16)
            0xBE92_3C6A, // s_and_saveexec_b32 s18, vcc_lo
            0x7E08_0218, // v_mov_b32 v4, s24 (same word the then arm wrote)
            0xBEFE_0312, // s_mov_b32 exec_lo, s18
        ],
    );
    let shader = build(&words, false);
    let results = sampled(&shader);
    if !device_ran(&results) {
        return;
    }
    let want: Vec<f32> = (0..32).map(|l| if l < 16 { 1.0 } else { 2.0 }).collect();
    for (label, red) in results {
        assert_eq!(red, want, "{label}");
    }
}

/// `image_gather4_l` at (0.375, 0.375) of a 4x4 texture whose red is
/// 10 * y + x, level 2.0 (the view has one level: clamped to level 0). Lanes
/// 0-7 store the four results at dwords 4 * lane...
const GATHER4_L_WAVE32: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xBE86_0301, // s_mov_b32 s6, s1
    0xBE87_0380, // s_mov_b32 s7, 0
    0xF40C_0302,
    0xFA00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
    0xF408_0203,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
    0xF408_0A02,
    0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
    0x3620_0087, // v_and_b32 v16, 7, v0
    0x3420_2084, // v_lshlrev_b32 v16, 4, v16
    0x7E04_02FF,
    0x3EC0_0000, // v_mov_b32 v2, 0.375
    0x7E06_02FF,
    0x3EC0_0000, // v_mov_b32 v3, 0.375
    0x7E08_02FF,
    0x4000_0000, // v_mov_b32 v4, 2.0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xF110_0108,
    0x0043_0C02, // image_gather4_l v[12:15], v[2:4], s[12:19], s[8:11] dmask:0x1 dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xE070_1000,
    0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
    0xE070_1004,
    0x800A_0D10, // buffer_store_dword v13, v16, s[40:43], 0 offen offset:4
    0xE070_1008,
    0x800A_0E10, // buffer_store_dword v14, v16, s[40:43], 0 offen offset:8
    0xE070_100C,
    0x800A_0F10, // buffer_store_dword v15, v16, s[40:43], 0 offen offset:12
    0xBF81_0000, // s_endpgm
];

#[test]
fn gather4_with_a_level_returns_the_four_texels_of_that_level_in_gather_order() {
    let (shader, _) = build_any(GATHER4_L_WAVE32, false);
    let texels: Vec<[f32; 4]> = (0..16)
        .map(|i| [(10 * (i / 4) + i % 4) as f32, 0.0, 0.0, 1.0])
        .collect();
    let views = [
        View::Texture(4, 4, &texels),
        View::Texture(4, 4, &texels),
        View::RawUav(32),
    ];
    let results = run(&shader, &views);
    if !device_ran(&results) {
        return;
    }
    for (label, words) in results {
        let got: Vec<f32> = words.into_iter().map(f32::from_bits).collect();
        // u * 4 - 0.5 = 1: texels (1, 2), (2, 2), (2, 1), (1, 1).
        let want: Vec<f32> = (0..32).map(|i| [21.0, 22.0, 12.0, 11.0][i % 4]).collect();
        assert_eq!(got, want, "{label}");
    }
}

#[test]
fn gather4_with_a_level_far_outside_the_view_reads_the_nearest_edge() {
    let texels: Vec<[f32; 4]> = (0..16)
        .map(|i| [(10 * (i / 4) + i % 4) as f32, 0.0, 0.0, 1.0])
        .collect();
    // (u, v) and the texel (10 * y + x) all four of the footprint clamp to.
    // u * 4 above i32 (or infinite) must give the last column, not wrap
    // to the first when the footprint's second column is added; far below,
    // the first; NaN, the first.
    let cases = [
        (1e9, -1e9, 3.0),
        (-1e9, 1e9, 30.0),
        (f32::INFINITY, f32::NEG_INFINITY, 3.0),
        (f32::NAN, f32::INFINITY, 30.0),
    ];
    for (u, v, texel) in cases {
        let mut words = GATHER4_L_WAVE32.to_vec();
        // The literals of `v_mov_b32 v2, u` and `v_mov_b32 v3, v`.
        assert_eq!((words[12], words[14]), (0x7E04_02FF, 0x7E06_02FF));
        words[13] = f32::to_bits(u);
        words[15] = f32::to_bits(v);
        let (shader, _) = build_any(&words, false);
        let views = [
            View::Texture(4, 4, &texels),
            View::Texture(4, 4, &texels),
            View::RawUav(32),
        ];
        let results = run(&shader, &views);
        if !device_ran(&results) {
            return;
        }
        for (label, words) in results {
            let got: Vec<f32> = words.into_iter().map(f32::from_bits).collect();
            assert_eq!(got, vec![texel; 32], "{label}: ({u}, {v})");
        }
    }
}

#[test]
fn explicit_cube_gradients_select_face_and_mip() {
    // Six faces, split into four groups of lanes with different derivatives.
    // At width 8, ds/dx of 1/4 chooses mip 1; dt/dy of 1/2 or 1 chooses mip 2 or 3.
    let setup = [
        0xbe84_0300, // s_mov_b32 s4, s0
        0xbe85_0380, // s_mov_b32 s5, 0
        0xbe86_0301, // s_mov_b32 s6, s1
        0xbe87_0380, // s_mov_b32 s7, 0
        0xf40c_0302,
        0xfa00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0xf408_0a02,
        0xfa00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0xf408_0203,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
        0x3604_0087, // v_and_b32_e32 v2, 7, v0
        0x2604_0485, // v_min_u32_e32 v2, 5, v2
        0x7e14_0d02, // v_cvt_f32_u32_e32 v10, v2
        0x2c06_0083, // v_lshrrev_b32_e32 v3, 3, v0
        0x3608_0681, // v_and_b32_e32 v4, 1, v3
        0x7e08_0d04, // v_cvt_f32_u32_e32 v4, v4
        0x1008_08ff,
        0x3e80_0000, // v_mul_f32_e32 v4, 0x3e800000, v4
        0x7e0a_0280, // v_mov_b32_e32 v5, 0
        0x7e0c_0280, // v_mov_b32_e32 v6, 0
        0x2c0e_0681, // v_lshrrev_b32_e32 v7, 1, v3
        0x340e_0f03, // v_lshlrev_b32_e32 v7, v3, v7 (0, 0, 4, 8)
        0x7e0e_0d07, // v_cvt_f32_u32_e32 v7, v7
        0x100e_0eff,
        0x3e00_0000, // v_mul_f32_e32 v7, 0.125, v7
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
    ];
    let ordinary = [
        0x7e10_02ff,
        0x3fc0_0000, // v8 = 1.5
        0x7e12_02ff,
        0x3fc0_0000, // v9 = 1.5
        0xf088_0118,
        0x0043_0c04, // image_sample_d v12, v[4:10], s[12:19], s[8:11], cube
    ];
    let packed = [
        0x7e10_02ff,
        0x3e00_3e00, // s and t packed into v8
        0x7e12_150a, // v_cvt_f16_f32 v9, v10 (face)
        0x3812_12f4, // v_or_b32 v9, 0x40000000, v9 (minimum LOD 2)
        0xf08c_0118,
        0x4043_0c04, // image_sample_d_cl v12, v[4:9], s[12:19], s[8:11], cube a16
    ];
    // The low and high halves hold ds and dt, respectively, for each axis.
    let half_gradients = [
        0x7e08_1504, // write ds/dx to low half of v4
        0x3608_08ff,
        0x0000_ffff, // clear preserved high half (dt/dx)
        0x7e0a_1507, // v5 = f16(dt/dy)
        0x340a_0a90, // v5 <<= 16; ds/dy = 0
    ];
    let g16 = [
        0x7e0c_02ff,
        0x3fc0_0000, // v6 = s
        0x7e0e_02ff,
        0x3fc0_0000, // v7 = t
        0x7e10_030a, // v8 = face
        0xf088_0119,
        0x0043_0c04, // image_sample_d_g16, cube
    ];
    let g16_a16 = [
        0x7e0c_02ff,
        0x3e00_3e00, // v6 = packed s and t
        0x7e0e_0280, // clear both halves before packing face and minimum LOD
        0x7e0e_150a, // v7 = f16(face)
        0x380e_0ef4, // high half of v7 = minimum LOD 2
        0xf08c_0119,
        0x4043_0c04, // image_sample_d_cl_g16, cube a16
    ];
    let store = [0xe070_1000, 0x800a_0c10, 0xbf81_0000]; // store v12 at v16, then end
    let mut texels = Vec::new();
    for face in 0..6 {
        for mip in 0..4 {
            let size = 8 >> mip;
            texels.extend(std::iter::repeat_n(
                [(face * 10 + mip) as f32; 4],
                size * size,
            ));
        }
    }
    let views = [
        View::Cube(8, 4, &texels),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::RawUav(32),
    ];
    for (gradients, sample, min_lod) in [
        (&[][..], &ordinary[..], 0),
        (&[][..], &packed[..], 2),
        (&half_gradients[..], &g16[..], 0),
        (&half_gradients[..], &g16_a16[..], 2),
    ] {
        let words = [&setup[..], gradients, sample, &store[..]].concat();
        let (shader, _) = build_any(&words, false);
        let expected: Vec<_> = (0..32)
            .map(|lane| {
                let face = (lane % 8).min(5);
                let mip = [0, 1, 2, 3][lane / 8].max(min_lod);
                ((face * 10 + mip) as f32).to_bits()
            })
            .collect();
        let results = run(&shader, &views);
        if !device_ran(&results) {
            return;
        }
        for (backend, readback) in results {
            assert_eq!(readback, expected, "{backend}, minimum LOD {min_lod}");
            eprintln!("{backend}: all 32 face and mip values matched at minimum LOD {min_lod}");
        }
    }
}

#[test]
fn scalar_structured_load_and_record_count_use_the_same_view() {
    #[rustfmt::skip]
    let words = [
        0xbe840300, 0xbe850380,     // s[4:5] = view table
        0xf4080202, 0xfa000000,     // s_load_dwordx4 s[8:11], s[4:5], 0
        0xf4080302, 0xfa000040,     // s_load_dwordx4 s[12:15], s[4:5], 64
        0xf4200404, 0xfa000014,     // s_buffer_load_dword s16, s[8:11], 20
        0x80110a10,                 // s_add_u32 s17, s16, s10 (record count)
        0x7e020211,                 // v_mov_b32 v1, s17
        0x34040082,                 // v_lshlrev_b32 v2, 2, v0 (lane * 4)
        0xe0701000, 0x80030102,     // buffer_store_dword v1, v2, s[12:15], 0 offen
        0xbf810000,                 // s_endpgm
    ];
    // The second program uses descriptor word 2 only as the byte offset
    // of the scalar buffer load that also consumes the whole descriptor.
    let mut offset_words = words;
    offset_words[7] = 0x14000000; // soffset = s10, immediate offset = 0
    offset_words[8] = 0xbe910310; // s_mov_b32 s17, s16
    let (sum, _) = build_with_view(&words, Some(16));
    let (offset, _) = build_with_view(&offset_words, Some(16));
    for records in [4, 8] {
        let mut data = vec![0u32; records * 4];
        data[1] = 0x1234;
        data[2] = 0x5678;
        data[5] = 0x9012;
        let views = [
            View::Buffer {
                data: &data,
                stride: Stride::Structured(16),
            },
            View::Texture(1, 1, &[[0.0; 4]]),
            View::RawUav(32),
        ];
        for (shader, expected) in [
            (&sum, data[5] + records as u32),
            (&offset, data[records / 4]),
        ] {
            let results = run(shader, &views);
            if !device_ran(&results) {
                return;
            }
            for (label, result) in results {
                assert_eq!(result, vec![expected; 32], "{label}");
            }
        }
    }
}

#[test]
fn wave64_implicit_sample_preserves_guest_coordinates_and_mips() {
    // Each 8-wide row doubles the horizontal derivative up to mip 3.
    // Both halves of the 8x8 guest group must fill their own output slots.
    let words = [
        0xbe84_0300, // s_mov_b32 s4, s0
        0xbe85_0380, // s_mov_b32 s5, 0
        0xbe86_0301, // s_mov_b32 s6, s1
        0xbe87_0380, // s_mov_b32 s7, 0
        0xf40c_0302,
        0xfa00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0xf408_0a02,
        0xfa00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0xf408_0203,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
        0x3420_0283, // v_lshlrev_b32_e32 v16, 3, v1
        0x4a20_2100, // v_add_nc_u32_e32 v16, v0, v16
        0x3420_2082, // v_lshlrev_b32_e32 v16, 2, v16
        0x3604_0081, // v_and_b32_e32 v2, 1, v0
        0x3606_0283, // v_and_b32_e32 v3, 3, v1
        0x3404_0503, // v_lshlrev_b32_e32 v2, v3, v2
        0x7e04_0d02, // v_cvt_f32_u32_e32 v2, v2
        0x100c_04ff,
        0x3e00_0000, // v_mul_f32_e32 v6, 0x3e000000, v2
        0x7e0e_0280, // v_mov_b32_e32 v7, 0
        0xf080_0108,
        0x0043_0c06, // image_sample v12, v[6:7], s[12:19], s[8:11] dmask:0x1 dim:SQ_RSRC_IMG_2D
        0xe070_1000,
        0x800a_0c10, // buffer_store_dword v12, v16, s[40:43], 0 offen
        0xbf81_0000, // s_endpgm
    ];
    let io = ComputeIo {
        rsrc2: (2 << 1) | (1 << 11),
        threads: [8, 8, 1],
    };
    let narrow = super::ComputeWaves {
        wave32: true,
        narrow_quads: true,
    };
    let native = super::ComputeWaves::with_wave32(true);
    let check_layout = |shader: &HostShader, threads: [u32; 3], wave: Option<u32>| {
        let c = crate::Container::parse(&shader.dxil).unwrap();
        let psv = c.part(crate::FourCC::PSV0).unwrap().data;
        let word = |offset| u32::from_le_bytes(psv[offset..offset + 4].try_into().unwrap());
        assert_eq!([word(40), word(44), word(48)], threads);
        assert_eq!([word(20), word(24)], wave.map_or([0, u32::MAX], |w| [w; 2]));
    };
    let (original, lowered) = build_compute(&words, None, true, native, io);
    assert!(!lowered.has_quad_read);
    check_layout(&original, io.threads, Some(64));

    // An actual barrier requires the original group. In particular it cannot
    // be hidden by the absence of any declared group-shared memory.
    let mut barrier = words.to_vec();
    barrier.insert(words.len() - 1, 0xbf8a_0000); // s_barrier
    let (guarded, _) = build_compute(&barrier, None, true, narrow, io);
    check_layout(&guarded, io.threads, Some(64));

    let mut texels = Vec::new();
    for mip in 0..4 {
        let size = 8 >> mip;
        texels.extend(std::iter::repeat_n([(mip + 1) as f32; 4], size * size));
    }
    let views = [
        View::TextureMips(8, 8, 4, &texels),
        View::Texture(1, 1, &[[0.0; 4]]),
        View::RawUav(64),
    ];
    let expected: Vec<_> = (0..64)
        .map(|i| ((i / 8 % 4 + 1) as f32).to_bits())
        .collect();
    // Select each mip once via lane 1 minus lane 0 and once via lane 2 minus
    // lane 0. Both also test that row-major addresses cover the upper half.
    let mut vertical = words.to_vec();
    vertical.splice(
        13..14,
        [
            0x2c04_0081, // v_lshrrev_b32_e32 v2, 1, v0
            0x3604_0481, // v_and_b32_e32 v2, 1, v2
        ],
    );
    for program in [&words[..], &vertical] {
        let (shader, lowered) = build_compute(program, None, true, narrow, io);
        assert!(lowered.has_quad_read);
        check_layout(&shader, [64, 1, 1], None);
        for (label, got) in run(&shader, &views) {
            assert_eq!(got, expected, "{label}");
            eprintln!("{label}: all 64 guest coordinates and four mip levels verified");
        }
    }
}
