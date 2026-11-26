//! Local-data-share reads, writes and inter-thread exchanges, including execution
//! on available D3D12 adapters and WARP.

use super::{HostShader, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// `v_mov_b32 v[dst], literal` (VOP1 with 32-bit inline literal).
fn v_mov_lit(dst: u32, lit: u32) -> [u32; 2] {
    [0x7E00_02FF | (dst << 17), lit]
}

/// DS instruction with a single 16-bit byte offset.
fn ds(op: u32, off: u32, addr: u32, data0: u32, dst: u32) -> [u32; 2] {
    [
        0xD800_0000 | (op << 18) | (off & 0xFFFF),
        (dst << 24) | (data0 << 8) | addr,
    ]
}

/// `buffer_store_dword v[data], off, s[8:11], 0 offset:byte_off`.
fn store_uav(vdata: u32, dword_index: u32) -> [u32; 2] {
    let byte_off = dword_index * 4;
    [
        0xE070_0000 | (byte_off & 0x0FFF),
        0x8002_0000 | (vdata << 8),
    ]
}

/// Builds a 64-thread compute shader with 256 dwords of LDS (16 private bytes
/// per thread, addressed from `v0 = thread_id * 16`) and a root UAV descriptor
/// in `s[8:11]` (slot 3), runs `body`, and checks readback against `expected`
/// on every available SM 6.6 backend (hardware GPU and WARP). Using >32 threads
/// avoids emitting `WaveSize(32)`, so WARP (4 lanes) executes it too.
fn run_lds_program(body: &[u32], expected: &[u32]) {
    let (shader, _) = build_lds_program::<false>(
        body,
        false,
        [64, 1, 1],
        super::ComputeWaves::with_wave32(true),
        2,
    )
    .unwrap();
    #[cfg(windows)]
    check_readback(shader, expected);
    #[cfg(not(windows))]
    let _ = (shader, expected);
}

fn build_lds_program<const PARTIAL: bool>(
    body: &[u32],
    guest_wave64: bool,
    threads: [u32; 3],
    waves: super::ComputeWaves,
    lds_blocks: u32,
) -> Result<(HostShader, usize), crate::ShaderError> {
    let slot = 3;
    let mut words = vec![
        0xF404_0200, // s_load_dwordx2 s[8:9], s[0:1], 0
        0xFA00_0000,
        0xBE8A_0384, // descriptor size
        0xBE8B_0380, // remaining descriptor flags
    ];
    if !guest_wave64 {
        words.push(0x3400_0084); // private 16-byte slice for the sub-dword tests
    }
    words.extend_from_slice(body);
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: guest_wave64,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            sgprs: 2,
            base_register: 0,
            user_data: 16,
            slot,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 16,
            slot,
        }],
        io: StageIo::Cs(ComputeIo {
            // Four user SGPRs plus LDS in 128-dword blocks.
            rsrc2: (4 << 1) | (lds_blocks << 15) | (u32::from(threads[1] > 1) << 11),
            threads,
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let p =
        ir::build_with(&native.code, native.wave64, native.vgprs, true).expect("ir::build_with");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower_with_waves::<PARTIAL>(
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
    )?;

    Ok((
        HostShader {
            stage: HwStage::Cs,
            dxil: lowered.dxil,
            user_data: lowered.user_data,
            samplers: lowered.samplers,
            static_samplers: Vec::new(),
            root_cbvs: lowered.root_cbvs,
            root_srvs: lowered.root_srvs,
            root_uavs: lowered.root_uavs,
        },
        lowered.barriers,
    ))
}

#[cfg(windows)]
fn check_readback(mut shader: HostShader, expected: &[u32]) {
    use super::host::{Dispatch, Host, Root, View};
    use super::valu_tests::{ONE_AT_A_TIME, backends};

    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut shader.dxil)
            .expect("DXIL validation failed");
    }
    let mut ran = 0;
    let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
    let n = expected.len() as u32;
    for &(label, warp) in backends(force_warp) {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(vec![0])),
                        (
                            1,
                            Root::Table {
                                sampler: false,
                                index: 1,
                            },
                        ),
                    ],
                    views: &[View::RawUav(n), View::RawUav(n)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        assert_eq!(out[1], expected, "{label}");
        eprintln!("{label}: all {n} LDS values matched");
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

#[test]
fn lds_byte_reads_and_writes_on_hardware() {
    let mut body = Vec::new();
    // Seed lds[0] = 0x1122_3344.
    body.extend_from_slice(&v_mov_lit(1, 0x1122_3344));
    body.extend_from_slice(&ds(0x00d, 0, 0, 1, 0)); // ds_write_b32

    // Overwrite byte 1 with 0x81, byte 3 with 0x7F, byte 0 with 0xFE (via _d16_hi).
    body.extend_from_slice(&v_mov_lit(1, 0xDEAD_0081));
    body.extend_from_slice(&ds(0x01e, 1, 0, 1, 0)); // ds_write_b8 offset:1
    body.extend_from_slice(&v_mov_lit(1, 0xBEEF_007F));
    body.extend_from_slice(&ds(0x01e, 3, 0, 1, 0)); // ds_write_b8 offset:3
    body.extend_from_slice(&v_mov_lit(1, 0x00FE_1234));
    body.extend_from_slice(&ds(0x0a0, 0, 0, 1, 0)); // ds_write_b8_d16_hi offset:0

    // Read full dword (verifies untouched byte 2 == 0x22), then u8 / i8 reads.
    body.extend_from_slice(&ds(0x036, 0, 0, 0, 2)); // ds_read_b32 -> 0x7F22_81FE
    body.extend_from_slice(&store_uav(2, 0));
    body.extend_from_slice(&ds(0x03a, 0, 0, 0, 2)); // ds_read_u8 offset:0 -> 0x0000_00FE
    body.extend_from_slice(&store_uav(2, 1));
    body.extend_from_slice(&ds(0x039, 0, 0, 0, 2)); // ds_read_i8 offset:0 -> 0xFFFF_FFFE
    body.extend_from_slice(&store_uav(2, 2));
    body.extend_from_slice(&ds(0x039, 1, 0, 0, 2)); // ds_read_i8 offset:1 -> 0xFFFF_FF81
    body.extend_from_slice(&store_uav(2, 3));
    body.extend_from_slice(&ds(0x039, 3, 0, 0, 2)); // ds_read_i8 offset:3 -> 0x0000_007F
    body.extend_from_slice(&store_uav(2, 4));

    run_lds_program(
        &body,
        &[
            0x7F22_81FE,
            0x0000_00FE,
            0xFFFF_FFFE,
            0xFFFF_FF81,
            0x0000_007F,
        ],
    );
}

#[test]
fn lds_halfword_and_straddling_halfword_on_hardware() {
    let mut body = Vec::new();
    // Seed lds[0..3] with distinct bytes.
    body.extend_from_slice(&v_mov_lit(1, 0x4433_2211));
    body.extend_from_slice(&ds(0x00d, 0, 0, 1, 0)); // ds_write_b32 offset:0
    body.extend_from_slice(&v_mov_lit(1, 0x8877_6655));
    body.extend_from_slice(&ds(0x00d, 4, 0, 1, 0)); // ds_write_b32 offset:4
    body.extend_from_slice(&v_mov_lit(1, 0xCCBB_AA99));
    body.extend_from_slice(&ds(0x00d, 8, 0, 1, 0)); // ds_write_b32 offset:8

    // Aligned halfword write & reads at offset 2.
    body.extend_from_slice(&v_mov_lit(1, 0x0000_89AB));
    body.extend_from_slice(&ds(0x01f, 2, 0, 1, 0)); // ds_write_b16 offset:2 -> lds[0] = 0x89AB_2211
    body.extend_from_slice(&ds(0x03c, 2, 0, 0, 2)); // ds_read_u16 offset:2 -> 0x0000_89AB
    body.extend_from_slice(&store_uav(2, 0));
    body.extend_from_slice(&ds(0x03b, 2, 0, 0, 2)); // ds_read_i16 offset:2 -> 0xFFFF_89AB
    body.extend_from_slice(&store_uav(2, 1));

    // Unaligned non-straddling halfword at offset 1.
    body.extend_from_slice(&v_mov_lit(1, 0x0000_CDEF));
    body.extend_from_slice(&ds(0x01f, 1, 0, 1, 0)); // ds_write_b16 offset:1 -> lds[0] = 0x89CD_EF11
    body.extend_from_slice(&ds(0x03c, 1, 0, 0, 2)); // ds_read_u16 offset:1 -> 0x0000_CDEF
    body.extend_from_slice(&store_uav(2, 2));

    // Straddling halfword read at offset 3 (lds[0][31:24]=0x89, lds[1][7:0]=0x55 -> 0x5589).
    body.extend_from_slice(&ds(0x03c, 3, 0, 0, 2)); // ds_read_u16 offset:3 -> 0x0000_5589
    body.extend_from_slice(&store_uav(2, 3));

    // Straddling halfword write at offset 3: 0xF0E1 -> lds[0] = 0xE1CD_EF11, lds[1] = 0x8877_66F0.
    body.extend_from_slice(&v_mov_lit(1, 0x1234_F0E1));
    body.extend_from_slice(&ds(0x01f, 3, 0, 1, 0)); // ds_write_b16 offset:3
    body.extend_from_slice(&ds(0x03c, 3, 0, 0, 2)); // ds_read_u16 offset:3 -> 0x0000_F0E1
    body.extend_from_slice(&store_uav(2, 4));
    body.extend_from_slice(&ds(0x03b, 3, 0, 0, 2)); // ds_read_i16 offset:3 -> 0xFFFF_F0E1
    body.extend_from_slice(&store_uav(2, 5));

    // Straddling _d16_hi write at offset 7: high half 0x9A8B -> lds[1] = 0x8B77_66F0, lds[2] = 0xCCBB_AA9A.
    body.extend_from_slice(&v_mov_lit(1, 0x9A8B_1234));
    body.extend_from_slice(&ds(0x0a1, 7, 0, 1, 0)); // ds_write_b16_d16_hi offset:7

    // Read back all three dwords to prove neighboring bytes stayed intact.
    body.extend_from_slice(&ds(0x036, 0, 0, 0, 2)); // lds[0] = 0xE1CD_EF11
    body.extend_from_slice(&store_uav(2, 6));
    body.extend_from_slice(&ds(0x036, 4, 0, 0, 2)); // lds[1] = 0x8B77_66F0
    body.extend_from_slice(&store_uav(2, 7));
    body.extend_from_slice(&ds(0x036, 8, 0, 0, 2)); // lds[2] = 0xCCBB_AA9A
    body.extend_from_slice(&store_uav(2, 8));

    run_lds_program(
        &body,
        &[
            0x0000_89AB,
            0xFFFF_89AB,
            0x0000_CDEF,
            0x0000_5589,
            0x0000_F0E1,
            0xFFFF_F0E1,
            0xE1CD_EF11,
            0x8B77_66F0,
            0xCCBB_AA9A,
        ],
    );
}

#[test]
fn lds_d16_destination_forms_on_hardware() {
    let mut body = Vec::new();
    // Seed lds[0] = 0x827F_FE12, lds[1] = 0x0000_00A5.
    body.extend_from_slice(&v_mov_lit(1, 0x827F_FE12));
    body.extend_from_slice(&ds(0x00d, 0, 0, 1, 0)); // ds_write_b32 offset:0
    body.extend_from_slice(&v_mov_lit(1, 0x0000_00A5));
    body.extend_from_slice(&ds(0x00d, 4, 0, 1, 0)); // ds_write_b32 offset:4

    // For each _d16 / _d16_hi read, prime v2 with 0xDEAD_BEEF so we verify the
    // untouched 16-bit half of v2 is preserved.
    let cases: &[(u32, u32, u32)] = &[
        (0x0a2, 1, 0xDEAD_00FE), // ds_read_u8_d16 offset:1 (0xFE)
        (0x0a3, 1, 0x00FE_BEEF), // ds_read_u8_d16_hi offset:1 (0xFE)
        (0x0a4, 1, 0xDEAD_FFFE), // ds_read_i8_d16 offset:1 (-2 -> 0xFFFE)
        (0x0a5, 1, 0xFFFE_BEEF), // ds_read_i8_d16_hi offset:1 (-2 -> 0xFFFE)
        (0x0a6, 2, 0xDEAD_827F), // ds_read_u16_d16 offset:2 (0x827F)
        (0x0a7, 2, 0x827F_BEEF), // ds_read_u16_d16_hi offset:2 (0x827F)
        (0x0a6, 3, 0xDEAD_A582), // ds_read_u16_d16 offset:3 (straddling 0xA582)
        (0x0a7, 3, 0xA582_BEEF), // ds_read_u16_d16_hi offset:3 (straddling 0xA582)
    ];

    let mut expected = Vec::with_capacity(cases.len());
    for (i, &(op, off, exp)) in cases.iter().enumerate() {
        body.extend_from_slice(&v_mov_lit(2, 0xDEAD_BEEF));
        body.extend_from_slice(&ds(op, off, 0, 0, 2));
        body.extend_from_slice(&store_uav(2, i as u32));
        expected.push(exp);
    }

    run_lds_program(&body, &expected);
}

fn wave_range(shader: &HostShader) -> [u32; 2] {
    let container = crate::Container::parse(&shader.dxil).unwrap();
    let psv = container.part(crate::FourCC::PSV0).unwrap().data;
    let word = |i| u32::from_le_bytes(psv[i..i + 4].try_into().unwrap());
    [word(20), word(24)]
}

#[test]
fn lds_max_across_a_split_wave64() {
    let body = [
        0x34100283, 0x4a001100, // flatten 8 by 8 thread IDs
        0x34020082, // byte address of this thread's own dword
        0x7e040280, // v2 = 0
        0xd8340000, 0x00000201, // initialize each thread's dword to zero
        0xd8200000, 0x00000002, // atomic_max(LDS[0], flat thread ID)
        0xd8d80000, 0x03000002, // each thread reads the same maximum
        0xe0701000, 0x80020301, // out[flat thread ID] = maximum
    ];
    let narrow = super::ComputeWaves {
        wave32: true,
        narrow_quads: true,
    };
    let native = super::ComputeWaves::with_wave32(true);
    let build = |body: &[u32], waves, expected_barriers| {
        let (shader, barriers) =
            build_lds_program::<false>(body, true, [8, 8, 1], waves, 2).unwrap();
        assert_eq!(barriers, expected_barriers, "group syncs in lowered shader");
        shader
    };
    assert_eq!(wave_range(&build(&body, native, 0)), [64, 64]);
    let shader = build(&body, narrow, 2);
    assert_eq!(wave_range(&shader), [0, u32::MAX]);

    // A thread-count dispatch that ends inside a group cannot run the two
    // generated group barriers after the inactive threads have returned.
    assert!(matches!(
        build_lds_program::<true>(&body, true, [8, 8, 1], narrow, 2),
        Err(crate::ShaderError::Unsupported {
            what: "a partial dispatch with a group barrier",
            ..
        })
    ));

    // An extra LDS mutation after the maximum needs its own synchronization.
    let mut second_write = body.to_vec();
    second_write.splice(8..8, body[4..6].iter().copied());
    assert_eq!(wave_range(&build(&second_write, narrow, 0)), [64, 64]);
    // A per-lane seed can make colliding writes sensitive to lane order.
    let mut lane_seed = body;
    lane_seed[5] = 0x0000_0001; // ds_write_b32 v1, v0
    assert_eq!(wave_range(&build(&lane_seed, narrow, 0)), [64, 64]);

    // Only the first 24 threads contribute. All 64 must reach both barriers
    // and read the same maximum after EXEC is restored.
    let mut masked = body.to_vec();
    masked.insert(8, 0xbefe_0404); // s_mov_b64 exec, s[4:5]
    masked.splice(
        6..6,
        [
            0xd4c1_0006,
            0x0001_3100, // v_cmp_lt_u32 s[6:7], v0, 24
            0xbe84_2406, // s_and_saveexec_b64 s[4:5], s[6:7]
        ],
    );
    let partial = build(&masked, narrow, 2);
    assert_eq!(wave_range(&partial), [0, u32::MAX]);
    #[cfg(windows)]
    {
        check_readback(shader, &[63; 64]);
        check_readback(partial, &[23; 64]);
    }
}

#[test]
fn single_wave_group_reads_values_from_both_halves() {
    let words = [
        0x3410_0283,
        0x4a00_1100, // v0 = 8*y + x
        0x3402_0084, // v1 = flat * 16 output byte offset
        0xd8d4_401f,
        0x0500_0000, // v5 = v0 from lane XOR 16 within each half
        0xd760_0002,
        0x0001_0905, // s2 = v5 from lane 4
        0xd760_0003,
        0x0001_4905, // s3 = v5 from lane 36
        0x7e0c_0202,
        0x7e0e_0203, // copy both results to v6,v7
        0xd8d4_041f,
        0x0800_0005, // v8 = v5 from lane XOR 1
        0xd760_0006,
        0x0001_7f08, // s6 = v8 from lane 63
        0x7e10_0206, // v8 = s6
        0xe078_1000,
        0x8002_0501, // store v[5:8] for every thread
    ];
    let narrow = super::ComputeWaves {
        wave32: true,
        narrow_quads: true,
    };
    let (shader, barriers) =
        build_lds_program::<false>(&words, true, [8, 8, 1], narrow, 0).unwrap();
    assert_eq!(barriers, 3);
    let native = super::ComputeWaves::with_wave32(true);
    let (native_shader, native_barriers) =
        build_lds_program::<false>(&words, true, [8, 8, 1], native, 0).unwrap();
    assert_eq!(native_barriers, 0);
    assert_eq!(wave_range(&native_shader), [64, 64]);
    assert_eq!(wave_range(&shader), [0, u32::MAX]);
    // Existing shared memory and multiple console waves require different
    // addressing and synchronization. Both retain the native path.
    for (threads, blocks) in [([8, 8, 1], 1), ([16, 8, 1], 0)] {
        let (other, barriers) =
            build_lds_program::<false>(&words, true, threads, narrow, blocks).unwrap();
        assert_eq!(barriers, 0);
        assert_eq!(wave_range(&other), [64, 64]);
    }
    assert!(matches!(
        build_lds_program::<true>(&words, true, [8, 8, 1], narrow, 0),
        Err(crate::ShaderError::Unsupported {
            what: "a partial dispatch with a group barrier",
            ..
        })
    ));
    #[cfg(windows)]
    {
        let expected: Vec<_> = (0..64).flat_map(|lane| [lane ^ 16, 20, 52, 46]).collect();
        check_readback(shader, &expected);
    }
    #[cfg(not(windows))]
    let _ = shader;
}

#[test]
fn group_exchange_waits_for_reconvergence_after_a_masked_branch() {
    let prefix = [
        0xd765_0003,
        0x0001_00c1, // v3 = popcount of lower lanes in the low half
        0xd766_0003,
        0x0002_06c1, // v3 += popcount of lower lanes in the high half
        0x3402_0082, // v1 = group thread ID * 4 (output address)
        0xd8d4_401f,
        0x0500_0003, // v5 = lane ID of the neighbour at XOR 16
        0xd4c1_0004,
        0x0001_4103, // s[4:5] = v3 < 32
        0xbe86_2404, // save exec, enable only the low half
    ];
    let add = 0x4a0a_0a87; // v5 += 7
    let restore = 0xbefe_0406; // restore exec from s[6:7]
    let swap = [0xd8d4_041f, 0x0600_0005]; // v6 = neighbour's v5 at XOR 1
    let store = [0xe070_1000, 0x8002_0601]; // store v6 at v1
    let narrow = super::ComputeWaves {
        wave32: true,
        narrow_quads: true,
    };
    let joined = [&prefix[..], &[0xbf88_0001, add, restore], &swap, &store].concat();
    let (shader, barriers) =
        build_lds_program::<false>(&joined, true, [64, 1, 1], narrow, 0).unwrap();
    assert_eq!(wave_range(&shader), [0, u32::MAX]);
    assert_eq!(barriers, 2);
    // Moving the second exchange inside the branch would deadlock a group
    // barrier. Keep that shader on a full native wave.
    let masked = [&prefix[..], &[0xbf88_0003, add], &swap, &[restore], &store].concat();
    let (other, barriers) =
        build_lds_program::<false>(&masked, true, [64, 1, 1], narrow, 0).unwrap();
    assert_eq!(wave_range(&other), [64, 64]);
    assert_eq!(barriers, 0);
    // All lanes visit the block, but EXEC masks the exchange itself.
    let guarded = [&prefix[..], &[add], &swap, &[restore], &store].concat();
    let (other, barriers) =
        build_lds_program::<false>(&guarded, true, [64, 1, 1], narrow, 0).unwrap();
    assert_eq!(wave_range(&other), [64, 64]);
    assert_eq!(barriers, 0);
    #[cfg(windows)]
    {
        let expected: Vec<_> = (0..64)
            .map(|lane| (lane ^ 1 ^ 16) + if lane < 32 { 7 } else { 0 })
            .collect();
        check_readback(shader, &expected);
    }
}

#[test]
fn an_entry_first_lane_read_is_shared_across_the_full_group() {
    let before = [
        0x3402_0082, // v1 = thread ID * 4
        0x4a0a_0089, // v5 = thread ID + 9
    ];
    let first = 0x7e04_0505; // s2 = v5 from the first active lane
    let finish = [
        0xd8d4_401f,
        0x0600_0005, // v6 = v5 from lane XOR 16
        0xbf8c_c07f, // wait for the swizzle
        0x4a0c_0c02, // v6 += s2
        0xe070_1000,
        0x8002_0601, // store v6 at v1
    ];
    let waves = super::ComputeWaves {
        wave32: true,
        narrow_quads: true,
    };
    let joined = [&before[..], &[first], &finish].concat();
    let (shader, barriers) =
        build_lds_program::<false>(&joined, true, [64, 1, 1], waves, 0).unwrap();
    assert_eq!(wave_range(&shader), [0, u32::MAX]);
    assert!(barriers > 0);
    let (partial, barriers) =
        build_lds_program::<true>(&joined, true, [64, 1, 1], waves, 0).unwrap();
    assert_eq!(wave_range(&partial), [64, 64]);
    assert_eq!(barriers, 0);
    // Mask out lane zero before the read, then restore full EXEC for the swizzle.
    let narrowed = [
        0xd4c1_0004,
        0x0001_4100, // s[4:5] = thread ID < 32
        0xbe86_047e, // save EXEC in s[6:7]
        0x8afe_047e, // remove the low half from EXEC
        first,
        0xbefe_0406, // restore EXEC
    ];
    let masked = [&before[..], &narrowed, &finish].concat();
    let (other, barriers) =
        build_lds_program::<false>(&masked, true, [64, 1, 1], waves, 0).unwrap();
    assert_eq!(wave_range(&other), [64, 64]);
    assert_eq!(barriers, 0);
    // A three-iteration loop contains the read. The native wave is retained.
    let looped = [
        &before[..],
        &[0xbe83_0383, first, 0x8083_8103, 0xbf07_8003, 0xbf85_fffc],
        &finish,
    ]
    .concat();
    let (other, barriers) =
        build_lds_program::<false>(&looped, true, [64, 1, 1], waves, 0).unwrap();
    assert_eq!(wave_range(&other), [64, 64]);
    assert_eq!(barriers, 0);
    #[cfg(windows)]
    check_readback(
        shader,
        &(0..64).map(|lane| 18 + (lane ^ 16)).collect::<Vec<_>>(),
    );
}
