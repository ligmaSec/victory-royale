//! Hardware and WARP execution tests for sub-dword local-data-share (LDS)
//! reads and writes: byte, halfword, dword-straddling halfword, and 16-bit
//! destination (`_d16` / `_d16_hi`) forms.

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
    let slot = 3;
    let mut words = vec![
        0xF404_0200, // s_load_dwordx2 s[8:9], s[0:1], 0
        0xFA00_0000,
        0xBE8A_0384, // descriptor size
        0xBE8B_0380, // remaining descriptor flags
        0x3400_0084, // v_lshlrev_b32 v0, 4, v0 (per-thread 16-byte LDS slice)
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
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
            // 4 user SGPRs (bits 5:1) + 256 dwords of LDS (bits 23:15).
            rsrc2: (4 << 1) | (2 << 15),
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };

    let p =
        ir::build_with(&native.code, native.wave64, native.vgprs, true).expect("ir::build_with");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("lower");

    let mut shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        buffer_table: false,
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
    };

    #[cfg(windows)]
    check_readback(&mut shader, expected);
    #[cfg(not(windows))]
    let _ = (&mut shader, expected);
}

#[cfg(windows)]
fn check_readback(shader: &mut HostShader, expected: &[u32]) {
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
            .compute_pipeline(shader)
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
