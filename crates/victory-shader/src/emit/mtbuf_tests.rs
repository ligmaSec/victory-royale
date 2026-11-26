//! Synthetic normalized MTBUF loads. gfx1036 measurements in #1060 found
//! correctly rounded results for all twenty formats, including packed fields.

use super::{Emission, HostShader, StructuredBinding, ViewStride, emit_with_view_strides};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{Container, FourCC, io, ir, resource, structure};

// UNORM format and field widths from low to high. The next format is SNORM.
const FORMATS: &[(u8, &[u32])] = &[
    (1, &[8]),
    (7, &[16]),
    (14, &[8, 8]),
    (23, &[16, 16]),
    (30, &[11, 11, 10]),
    (37, &[10, 11, 11]),
    (44, &[2, 10, 10, 10]),
    (50, &[10, 10, 10, 2]),
    (56, &[8, 8, 8, 8]),
    (65, &[16, 16, 16, 16]),
];

/// Each thread reads one 16-byte row of a raw SRV, then writes its four
/// converted dwords to the corresponding row of a separate raw UAV.
fn load_shader(format: u8) -> HostShader {
    let words = [
        0x8F02_8601, // s_lshl_b32 s2, s1, 6 (group ID x times 64)
        0x4A00_0002, // v_add_nc_u32 v0, s2, v0 (plus thread ID x)
        0x3400_0084, // v_lshlrev_b32 v0, 4, v0 (byte offset)
        0xBE81_0380, // s_mov_b32 s1, 0 (high half of table address)
        0xF408_0200,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[0:1], 0
        0xF408_0300,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[0:1], 32
        0xE803_1000 | (u32::from(format) << 19),
        0x8002_0100, // tbuffer_load_format_xyzw v[1:4], v0, s[8:11], 0 offen
        0xE078_1000,
        0x8003_0100, // buffer_store_dwordx4 v[1:4], v0, s[12:15], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    buffer_shader(&words, Some(0), &|_, _| None).unwrap()
}

fn buffer_shader(
    words: &[u32],
    input_stride: Option<u32>,
    view_stride: ViewStride<'_>,
) -> Result<HostShader, crate::ShaderError> {
    let range = |kind, table_offset_dwords, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords: 4,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 5,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: input_stride
            .map(|stride| range(ResourceType::Srv, 0, stride))
            .into_iter()
            .chain([range(ResourceType::Uav, 8, 0)])
            .collect(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 7 | 1 << 1, // group ID x and one user SGPR
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let bytes = crate::dxbc::build(&[]);
    let container = Container::parse(&bytes).unwrap();
    emit_with_view_strides(
        Emission {
            c: &container,
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
        },
        true,
        Some(view_stride),
    )
}

#[test]
fn raw_buffer_byte_and_halfword_loads_extend_at_every_byte_offset() {
    // One lane per byte offset. All four loads use the same address.
    let words = [
        0xbe81_0380, // upper half of table address
        0xf408_0200,
        0xfa00_0000, // SRV in s[8:11]
        0xf408_0300,
        0xfa00_0020, // UAV in s[12:15]
        0xe020_1000,
        0x8002_0100, // buffer_load_ubyte v1 at v0
        0xe024_1000,
        0x8002_0200, // buffer_load_sbyte v2
        0xe028_1000,
        0x8002_0300, // buffer_load_ushort v3
        0xe02c_1000,
        0x8002_0400, // buffer_load_sshort v4
        0x3400_0084, // output byte offset = lane * 16
        0xe078_1000,
        0x8003_0100, // store four result dwords
        0xbf81_0000,
    ];
    let shader = buffer_shader(&words, Some(0), &|_, _| None).unwrap();
    assert!(
        Container::parse(&shader.dxil)
            .unwrap()
            .part(FourCC::DXIL)
            .is_some()
    );

    #[cfg(windows)]
    {
        use super::host::{Dispatch, Host, Root, Stride, View};
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = (0u8..68)
            .map(|n| n.wrapping_mul(91).wrapping_add(17))
            .collect();
        let data: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(u32::from_le_bytes)
            .collect();
        let expected: Vec<u32> = bytes
            .windows(2)
            .take(64)
            .flat_map(|b| {
                let half = u16::from_le_bytes([b[0], b[1]]);
                [
                    u32::from(b[0]),
                    b[0] as i8 as i32 as u32,
                    u32::from(half),
                    half as i16 as i32 as u32,
                ]
            })
            .collect();
        let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
        for &(name, warp) in super::valu_tests::backends(force_warp) {
            let Ok(host) = Host::new(warp) else { continue };
            let (root, pso) = host.compute_pipeline(&shader).unwrap();
            let output = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(
                            0,
                            Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                        )],
                        views: &[
                            View::Buffer {
                                data: &data,
                                stride: Stride::Raw,
                            },
                            View::RawUav(256),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            assert_eq!(output[0], expected, "{name}");
            eprintln!("{name}: verified all 256 extension results");
        }
    }
}

#[test]
fn raw_halfword_stores_preserve_neighbors_and_drop_incomplete_writes() {
    // Each lane reads a synthetic address, value and two initial dwords.
    // The second scenario shares dwords between pairs of concurrent lanes.
    for shared in [false, true] {
        let mut words = vec![
            0xbe81_0380,
            0xf408_0200,
            0xfa00_0000, // raw SRV
            0xf408_0300,
            0xfa00_0020, // raw UAV
            0x3400_0084, // v0 = lane * 16
            0xe038_1000,
            0x8002_0100, // buffer_load_dwordx4 v[1:4]
        ];
        if !shared {
            words.extend([
                0xe074_1000,
                0x8003_0300, // initialize v0..v0+7
                0xe074_1008,
                0x8003_0300, // initialize v0+8..v0+15
                0x3606_00ff,
                128,         // v3 = (lane * 16) & 128
                0x7daa_0680, // v_cmpx_ne_u32 exec, 0, v3
            ]);
        }
        words.extend([
            0xe068_1000,
            0x8003_0201, // store low half of v2 at byte v1
            0xbf81_0000,
        ]);
        let shader = buffer_shader(&words, Some(0), &|_, _| None).unwrap();
        assert!(
            Container::parse(&shader.dxil)
                .unwrap()
                .part(FourCC::DXIL)
                .is_some()
        );

        #[cfg(windows)]
        {
            use super::host::{Dispatch, Host, Root, Stride, View};
            let _one = super::valu_tests::ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut data = Vec::new();
            let mut expected = vec![0u8; if shared { 128 } else { 1024 }];
            for lane in 0..64u32 {
                let address = if shared {
                    2 * lane
                } else {
                    let start = lane as usize * 16;
                    for row in expected[start..start + 16].chunks_exact_mut(8) {
                        row.copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
                    }
                    match lane {
                        63 => 1023, // only the first byte would fit
                        62 => u32::MAX,
                        _ => lane * 16 + lane % 4,
                    }
                };
                let value = 0xdead_a17e ^ (lane * 197);
                data.extend([address, value, 0x6745_2301, 0xefcd_ab89]);
                if (shared || lane & 8 != 0)
                    && let Some(bytes) = expected
                        .get_mut(address as usize..)
                        .and_then(|s| s.get_mut(..2))
                {
                    bytes.copy_from_slice(&(value as u16).to_le_bytes());
                }
            }
            let expected: Vec<_> = expected
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .map(u32::from_le_bytes)
                .collect();
            let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
            let mut checked = false;
            for &(name, warp) in super::valu_tests::backends(force_warp) {
                let Ok(host) = Host::new(warp) else { continue };
                let (root, pso) = host.compute_pipeline(&shader).unwrap();
                let output = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(
                                0,
                                Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                            )],
                            views: &[
                                View::Buffer {
                                    data: &data,
                                    stride: Stride::Raw,
                                },
                                View::RawUav(expected.len() as u32),
                            ],
                            samplers: &[],
                            groups: [1, 1, 1],
                        },
                    )
                    .unwrap();
                assert_eq!(output[0], expected, "{name}, shared dwords = {shared}");
                checked = true;
                eprintln!("{name}: verified halfword stores, shared dwords = {shared}");
            }
            assert!(checked, "no SM 6.6 device for halfword store readback");
        }
    }
}

#[test]
fn normalized_mtbuf_loads_need_no_int64() {
    for &(unorm, _) in FORMATS {
        for format in [unorm, unorm + 1] {
            let shader = load_shader(format);
            let c = Container::parse(&shader.dxil).unwrap();
            let sfi0 = c.part(FourCC::SFI0).unwrap().data;
            let flags = u64::from_le_bytes(sfi0.try_into().unwrap());
            assert_eq!(flags & 0x8000, 0, "format {format}, SFI0 {flags:#x}");
            #[cfg(windows)]
            if let Ok(mut validator) = super::validator::Validator::load(
                super::validator::Validator::default_path().as_deref(),
            ) {
                validator.validate(&mut shader.dxil.clone()).unwrap();
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn normalized_mtbuf_loads_round_every_field_value() {
    use super::host::{Dispatch, Host, Root, Stride, View};
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // `VICTORY_WARP=1` makes `Host::new(false)` WARP too: the shared list
    // then runs it once, labelled as such.
    let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
    let hosts: Vec<_> = super::valu_tests::backends(force_warp)
        .iter()
        .filter_map(|&(name, warp)| Host::new(warp).ok().map(|h| (name, h)))
        .collect();
    assert!(!hosts.is_empty(), "no SM 6.6 device");

    for &(unorm, widths) in FORMATS {
        let rows = 1u32 << widths.iter().max().unwrap();
        for signed in [false, true] {
            let format = unorm + u8::from(signed);
            let shader = load_shader(format);
            let mut data = Vec::new();
            let mut expected = Vec::new();
            for row in 0..rows {
                let mut packed = 0u64;
                let mut shift = 0;
                for (channel, &width) in widths.iter().enumerate() {
                    // Distinguish the channels and visit every field value.
                    let raw = (row + channel as u32 * 71) & ((1 << width) - 1);
                    packed |= u64::from(raw) << shift;
                    shift += width;
                    let max = (1i32 << (width - u32::from(signed))) - 1;
                    let k = if signed && raw > max as u32 {
                        raw as i32 - (1 << width)
                    } else {
                        raw as i32
                    };
                    let ratio = f64::from(k.max(-max)) / f64::from(max);
                    expected.push((ratio as f32).to_bits());
                }
                data.extend([packed as u32, (packed >> 32) as u32, 0, 0]);
            }
            for (name, host) in &hosts {
                let (root, pso) = host.compute_pipeline(&shader).unwrap();
                let out = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(
                                0,
                                Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                            )],
                            views: &[
                                View::Buffer {
                                    data: &data,
                                    stride: Stride::Raw,
                                },
                                View::RawUav(4 * rows),
                            ],
                            samplers: &[],
                            groups: [rows / 64, 1, 1],
                        },
                    )
                    .unwrap();
                assert_eq!(out[0].len(), 4 * rows as usize, "{name}: format {format}");
                for (row, (got, want)) in out[0]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(expected.chunks_exact(widths.len()))
                    .enumerate()
                {
                    assert_eq!(
                        &got[..widths.len()],
                        want,
                        "{name}: format {format}, row {row}"
                    );
                }
            }
        }
    }
}

// Read dword 1 of each record, then copy it to a raw output at 4 * thread ID.
const STRUCTURED_READ: &[u32] = &[
    0xBE81_0380, // s_mov_b32 s1, 0
    0xF408_0200,
    0xFA00_0000, // s_load_dwordx4 s[8:11], s[0:1], 0
    0xF408_0300,
    0xFA00_0020, // s_load_dwordx4 s[12:15], s[0:1], 32
    0xE030_2004,
    0x8002_0100, // buffer_load_dword v1, v0, s[8:11], 0 idxen offset:4
    0x3400_0082, // v_lshlrev_b32 v0, 2, v0
    0xE070_1000,
    0x8003_0100, // buffer_store_dword v1, v0, s[12:15], 0 offen
    0xBF81_0000, // s_endpgm
];

#[test]
fn indexed_load_uses_the_view_stride_when_metadata_is_missing_or_invalid() {
    for metadata in [None, Some(0), Some(6), Some(2052)] {
        assert!(matches!(
            buffer_shader(STRUCTURED_READ, metadata, &|_, _| None),
            Err(crate::ShaderError::Unsupported {
                what: super::UNKNOWN_STRUCTURED_STRIDE,
                ..
            })
        ));
        for stride in [16, 32] {
            let called = std::cell::Cell::new(false);
            buffer_shader(STRUCTURED_READ, metadata, &|binding, required| {
                if !required {
                    return None;
                }
                called.set(true);
                assert_eq!(
                    binding,
                    StructuredBinding {
                        slot: 0,
                        offset_dwords: 0,
                        uav: false
                    }
                );
                Some(stride)
            })
            .unwrap();
            assert!(called.get());
        }
    }
}

#[cfg(windows)]
#[test]
fn undeclared_structured_stride_reads_two_view_strides() {
    use super::host::{Dispatch, Host, Root, Stride, View};

    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let force_warp = std::env::var("VICTORY_WARP").is_ok_and(|v| v == "1");
    let hosts: Vec<_> = super::valu_tests::backends(force_warp)
        .iter()
        .filter_map(|&(name, warp)| match Host::new(warp) {
            Ok(host) => Some((name, host)),
            Err(error) => {
                eprintln!("{name}: {error}");
                None
            }
        })
        .collect();
    for stride in [16, 32] {
        let mut shader = buffer_shader(STRUCTURED_READ, Some(0), &|_, required| {
            required.then_some(stride)
        })
        .unwrap();
        let path = super::validator::Validator::default_path();
        if path.is_some() {
            super::validator::Validator::load(path.as_deref())
                .unwrap()
                .validate(&mut shader.dxil)
                .unwrap();
        }
        let expected: Vec<u32> = (0..64).map(|i| 0x1234_0000 + i * 61 + stride).collect();
        let mut data = vec![0xdead_beef; 64 * stride as usize / 4];
        for (record, value) in data.chunks_exact_mut(stride as usize / 4).zip(&expected) {
            record[1] = *value;
        }
        for (name, host) in &hosts {
            let (root, pso) = host.compute_pipeline(&shader).unwrap();
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(
                            0,
                            Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                        )],
                        views: &[
                            View::Buffer {
                                data: &data,
                                stride: Stride::Structured(stride),
                            },
                            View::RawUav(64),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            assert_eq!(out[0], expected, "{name}, stride {stride}");
            eprintln!("{name}: exact readback at stride {stride}");
        }
    }
}

#[test]
fn indexed_store_and_atomic_use_the_structured_uav_view_stride() {
    for opcode in [0x1C_u32, 0x32_u32] {
        let words = [
            0xBE81_0380, // s_mov_b32 s1, 0
            0xF408_0300,
            0xFA00_0020, // s_load_dwordx4 s[12:15], s[0:1], 32
            0xE000_2004 | (opcode << 18),
            0x8003_0000, // store / atomic_add v0, v0, s[12:15], 0 idxen offset:4
            0xBF81_0000, // s_endpgm
        ];
        assert!(buffer_shader(&words, None, &|_, _| None).is_err());
        let shader = buffer_shader(&words, None, &|binding, required| {
            if !required {
                return None;
            }
            assert_eq!(
                binding,
                StructuredBinding {
                    slot: 0,
                    offset_dwords: 8,
                    uav: true
                }
            );
            Some(16)
        })
        .unwrap();
        #[cfg(windows)]
        if let Some(path) = super::validator::Validator::default_path() {
            super::validator::Validator::load(Some(&path))
                .unwrap()
                .validate(&mut shader.dxil.clone())
                .unwrap();
        }
        assert!(!shader.dxil.is_empty());
    }
}
