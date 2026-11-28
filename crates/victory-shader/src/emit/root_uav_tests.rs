//! A synthetic compute shader writes one word through a spilled root descriptor.

use super::{SLOT_MEMORY, USER_ROOT_ADDRESS, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, resource, structure};

#[test]
fn spilled_root_uav_uses_the_title_address_directory() {
    let slot = 3;
    let words = [
        0x8701_FF01u32, // s_and_b32 s1, s1, 0xff (40-bit pointer)
        0xff,
        0xF404_0200, // s_load_dwordx2 s[8:9], s[0:1], 0
        0xFA00_0000,
        0xBE8A_0384, // descriptor size
        0xBE8B_0380, // remaining descriptor flags
        0x7E00_029D, // v_mov_b32 v0, 29
        0xE070_0004, // buffer_store_dword v0, off, s[8:11], 0 offset:4
        0x8002_0000,
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 1,
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
            rsrc2: 4 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::spill::build(&native).expect("spill build");
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

    assert!(lowered.root_srvs.contains(&SLOT_MEMORY));
    assert!(!lowered.root_uavs.contains(&slot));
    for kind in [USER_ROOT_ADDRESS, USER_ROOT_ADDRESS + 1] {
        assert!(
            lowered
                .user_data
                .iter()
                .any(|u| u.root == RootType::Other(kind) && u.slot == slot)
        );
    }
}
