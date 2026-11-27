//! End-to-end tests of lane masks read as data where they hold lanes that
//! do not run the block writing them: synthetic wave32 GFX10 compute
//! programs lowered to DXIL, validated by `dxil.dll` when present, and run
//! on the host GPU and WARP against the console's result, lane by lane.

use super::host::{Dispatch, Host, Root, View, assert_test_wave_size};
use super::{HostShader, lower, validator};
use crate::dxbc::Container;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const LANES: usize = 32;
const VCC_LO: u32 = 106;
const EXEC_LO: u32 = 126;

const fn sop1(op: u32, sdst: u32, src0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | src0
}
const fn sop2(op: u32, sdst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0
}
const fn s_mov_b32(sdst: u32, src0: u32) -> u32 {
    sop1(0x03, sdst, src0)
}
const fn s_not_b32(sdst: u32, src0: u32) -> u32 {
    sop1(0x07, sdst, src0)
}
const fn s_bcnt1_i32_b32(sdst: u32, src0: u32) -> u32 {
    sop1(0x0F, sdst, src0)
}
const fn s_and_saveexec_b32(sdst: u32, src0: u32) -> u32 {
    sop1(0x3C, sdst, src0)
}
const fn s_or_saveexec_b32(sdst: u32, src0: u32) -> u32 {
    sop1(0x3D, sdst, src0)
}
const fn s_xor_b32(sdst: u32, src0: u32, src1: u32) -> u32 {
    sop2(0x12, sdst, src0, src1)
}
const fn s_or_b32(sdst: u32, src0: u32, src1: u32) -> u32 {
    sop2(0x10, sdst, src0, src1)
}
const fn s_andn2_b32(sdst: u32, src0: u32, src1: u32) -> u32 {
    sop2(0x14, sdst, src0, src1)
}
const fn s_cbranch_execz(words: u16) -> u32 {
    0xBF88_0000 | words as u32
}
/// `v_cmp_gt_u32 vcc_lo, n, v0`: the lanes below `n` (v0 is the lane).
const fn v_cmp_lanes_below(n: u32) -> u32 {
    0x7D88_0000 | (0x80 + n)
}
/// `v_cmp_lt_u32 vcc_lo, n, v0`: the lanes above `n`.
const fn v_cmp_lanes_above(n: u32) -> u32 {
    0x7D82_0000 | (0x80 + n)
}
/// `v_mov_b32 vdst, src0` (`src0`: an SGPR or inline constant code).
const fn v_mov_b32(vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | 0x01 << 9 | src0
}

/// Wraps `body` (its result in v1, 0 before it) in a wave32 program of 32
/// lanes that stores v1 at dword `lane` of the raw UAV at table entry 0.
fn program(body: &[u32]) -> Vec<u32> {
    let mut w = vec![
        s_mov_b32(1, 0x80), // s1 = 0: s[0:1] is the table's address
        0xF408_0200,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[0:1], 0x0
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        v_mov_b32(1, 0x80),
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ];
    w.extend_from_slice(body);
    w.extend_from_slice(&[
        0xE070_1000,
        0x8002_0110, // buffer_store_dword v1, v16, s[8:11], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    w
}

/// `before` runs in every lane, then `inside` in the lanes below s2
/// (`s_and_saveexec_b32 s6, vcc_lo` and `s_cbranch_execz` over it); EXEC
/// is restored from s6 after it, then `after` runs in every lane.
fn branch(before: &[u32], inside: &[u32], after: &[u32]) -> Vec<u32> {
    let mut body = before.to_vec();
    body.extend_from_slice(&[
        0x7D88_0002, // v_cmp_gt_u32 vcc_lo, s2, v0
        s_and_saveexec_b32(6, VCC_LO),
        s_cbranch_execz(inside.len() as u16),
    ]);
    body.extend_from_slice(inside);
    body.push(s_or_b32(EXEC_LO, EXEC_LO, 6));
    body.extend_from_slice(after);
    program(&body)
}

/// Lowers a wave32 compute shader with the UAV table at s0 and the branch
/// limit in s2. `wide` uses the same retry as opt-in wave-wide translation.
fn lowered(words: &[u32], wide: bool) -> Result<(HostShader, Option<u32>), crate::ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 2,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [LANES as u32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    if wide {
        return super::widen(&c, &native, &p, false, false);
    }
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    super::emit_program(&c, &native, &p, &st, &io, &res)
}

fn build(words: &[u32], wide: bool) -> HostShader {
    let (shader, wave_size) = lowered(words, wide).expect("lower");
    assert_eq!(wave_size, Some(32));
    assert_test_wave_size(&shader, 32);
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

fn build_after_widening(words: &[u32]) -> HostShader {
    lower::take_needs();
    let Err(error) = lowered(words, false) else {
        panic!("a per-lane join would give some lanes the wrong SGPR value");
    };
    assert!(
        matches!(
            error,
            crate::ShaderError::Unsupported {
                what: lower::LANE_MASK,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        !lower::take_needs().is_empty(),
        "no divergent arm was named"
    );
    build(words, true)
}

/// On each backend that supports wave32, dispatches every case and compares
/// each lane's stored dword. A supported device must accept the pipeline.
fn check(shader: &HostShader, cases: &[(u32, [u32; LANES])]) {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let host = match Host::new_test_backend(warp) {
            Ok(host) => host,
            Err(why) => {
                eprintln!("{label}: {why}; skipped");
                continue;
            }
        };
        if !host.supports_test_wave(32, label) {
            continue;
        }
        // A device that runs 32-lane waves must take the pipeline: a refusal
        // there is the emitted DXIL's, not the device's.
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let limit = shader
            .user_data
            .iter()
            .find(|u| (u.root, u.slot, u.dword) == (RootType::Constants, 1, 0))
            .expect("s2 branch limit")
            .index as usize;
        let views = [View::RawUav(LANES as u32)];
        for (below, want) in cases {
            let mut ud = vec![0u32; shader.user_data_dwords() as usize];
            ud[limit] = *below;
            let words = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            assert_eq!(words[0], want, "{label}, lanes below {below}");
        }
        ran += 1;
    }
    if ran == 0 {
        eprintln!("no SM 6.6 device with a 32-lane wave; not run");
    }
}

/// The console's result: `inside` for the lanes below 10, `outside` for
/// the others.
fn below_ten(inside: u32, outside: u32) -> [u32; LANES] {
    std::array::from_fn(|l| if l < 10 { inside } else { outside })
}

/// `s_not_b32` of a compare in a branch sets the bits of the lanes outside
/// the branch: 27 lanes are counted, 22 of which do not run there.
#[test]
fn a_negated_compare_counts_the_lanes_outside_its_branch() {
    let shader = build(
        &branch(
            &[],
            &[
                v_cmp_lanes_below(5), // lanes 0-4 of the 10 in the branch
                s_not_b32(20, VCC_LO),
                s_bcnt1_i32_b32(4, 20),
                v_mov_b32(1, 4),
            ],
            &[],
        ),
        false,
    );
    check(&shader, &[(10, below_ten(27, 0))]);
}

/// The lanes that skipped a branch, as the saved EXEC without the current
/// one (`s_andn2_b32 s20, s6, exec_lo`), read as a number in the branch.
#[test]
fn the_lanes_that_skipped_a_branch_read_as_a_number_inside_it() {
    let shader = build(
        &branch(&[], &[s_andn2_b32(20, 6, EXEC_LO), v_mov_b32(1, 20)], &[]),
        false,
    );
    check(
        &shader,
        &[
            (0, [0; LANES]),
            (10, below_ten(!0x3FF, 0)),
            (32, [0; LANES]),
        ],
    );
}

/// A compare written in a branch and read as a number after EXEC is
/// restored, through the join of its value and the 0 the other path keeps.
#[test]
fn a_compare_from_a_branch_reads_as_a_number_after_the_join() {
    let shader = build_after_widening(&branch(
        &[s_mov_b32(20, 0x80)],
        &[v_cmp_lanes_below(5), s_mov_b32(20, VCC_LO)],
        &[v_mov_b32(1, 20)],
    ));
    check(&shader, &[(0, [0; LANES]), (10, [0x1F; LANES])]);
}

/// The lanes that skip a branch keep the -1 written before it only where no
/// lane enters; once one does, the wave's s20 is the branch's compare,
/// clear in them. Taking each lane's own path would give them -1.
#[test]
fn a_branch_mask_overwrites_the_old_number_for_every_lane() {
    let shader = build_after_widening(&branch(
        &[s_mov_b32(20, 0xC1)],
        &[v_cmp_lanes_below(5), s_mov_b32(20, VCC_LO)],
        &[v_mov_b32(1, 20)],
    ));
    check(&shader, &[(0, [!0; LANES]), (10, [0x1F; LANES])]);
}

/// Both arms of an if/else write s20: the wave leaves through the else
/// arm, so on the console the lanes of the then arm read its compare
/// (clear in them), not their own.
#[test]
fn the_last_arm_to_write_a_mask_broadcasts_it_to_every_lane() {
    let then_arm = [v_cmp_lanes_below(5), s_mov_b32(20, VCC_LO)];
    let else_arm = [v_cmp_lanes_above(19), s_mov_b32(20, VCC_LO)];
    let mut body = vec![
        s_mov_b32(20, 0x80),
        0x7D88_0002, // v_cmp_gt_u32 vcc_lo, s2, v0
        s_and_saveexec_b32(6, VCC_LO),
        s_xor_b32(6, EXEC_LO, 6),
        s_cbranch_execz(then_arm.len() as u16),
    ];
    body.extend_from_slice(&then_arm);
    body.extend_from_slice(&[
        s_or_saveexec_b32(6, 6),
        s_xor_b32(EXEC_LO, EXEC_LO, 6),
        s_cbranch_execz(else_arm.len() as u16),
    ]);
    body.extend_from_slice(&else_arm);
    body.extend_from_slice(&[s_or_b32(EXEC_LO, EXEC_LO, 6), v_mov_b32(1, 20)]);
    let shader = build_after_widening(&program(&body));
    check(
        &shader,
        &[
            (0, [0xFFF0_0000; LANES]),
            (10, [0xFFF0_0000; LANES]),
            (32, [0x1F; LANES]),
        ],
    );
}
