use super::*;

#[test]
fn thread_dimensions_cover_partial_groups_on_each_axis() {
    assert_eq!(
        thread_groups([65, 1, 1], [64, 1, 1]),
        Some(([2, 1, 1], true))
    );
    assert_eq!(
        thread_groups([10, 16, 5], [8, 8, 4]),
        Some(([2, 2, 2], true))
    );
    assert_eq!(
        thread_groups([82_606, 1, 1], [32, 1, 1]),
        Some(([2582, 1, 1], true))
    );
    assert_eq!(
        thread_groups([64, 8, 4], [32, 8, 4]),
        Some(([2, 1, 1], false))
    );
    assert_eq!(thread_groups([0, 1, 1], [64, 1, 1]), None);
    assert_eq!(thread_groups([u32::MAX, 1, 1], [64, 1, 1]), None);
}

#[cfg(windows)]
#[test]
#[ignore = "requires a D3D12 device with SM 6.6 support"]
fn dispatch_x_writes_only_requested_threads() {
    let rows = [
        (1, Some(0x8020), 1),
        (64, Some(0x8020), 64),
        (65, Some(0x8020), 65),
        (127, Some(0x20), 127),
        (0, Some(0x20), 0),
        (65, Some(0x20), 65),
        (2, None, 128),
        (2, Some(0x8000), 128),
        (65, Some(0), 65 * 64),
    ]
    .map(|(x, flags, n)| ([x, 1, 1], flags, [n, 1, 1]));
    check_grid(&kernel(false), [127 * 64, 1, 1], &rows);
    // Refusing a partial synchronized group must leave the following
    // command on the same pipeline able to run normally.
    check_grid(
        &kernel(true),
        [128, 1, 1],
        &[
            ([65, 1, 1], Some(0x20), [0, 1, 1]),
            ([2, 1, 1], None, [128, 1, 1]),
        ],
    );
    // No guest bounds check. Cut two axes at once, then all three.
    let extent = [16, 16, 8];
    check_grid(
        &voxel_kernel(),
        extent,
        &[
            (extent, Some(0x20), extent),
            ([10, 16, 5], Some(0x20), [10, 16, 5]),
            ([10, 13, 5], Some(0x8020), [10, 13, 5]),
            ([2, 2, 2], None, extent),
        ],
    );
}

#[cfg(windows)]
fn check_grid(code: &[u8], extent: [u32; 3], cases: &[([u32; 3], Option<u32>, [u32; 3])]) {
    #[repr(C)]
    struct ComputeDesc {
        root: This,
        code: *const u8,
        size: usize,
    }
    let host = crate::host::host().expect("D3D12 device");
    let null = std::ptr::null();
    let desc = ComputeDesc {
        root: null,
        code: code.as_ptr(),
        size: code.len(),
    };
    let (mut pipeline, mut allocator, mut list, mut heap) = (null, null, null, null);
    assert_eq!(
        crate::pipeline::create_compute(
            null,
            std::ptr::from_ref(&desc).cast(),
            &PAGEABLE_IIDS[0],
            &mut pipeline
        ),
        S_OK
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while crate::pipeline::host::bound_pipeline(pipeline, None, |_| None).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "compute PSO unavailable"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
        S_OK
    );
    assert_eq!(
        create_list(null, 0, 0, allocator, pipeline, &LIST_IIDS[1], &mut list),
        S_OK
    );
    let heap_desc = [0u32, 16, 1, 0, 0, 0, 0, 0];
    assert_eq!(
        crate::descriptor::create_heap(
            null,
            heap_desc.as_ptr().cast(),
            &PAGEABLE_IIDS[0],
            &mut heap
        ),
        S_OK
    );
    state::heaps(list, 1, &heap);
    // Separate, initially zero output for each command on the same list.
    let slots = extent.iter().product::<u32>();
    let mut outputs = Vec::new();
    for &([x, y, z], flags, writes) in cases {
        let bytes = u64::from(slots) * 4;
        let buffer = crate::resource::test_buffer(bytes);
        let address = crate::resource::buffer_bytes(buffer).unwrap().0 as u64;
        let handle = crate::descriptor::heap(heap).start + 32 * outputs.len() as u64;
        let uav = [39, 1, 0, 0, slots, 0, 0, 0, 1, 0];
        crate::descriptor::create_uav(null, buffer, null, &uav, handle);
        state::compute_table(list, 0, handle);
        match flags {
            Some(flags) => dispatch_x(list, x, y, z, flags),
            None => state::dispatch(list, x, y, z),
        }
        outputs.push((buffer, address, writes));
    }
    assert_eq!(close(list), S_OK);
    host.execute(&[&host_list(list).unwrap()]);
    let release = |o| object::slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
    for (buffer, address, writes) in outputs {
        let shadow = crate::shadow::find(address, u64::from(slots) * 4).unwrap();
        let output = shadow.bytes(address, u64::from(slots) * 4).unwrap();
        for (lane, word) in output.as_chunks::<4>().0.iter().enumerate() {
            let value = u32::from_le_bytes(*word);
            let i = lane as u32;
            let point = [
                i % extent[0],
                i / extent[0] % extent[1],
                i / (extent[0] * extent[1]),
            ];
            let inside = (0..3).all(|k| point[k] < writes[k]);
            assert_eq!(
                value,
                u32::from(inside),
                "voxel {point:?} inside extent {writes:?}"
            );
        }
        release(buffer);
    }
    for o in [list, allocator, pipeline, heap] {
        release(o);
    }
}

/// Synthetic compute shader in the regular DXBC/DXIL native container.
/// s4 points to a table with a single UAV. Each lane writes 1 at its
/// global x. A group barrier can be added to exercise the refusal path.
fn kernel(barrier: bool) -> Vec<u8> {
    let words: [u32; 12] = [
        0x7e04_0205, // v_mov_b32 v2, s5 (group X)
        0x3404_0486, // v_lshlrev_b32 v2, 6, v2
        0x4a04_0102, // v_add_nc_u32 v2, v2, v0 (local X)
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2 (byte offset)
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
        0x7e06_0281, // v_mov_b32 v3, 1
        0xe070_1000,
        0x8002_0302, // buffer_store_dword v3, v2, s[8:11], 0 offen
        if barrier { 0xbf8a_0000 } else { 0xbf80_0000 }, // s_barrier or s_nop
        0xbf81_0000, // s_endpgm
    ];
    native_compute(&words, [138, 64, 1, 1])
}

#[cfg(windows)]
fn voxel_kernel() -> Vec<u8> {
    // s5..s7 are the group IDs and v0..v2 the local IDs. Flatten into
    // a 16 x 16 x 8 UAV (x + 16*y + 256*z) and mark the resulting cell.
    let words = [
        0xd51a_0003,
        0x0000_0a83, // v_lshlrev_b32_e64 v3, 3, s5
        0x4a06_0103, // v_add_nc_u32_e32 v3, v3, v0
        0xd51a_0004,
        0x0000_0c83, // v_lshlrev_b32_e64 v4, 3, s6
        0x4a08_0304, // v_add_nc_u32_e32 v4, v4, v1
        0x3408_0884, // v_lshlrev_b32_e32 v4, 4, v4
        0x4a06_0903, // v_add_nc_u32_e32 v3, v3, v4
        0xd51a_0004,
        0x0000_0e82, // v_lshlrev_b32_e64 v4, 2, s7
        0x4a08_0504, // v_add_nc_u32_e32 v4, v4, v2
        0x3408_0888, // v_lshlrev_b32_e32 v4, 8, v4
        0x4a06_0903, // v_add_nc_u32_e32 v3, v3, v4
        0x3404_0682, // v_lshlrev_b32_e32 v2, 2, v3
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0x7e06_0281, // v_mov_b32_e32 v3, 1
        0xe070_1000,
        0x8002_0302, // buffer_store_dword v3, v2, s[8:11], 0 offen
        0xbf81_0000, // s_endpgm
    ];
    native_compute(&words, [(5 << 1) | (7 << 7) | (2 << 11), 8, 8, 4])
}

fn native_compute(words: &[u32], state: [u32; 4]) -> Vec<u8> {
    const RECORD: usize = 0x68;
    const STAGE: usize = RECORD + 0x54;
    let mut payload = vec![0; STAGE + 0x38];
    payload[..8].copy_from_slice(&6u64.to_le_bytes());
    for at in [8, 16, 24, 32, 40, 48, 72] {
        payload[at..][..8].copy_from_slice(&0xdead_beef_dead_beefu64.to_le_bytes());
    }
    payload[0x60..][..8].copy_from_slice(&(RECORD as u64).to_le_bytes());
    let code = payload.len() as u64;
    payload[RECORD..][..8].copy_from_slice(&code.to_le_bytes());
    payload[RECORD + 8..][..8].copy_from_slice(&(4 * words.len() as u64).to_le_bytes());
    payload[RECORD + 0x14] = 32; // Wave32
    payload[RECORD + 0x50..][..4].copy_from_slice(&(8u32 | 31 << 20).to_le_bytes());
    for (dst, value) in payload[STAGE + 0x24..]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(state)
    {
        dst.copy_from_slice(&value.to_le_bytes());
    }
    payload.extend(words.iter().copied().flat_map(u32::to_le_bytes));
    let table = payload.len() as u64;
    payload[RECORD + 0x28..][..8].copy_from_slice(&table.to_le_bytes());
    payload[RECORD + 0x30..][..8].copy_from_slice(&(table + 20).to_le_bytes());
    payload[RECORD + 0x38] = 1;
    payload[RECORD + 0x3c] = 1;
    payload.extend([3u32, 0, 1, 0, 4].into_iter().flat_map(u32::to_le_bytes));
    payload.extend(
        [0x303u32, 0, 0, 0, 0x404]
            .into_iter()
            .flat_map(u32::to_le_bytes),
    );
    let mut fssm = b"FSSM".to_vec();
    fssm.extend(0x101u32.to_le_bytes());
    fssm.extend((payload.len() as u64).to_le_bytes());
    fssm.extend(payload);
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&fssm, 6);
    let mut xbch = b"XBCH".to_vec();
    xbch.extend(3u32.to_le_bytes());
    xbch.extend((compressed.len() as u64).to_le_bytes());
    xbch.extend((fssm.len() as u64).to_le_bytes());
    xbch.extend(compressed);
    let mut part = vec![0; 24];
    part.extend(b"XDAT");
    part.extend(
        [1u32, 36, 13, 48, xbch.len() as u32]
            .into_iter()
            .flat_map(u32::to_le_bytes),
    );
    part.extend(xbch);
    let mut out = b"DXBC".to_vec();
    out.extend([0; 16]);
    out.extend(
        [1u32, 44 + part.len() as u32, 1, 36]
            .into_iter()
            .flat_map(u32::to_le_bytes),
    );
    out.extend(b"DXIL");
    out.extend((part.len() as u32).to_le_bytes());
    out.extend(part);
    out
}

#[test]
fn partial_groups_guard_writes_and_refuse_group_sync() {
    use victory_shader::emit::{self, DISPATCH_THREADS};
    let code = kernel(false);
    let (cut, _) = emit::translate_partial_dispatch(&code, true, &|_, _| None).unwrap();
    let full = emit::translate(&code).unwrap();
    assert_eq!(cut.compute_threads(), Some([64, 1, 1]));
    assert_eq!(full.compute_threads(), Some([64, 1, 1]));
    assert_ne!(cut.dxil, full.dxil);
    assert_eq!(
        cut.user_data
            .iter()
            .filter(|u| u.root == DISPATCH_THREADS)
            .map(|u| u.dword)
            .collect::<Vec<_>>(),
        [0]
    );
    assert!(full.user_data.iter().all(|u| u.root != DISPATCH_THREADS));
    let group_sync = kernel(true);
    assert!(emit::translate(&group_sync).is_ok());
    assert!(matches!(
        emit::translate_partial_dispatch(&group_sync, true, &|_, _| None),
        Err(victory_shader::ShaderError::Unsupported {
            what: "a partial dispatch with a group barrier",
            ..
        })
    ));
}
