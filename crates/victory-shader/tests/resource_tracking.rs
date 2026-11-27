//! Synthetic console memory instructions exercise descriptor and address provenance.

use std::hint::black_box;
use std::time::Instant;

use victory_shader::ir::{self, Program, Reg};
use victory_shader::resource::{Address, track};
use victory_shader::{HwStage, RootParam, RootType, ShaderError};

const TABLE: RootParam = RootParam {
    root: RootType::Table,
    table_offset_dwords: 0,
    sgprs: 1,
    base_register: 0,
    user_data: 8,
    slot: 0,
};
const ZERO_HIGH: u32 = 0xBE83_0380; // s_mov_b32 s3, 0
const COPY_BASE: u32 = 0xBE82_0308; // s_mov_b32 s2, s8
const ADD_16: u32 = 0x8002_9002; // s_add_u32 s2, s2, 16
const LOAD: u32 = 0xF408_0401; // s_load_dwordx4 s[16:19], s[2:3], imm
const END: u32 = 0xBF81_0000; // s_endpgm

fn build(words: &[u32]) -> Program {
    let code: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    ir::build(&code, false, 256).unwrap()
}

// Each unrolled step advances the same base and reads several fields.
fn unrolled_loads(steps: u32, loads_per_step: u32) -> Program {
    let mut step = vec![ADD_16];
    for field in 0..loads_per_step {
        step.extend([LOAD, 0xFA00_0000 | (4 * field)]);
    }
    let mut words = vec![ZERO_HIGH, COPY_BASE];
    for _ in 0..steps {
        words.extend_from_slice(&step);
    }
    words.push(END);
    build(&words)
}

#[test]
fn tracks_every_offset_in_a_shared_address_chain() {
    let program = unrolled_loads(32, 4);
    let tracked = track(&program, &[TABLE], HwStage::Ps, None).unwrap();
    let expected: Vec<_> = (1..=32)
        .flat_map(|step| (0..4).map(move |field| (16 * step + 4 * field, None)))
        .collect();
    assert_eq!(
        tracked
            .scalar
            .iter()
            .map(|(_, a)| (a.offset, a.dynamic))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(tracked.scalar.iter().all(|(_, a)| a.param == 0));
}

#[test]
fn shared_address_keeps_its_dynamic_term_and_wraps_offsets() {
    let program = build(&[
        ZERO_HIGH,
        0x8002_0908, // s_add_u32 s2, s8, s9
        0x8002_FF02, // s_add_u32 s2, s2, 0xfffffff0
        0xFFFF_FFF0,
        LOAD,
        0xFA00_0004,
        LOAD,
        0xFA00_0014,
        END,
    ]);
    let dynamic = program
        .inputs
        .iter()
        .find(|(r, _)| *r == Reg::S(9))
        .unwrap()
        .1;
    let tracked = track(&program, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(
        tracked.scalar,
        [
            (
                (0, 3),
                Address {
                    param: 0,
                    offset: 0xFFFF_FFF4,
                    dynamic: Some(dynamic)
                }
            ),
            (
                (0, 4),
                Address {
                    param: 0,
                    offset: 4,
                    dynamic: Some(dynamic)
                }
            ),
        ]
    );
}

#[test]
fn shared_sum_with_two_dynamic_terms_stays_opaque() {
    let program = build(&[
        ZERO_HIGH,
        0x8004_0A09, // s_add_u32 s4, s9, s10
        0x8002_0408, // s_add_u32 s2, s8, s4
        LOAD,
        0xFA00_0000,
        LOAD,
        0xFA00_0004,
        END,
    ]);
    let dynamic = program.blocks[0].insts[1].write(Reg::S(4)).unwrap();
    let tracked = track(&program, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(
        tracked.scalar,
        [
            (
                (0, 3),
                Address {
                    param: 0,
                    offset: 0,
                    dynamic: Some(dynamic)
                }
            ),
            (
                (0, 4),
                Address {
                    param: 0,
                    offset: 4,
                    dynamic: Some(dynamic)
                }
            ),
        ]
    );
    assert_eq!(
        track(&program, &[], HwStage::Ps, None).unwrap_err(),
        ShaderError::Unsupported {
            offset: 12,
            what: "an address not from a root parameter",
        }
    );
}

#[test]
fn spill_table_loads_survive_branch_targets_and_keep_the_root_slot() {
    // s2 holds a root constant. A second constant starts the spill table;
    // its descriptor table is dword 4 of the spill table (user-data index 20).
    let layout = [
        RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 2,
            slot: 0,
        },
        RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 4,
            base_register: 0,
            user_data: 16,
            slot: 2,
        },
        RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 20,
            slot: 1,
        },
    ];
    let words = [
        0xBE84_0300u32, // s_mov_b32 s4, s0
        0x8705_FF01,    // s_and_b32 s5, s1, 0xff (40-bit address)
        0xff,
        0xBF06_8002, // s_cmp_eq_u32 s2, 0
        0xBF85_0001, // s_cbranch_scc1 +1 (skip s_nop so s_load sits at a branch target)
        0xBF80_0000, // s_nop 0
        0xF400_0202,
        0xFA00_0010, // s_load_dword s8, s[4:5], 0x10 (spilled s20)
        0xBE89_0380, // s_mov_b32 s9, 0
        0xF408_0404,
        0xFA00_0000, // s_load_dwordx4 s[16:19], s[8:9], null
        END,
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = victory_shader::NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: layout.to_vec(),
        used: vec![],
        io: victory_shader::native::StageIo::Cs(victory_shader::native::ComputeIo {
            rsrc2: 3 << 1,
            threads: [1, 1, 1],
        }),
        code,
    };
    assert!(victory_shader::spill::is_spilled(&native));
    let program = victory_shader::spill::build(&native).unwrap();
    let tracked = track(&program, &layout, HwStage::Cs, None).unwrap();
    assert_eq!(tracked.scalar.len(), 1);
    assert_eq!(
        tracked.scalar[0].1,
        Address {
            param: 2,
            offset: 0,
            dynamic: None,
        }
    );
}

#[test]
fn root_constant_dword_zero_is_a_dynamic_offset_not_a_second_base() {
    let layout = [
        RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 2,
            base_register: 0,
            user_data: 0,
            slot: 0,
        },
        TABLE,
    ];
    let program = build(&[
        ZERO_HIGH,
        0x8002_0008, // s_add_u32 s2, s8, s0 (table base + root constant dword 0)
        LOAD,
        0xFA00_0000,
        END,
    ]);
    let c0 = program
        .inputs
        .iter()
        .find(|(r, _)| *r == Reg::S(0))
        .unwrap()
        .1;
    let tracked = track(&program, &layout, HwStage::Ps, None).unwrap();
    assert_eq!(
        tracked.scalar,
        [(
            (0, 2),
            Address {
                param: 1,
                offset: 0,
                dynamic: Some(c0),
            }
        )]
    );

    // Using root constant dword 0 alone as an address still fails with its exact reason.
    let bad = build(&[
        0xBE81_0380, // s_mov_b32 s1, 0
        0xF408_0400,
        0xFA00_0000, // s_load_dwordx4 s[16:19], s[0:1], null
        END,
    ]);
    assert_eq!(
        track(&bad, &layout, HwStage::Ps, None).unwrap_err(),
        ShaderError::Unsupported {
            offset: 4,
            what: "an address from a root parameter that is no address",
        }
    );
}

#[test]
fn address_arithmetic_addk_sub_const_lshl_add_and_vadd3_trace() {
    // 1. s_addk_i32 + s_sub_u32 (constant) + s_lshl4_add_u32 (constant & dynamic)
    let program = build(&[
        ZERO_HIGH,
        COPY_BASE,
        0xB782_0040, // s_addk_i32 s2, 0x40
        0x8082_9002, // s_sub_u32 s2, s2, 16  -> offset 0x30
        LOAD,
        0xFA00_0000,
        0x9882_0882, // s_lshl4_add_u32 s2, 2, s8 -> s8 + (2 << 4) = s8 + 0x20
        LOAD,
        0xFA00_0000,
        0x9882_0809, // s_lshl4_add_u32 s2, s9, s8 -> dynamic table address
        LOAD,
        0xFA00_0000,
        0xD76D_0001,
        0x0281_2008, // v_add3_u32 v1, s8, 16, 32
        0x7E04_0501, // v_readfirstlane_b32 s2, v1
        LOAD,
        0xFA00_0000,
        END,
    ]);
    let dyn_lshl = program.blocks[0].insts[7].write(Reg::S(2)).unwrap();
    let tracked = track(&program, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(
        tracked.scalar,
        [
            (
                (0, 4),
                Address {
                    param: 0,
                    offset: 0x30,
                    dynamic: None,
                }
            ),
            (
                (0, 6),
                Address {
                    param: 0,
                    offset: 0x20,
                    dynamic: None,
                }
            ),
            (
                (0, 8),
                Address {
                    param: 0,
                    offset: 0,
                    dynamic: Some(dyn_lshl),
                }
            ),
            (
                (0, 11),
                Address {
                    param: 0,
                    offset: 0x30,
                    dynamic: None,
                }
            ),
        ]
    );

    // Dynamic s_lshl4_add_u32 on a root descriptor must NOT be accepted (its
    // lowered offset cannot reuse the shifted+base value).
    let desc = RootParam {
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        sgprs: 2,
        base_register: 0,
        user_data: 8,
        slot: 0,
    };
    let bad_desc = build(&[
        0x9882_080A, // s_lshl4_add_u32 s2, s10, s8
        0x8203_8009, // s_addc_u32 s3, s9, 0
        LOAD,
        0xFA00_0000,
        END,
    ]);
    assert_eq!(
        track(&bad_desc, &[desc], HwStage::Ps, None).unwrap_err(),
        ShaderError::Unsupported {
            offset: 8,
            what: "an address not from a root parameter",
        }
    );
}

#[test]
fn same_base_address_phi_loop_and_cselect_trace() {
    // 1. If/else diamond joining (s8 + 0x20) and (s8 + 0x40) via a phi
    let diamond = build(&[
        ZERO_HIGH,
        0xBF06_8009, // s_cmp_eq_u32 s9, 0
        0xBF85_0002, // s_cbranch_scc1 +2 -> L_else
        0x8002_A008, // s_add_u32 s2, s8, 32
        0xBF82_0001, // s_branch +1 -> L_join
        0x8002_C008, // L_else: s_add_u32 s2, s8, 64
        LOAD,        // L_join: phi(s2)
        0xFA00_0000,
        END,
    ]);
    let tracked = track(&diamond, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(tracked.scalar.len(), 1);
    assert_eq!(tracked.scalar[0].1.param, 0);
    assert_eq!(tracked.scalar[0].1.offset, 32);
    assert!(tracked.scalar[0].1.dynamic.is_some());

    // 2. Loop induction pointer: p = phi(s8, p + 16)
    let loop_prog = build(&[
        ZERO_HIGH,
        COPY_BASE,
        LOAD, // loop header: phi(s2)
        0xFA00_0000,
        ADD_16,      // s_add_u32 s2, s2, 16
        0xBF06_8009, // s_cmp_eq_u32 s9, 0
        0xBF85_FFFB, // s_cbranch_scc1 -5 -> back to LOAD
        END,
    ]);
    let tracked = track(&loop_prog, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(tracked.scalar.len(), 1);
    assert_eq!(tracked.scalar[0].1.param, 0);
    assert_eq!(tracked.scalar[0].1.offset, 0);
    assert!(tracked.scalar[0].1.dynamic.is_some());

    // 3. s_cselect_b32 between (s8 + 0x20) and (s8 + 0x40)
    let select_prog = build(&[
        ZERO_HIGH,
        0x8004_A008, // s_add_u32 s4, s8, 32
        0x8005_C008, // s_add_u32 s5, s8, 64
        0xBF06_8009, // s_cmp_eq_u32 s9, 0
        0x8502_0504, // s_cselect_b32 s2, s4, s5
        LOAD,
        0xFA00_0000,
        END,
    ]);
    let tracked = track(&select_prog, &[TABLE], HwStage::Ps, None).unwrap();
    assert_eq!(tracked.scalar.len(), 1);
    assert_eq!(tracked.scalar[0].1.param, 0);
    assert_eq!(tracked.scalar[0].1.offset, 32);
    assert!(tracked.scalar[0].1.dynamic.is_some());

    // 4. Differing bases in a phi stay refused with "an address from a phi".
    let table2 = RootParam {
        user_data: 10,
        slot: 1,
        ..TABLE
    };
    let mixed = build(&[
        ZERO_HIGH,
        0xBF06_8009, // s_cmp_eq_u32 s9, 0
        0xBF85_0002, // s_cbranch_scc1 +2
        0xBE82_0308, // s_mov_b32 s2, s8
        0xBF82_0001, // s_branch +1
        0xBE82_030A, // s_mov_b32 s2, s10
        LOAD,
        0xFA00_0000,
        END,
    ]);
    assert_eq!(
        track(&mixed, &[TABLE, table2], HwStage::Ps, None).unwrap_err(),
        ShaderError::Unsupported {
            offset: 24,
            what: "an address from a phi",
        }
    );
}

#[test]
fn first_lane_reads_from_smem_uniform_valu_and_waterfall_table_address_trace() {
    // 1. Uniform index loaded via s_load_dword, shifted with v_lshrrev_b32,
    //    added to table base in VGPRs, then read back via v_readfirstlane_b32.
    let uniform_smem = build(&[
        0xBE84_0308, // s_mov_b32 s4, s8
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF400_0182,
        0xFA00_0000, // s_load_dword s6, s[4:5], null (SGPR -> uniform)
        0x7E02_0206, // v_mov_b32 v1, s6
        0x2C02_0282, // v_lshrrev_b32 v1, 2, v1 (uniform VALU)
        0x4A02_0208, // v_add_nc_u32 v1, s8, v1 (s8 + v1)
        0x7E04_0501, // v_readfirstlane_b32 s2, v1
        ZERO_HIGH,
        LOAD,
        0xFA00_0000,
        END,
    ]);
    let tracked = track(&uniform_smem, &[TABLE], HwStage::Ps, None).unwrap();
    let index = uniform_smem.blocks[0].insts[4].write(Reg::V(1)).unwrap();
    assert_eq!(
        tracked.scalar,
        [
            (
                (0, 2),
                Address {
                    param: 0,
                    offset: 0,
                    dynamic: None,
                }
            ),
            (
                (0, 8),
                Address {
                    param: 0,
                    offset: 0,
                    dynamic: Some(index),
                }
            ),
        ]
    );

    // 2. Per-lane table address in VGPRs read with v_readfirstlane_b32 inside
    //    a proven single-block waterfall loop that loads and uses a buffer descriptor.
    let words = [
        0x3402_0085, // 0x00: v_lshlrev_b32 v1, 5, v0 (per-lane!)
        0x4A04_0208, // 0x04: v_add_nc_u32 v2, s8, v1 (per-lane table address)
        0xBE85_0380, // 0x08: s_mov_b32 s5, 0
        0xBE94_037E, // 0x0c: s_mov_b32 s20, exec_lo
        // 0x10 (loop header):
        0x7E08_0502, // 0x10: v_readfirstlane_b32 s4, v2
        0xD4C2_0015,
        0x0002_0404, // 0x14: v_cmp_eq_u32_e64 s21, s4, v2
        0xBE95_3C15, // 0x1c: s_and_saveexec_b32 s21, s21
        0xF408_0402,
        0xFA00_0000, // 0x20: s_load_dwordx4 s[16:19], s[4:5], null
        0xE030_0000,
        0x8004_0300, // 0x28: buffer_load_dword v3, off, s[16:19], 0
        0x897E_157E, // 0x30: s_xor_b32 exec_lo, exec_lo, s21
        0xBF89_FFF6, // 0x34: s_cbranch_execnz -> 0x10 (-10 dwords from 0x38)
        0xBEFE_0314, // 0x38: s_mov_b32 exec_lo, s20
        END,
    ];
    let waterfall_addr = build(&words);
    let st = victory_shader::structure::structure(&waterfall_addr).unwrap();
    let tracked = track(&waterfall_addr, &[TABLE], HwStage::Ps, Some(&st)).unwrap();
    assert_eq!(tracked.scalar.len(), 1);
    let (load, address) = tracked.scalar[0];
    let insts = &waterfall_addr.blocks[load.0].insts;
    let read = insts[0].write(Reg::S(4)).unwrap();
    let index = waterfall_addr.blocks[0].insts[0].write(Reg::V(1)).unwrap();
    assert_eq!(
        address,
        Address {
            param: 0,
            offset: 0,
            dynamic: Some(index),
        }
    );
    assert_eq!(tracked.waterfall.reads, [read]);
    assert_eq!(tracked.waterfall.compares, [(load.0, 1)]);
    assert_eq!(tracked.waterfall.loads, [load]);

    // A self-loop is not enough: the compared vector must be the address read.
    let mut wrong_compare = words;
    wrong_compare[6] = 0x0002_0204; // v_cmp_eq_u32_e64 s21, s4, v1
    let wrong_compare = build(&wrong_compare);
    let st = victory_shader::structure::structure(&wrong_compare).unwrap();
    assert_eq!(
        track(&wrong_compare, &[TABLE], HwStage::Ps, Some(&st)).unwrap_err(),
        ShaderError::Unsupported {
            offset: 0x20,
            what: "an address read from the first lane of a per-lane value",
        }
    );

    // 3. Same per-lane table address read OUTSIDE a waterfall loop stays refused.
    let non_waterfall = build(&[
        0x3402_0085, // v_lshlrev_b32 v1, 5, v0
        0x4A04_0208, // v_add_nc_u32 v2, s8, v1
        0x7E08_0502, // v_readfirstlane_b32 s4, v2
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0402,
        0xFA00_0000, // s_load_dwordx4 s[16:19], s[4:5], null
        END,
    ]);
    let st = victory_shader::structure::structure(&non_waterfall).unwrap();
    assert_eq!(
        track(&non_waterfall, &[TABLE], HwStage::Ps, Some(&st)).unwrap_err(),
        ShaderError::Unsupported {
            offset: 16,
            what: "an address read from the first lane of a per-lane value",
        }
    );
}

// cargo test -p victory-shader --release --test resource_tracking \
//     bench_unrolled_loads -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn bench_unrolled_loads() {
    for (steps, loads, iterations) in [(16, 4, 1_000), (128, 4, 50), (384, 4, 20)] {
        let program = unrolled_loads(steps, loads);
        let layout = [TABLE];
        assert_eq!(
            track(&program, &layout, HwStage::Ps, None)
                .unwrap()
                .scalar
                .len(),
            (steps * loads) as usize
        );
        let mut samples = [0.0; 7];
        for sample in &mut samples {
            let start = Instant::now();
            for _ in 0..iterations {
                let tracked =
                    track(black_box(&program), black_box(&layout), HwStage::Ps, None).unwrap();
                black_box(tracked);
            }
            *sample = start.elapsed().as_secs_f64() * 1e6 / f64::from(iterations);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{steps:>3} steps x {loads} loads: {:>9.2} us/track (median)",
            samples[3]
        );
    }
}
