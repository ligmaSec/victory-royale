//! GPU readback of synthetic, dynamically indexed buffer reads that feed an image store.

use super::*;

struct PixelPairs<const N: usize> {
    resource: ID3D12Resource,
    mapped: *const u8,
}

impl<const N: usize> PixelPairs<N> {
    fn new(host: &crate::host::Host) -> Self {
        let (resource, mapped) = host.readback_buffer((N * 8) as u64).unwrap();
        Self { resource, mapped }
    }

    fn values(&self) -> [[u32; 2]; N] {
        // SAFETY: the resource owns the mapping for N pairs; all fixture submissions
        // wait for their fence before this method reads the result.
        unsafe { self.mapped.cast::<[[u32; 2]; N]>().read_unaligned() }
    }
}

#[test]
fn gpu_selects_a_buffer_record_through_the_stage_binding() {
    exercise_image_pipeline(None);
}

#[test]
#[ignore = "synthetic large descriptor heap benchmark"]
fn prepare_gpu_selected_views_in_a_large_heap() {
    exercise_image_pipeline(Some((1_245_184, 77_000)));
}

fn exercise_image_pipeline(benchmark: Option<(u32, u32)>) {
    use victory_shader::native::{ComputeIo, RootParam, StageIo};
    use victory_shader::{HwStage, NativeProgram};
    let Some(host) = crate::host::host() else {
        eprintln!("skipped indexed buffer readback: no D3D12 device");
        return;
    };
    // Select an SRV via s1, read the record in s2 at byte 4 and write it to UAV slot 0.
    let words = [
        0xbe840300u32,
        0xbe850380u32,
        0x89068101u32,
        0x8f068506u32,
        0xf4080202u32,
        0x0c000000u32,
        0xf40c0402u32,
        0xfa000000u32,
        0xbf8cc07fu32,
        0x7e000202u32,
        0xe0342004u32,
        0x80020100u32,
        0x7e060280u32,
        0x7e080280u32,
        0x7e0a0280u32,
        0xbf8c3f70u32,
        0xf0200f08u32,
        0x00040104u32,
        0xbf810000u32,
    ];
    let root = |root, slot| RootParam {
        root,
        slot,
        user_data: slot,
        sgprs: 1,
        table_offset_dwords: 0,
        base_register: 0,
    };
    let shader = crate::pipeline::Shader::test_compute(NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 6,
        layout: vec![
            root(RootType::ResourceHeap, 0),
            root(RootType::Constants, 1),
            root(RootType::Constants, 2),
        ],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 4,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    });
    assert_eq!(
        shader.translated().unwrap().host.root_srvs,
        [victory_shader::emit::BUFFER_TABLE_SRV]
    );
    let pipeline = HostPipeline::new(&[shader], None, &[]).expect("indexed compute pipeline");
    let mut backing: Vec<u32> = (0..64).map(|i| 0x6200_0000 + i).collect();
    let base = backing.as_mut_ptr() as u64;
    crate::shadow::register(base as usize, base, 256);
    let _shadow = ShadowGuard {
        resource: base as usize,
        address: base,
    };
    #[repr(C)]
    struct HeapDesc {
        desc: D3D12_DESCRIPTOR_HEAP_DESC,
        placement: [u64; 2],
    }
    let desc = HeapDesc {
        desc: D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: benchmark.map_or(8, |(count, _)| count),
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        },
        placement: [0; 2],
    };
    let mut views = std::ptr::null();
    let iid = ID3D12DescriptorHeap::IID;
    assert_eq!(
        crate::descriptor::create_heap(
            std::ptr::null(),
            (&raw const desc).cast(),
            (&raw const iid).cast(),
            &mut views
        ),
        crate::object::S_OK
    );
    let _views = TitleObject(views);
    let heap = crate::descriptor::heap(views);
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_SAMPLE_DESC,
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 1,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut image: Option<ID3D12Resource> = None;
    // SAFETY: the descriptor requests a single typed UAV texture on the live device.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
                &mut image,
            )
            .unwrap()
    };
    let out = image.unwrap();
    // SAFETY: the live view heap contains the reserved output slot at its start.
    let image_slot = unsafe {
        heap.host()
            .unwrap()
            .heap
            .GetCPUDescriptorHandleForHeapStart()
    };
    // SAFETY: the typed image supports a default UAV and the output slot belongs to this fixture.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&out, None, None, image_slot)
    };
    let sample = host.default_buffer(512).unwrap();
    // SAFETY: the open list has a bound compute pipeline and live 1x1 UAV. The
    // aligned footprint fits the 512-byte staging buffer; each destination has
    // room for two dwords at offset. All resources outlive execution.
    let dispatch = |list: &HostList, dest: &ID3D12Resource, offset: u64| unsafe {
        list.list.Dispatch(1, 1, 1);
        list.list.ResourceBarrier(&[crate::host::transition(
            &out,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        let target = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&sample),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: desc.Format,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let source = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&out),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.list.CopyTextureRegion(&target, 0, 0, 0, &source, None);
        list.list.ResourceBarrier(&[crate::host::transition(
            &out,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        list.list.ResourceBarrier(&[crate::host::transition(
            &sample,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.list.CopyBufferRegion(dest, offset, &sample, 0, 8);
        list.list.ResourceBarrier(&[crate::host::transition(
            &sample,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
    };
    let mut resource = [0u32; 14];
    resource[0] = 1;
    resource[4] = 256;
    // The first view starts at byte 12 and has stride 12. The second starts
    // at byte 32 and has stride 32. The last contains only one record.
    let set_view = |slot: u64, address: u64, stride, count| {
        let mut words = [0u32; 16];
        words[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        words[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        words[4] = 1; // FirstElement
        words[6] = count;
        words[7] = stride;
        words[10] = address as u32;
        words[11] = (address >> 32) as u32;
        crate::descriptor::create_placed_srv(
            std::ptr::null(),
            &resource,
            &words,
            heap.start + slot * 32,
        );
    };
    for (slot, stride, count) in [(1, 12, 2), (2, 32, 2), (3, 16, 1)] {
        set_view(slot, base, stride, count);
    }
    let buffer = PixelPairs::<6>::new(host);
    let mut state = State::default();
    state.set_heaps(&[views]);
    let list = host.new_list().unwrap();
    // The last two addresses wrap during the multiplication or the offset addition.
    let cases = [
        (1, 1),
        (2, 1),
        (3, 1),
        (99, 1),
        (2, 0x0800_0001),
        (1, 0x1555_5555),
    ];
    for (i, (selected, record)) in cases.into_iter().enumerate() {
        state.set_constants(true, 2, &[record], 0);
        state.set_constants(true, 1, &[selected ^ 1], 0);
        assert!(state.bind(&list, &pipeline, true));
        dispatch(&list, &buffer.resource, 8 * i as u64);
    }
    list.close();
    host.execute(&[&list]);
    let got = buffer.values();
    assert_eq!(
        got,
        [
            [backing[7], backing[8]],
            [backing[17], backing[18]],
            [0; 2],
            [0; 2],
            [0; 2],
            [0; 2]
        ]
    );
    // The same compiled pipeline and closed list must see the new stride and then the shortened view.
    for (stride, count, expected) in [
        (20, 2, [backing[11], backing[12]]),
        (20, 1, [0; 2]),
        (8, 2, [backing[5], 0]),
        (12, 2, [backing[7], backing[8]]),
    ] {
        set_view(1, base, stride, count);
        host.execute(&[&list]);
        let again = buffer.values();
        assert_eq!(again[0], expected);
        assert_eq!(again[1..], got[1..]);
    }
    let mut replacement: Vec<u32> = (0..64).map(|i| 0x4300_0000 + i).collect();
    let second = replacement.as_mut_ptr() as u64;
    crate::shadow::register(second as usize, second, 256);
    let _second = ShadowGuard {
        resource: second as usize,
        address: second,
    };
    set_view(1, second, 12, 2);
    host.execute(&[&list]);
    let values = buffer.values();
    assert_eq!(values[0], [replacement[7], replacement[8]]);
    assert_eq!(values[1..], got[1..]);
    replacement[7] = 0x1234_5678;
    host.execute(&[&list]);
    let values = buffer.values();
    assert_eq!(values[0], [replacement[7], replacement[8]]);
    assert_eq!(values[1..], got[1..]);
    // A preceding list writes the selected backing on the GPU. The
    // deferred preparation of the already closed reader must retain it.
    let producer = host.new_list().unwrap();
    let shadow = crate::shadow::find(second, 256).unwrap();
    let (source, bytes) = host.upload_buffer(8).unwrap();
    let produced = [0x7331_0011_u32, 0x7331_0022];
    // SAFETY: the live upload is mapped for eight bytes.
    unsafe { bytes.cast::<[u32; 2]>().write_unaligned(produced) };
    shadow.refresh_as(&producer, second + 28, 8, D3D12_RESOURCE_STATE_COPY_DEST);
    shadow.gpu_writes(&producer, second + 28, 8);
    // SAFETY: the upload and destination contain the full eight bytes.
    unsafe {
        producer
            .list
            .CopyBufferRegion(&shadow.buffer, 28, &source, 0, 8)
    };
    producer.close();
    host.execute(&[&producer, &list]);
    let values = buffer.values();
    assert_eq!(values[0], produced);
    assert_eq!(values[1..], got[1..]);
    assert_eq!(replacement[7..9], produced);
    // Copy into the backing, consume it, then copy into it again in the same list.
    let combined_out = PixelPairs::<1>::new(host);
    let combined = host.new_list().unwrap();
    let mut combined_state = State::default();
    combined_state.set_heaps(&[views]);
    combined_state.set_constants(true, 2, &[1], 0);
    combined_state.set_constants(true, 1, &[0], 0);
    let next = [0x7000_0011_u32, 0x7000_0022];
    // SAFETY: the preceding submission completed and this upload mapping owns eight bytes.
    unsafe { bytes.cast::<[u32; 2]>().write_unaligned(next) };
    shadow.refresh_as(&combined, second + 28, 8, D3D12_RESOURCE_STATE_COPY_DEST);
    shadow.gpu_writes(&combined, second + 28, 8);
    // SAFETY: the open list retains both buffers and the eight-byte copy fits their ranges.
    unsafe {
        combined
            .list
            .CopyBufferRegion(&shadow.buffer, 28, &source, 0, 8)
    };
    assert!(combined_state.bind(&combined, &pipeline, true));
    assert_eq!(
        combined.recorded_shadow_state(&shadow.buffer),
        Some(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)
    );
    dispatch(&combined, &combined_out.resource, 0);
    shadow.refresh_as(&combined, second + 36, 8, D3D12_RESOURCE_STATE_COPY_DEST);
    assert_eq!(
        combined.recorded_shadow_state(&shadow.buffer),
        Some(D3D12_RESOURCE_STATE_COPY_DEST)
    );
    shadow.gpu_writes(&combined, second + 36, 8);
    // SAFETY: the open list retains both buffers and the eight-byte copy fits their ranges.
    unsafe {
        combined
            .list
            .CopyBufferRegion(&shadow.buffer, 36, &source, 0, 8)
    };
    combined.close();
    host.execute(&[&combined, &list]);
    assert_eq!(replacement[7..11], [next[0], next[1], next[0], next[1]]);
    assert_eq!(combined_out.values()[0], next);
    assert_eq!(buffer.values()[0], next);
    // Record a reader, followed by the first explicit access to this
    // backing. Retarget the descriptor only after the list is closed.
    let late = host.new_list().unwrap();
    let mut late_state = State::default();
    late_state.set_heaps(&[views]);
    late_state.set_constants(true, 2, &[1], 0);
    late_state.set_constants(true, 1, &[0], 0);
    let early_out = PixelPairs::<1>::new(host);
    let old = [backing[7], backing[8]];
    assert!(late_state.bind(&late, &pipeline, true));
    dispatch(&late, &early_out.resource, 0);
    let later_target = crate::shadow::find(base, 256).unwrap();
    later_target.refresh_as(&late, base + 28, 8, D3D12_RESOURCE_STATE_COPY_DEST);
    later_target.gpu_writes(&late, base + 28, 8);
    // SAFETY: the eight-byte upload and the backing at offset 28 are live for this copy.
    unsafe {
        late.list
            .CopyBufferRegion(&later_target.buffer, 28, &source, 0, 8)
    };
    late.close();
    set_view(1, base, 12, 2);
    host.execute(&[&late, &list]);
    assert_eq!(early_out.values()[0], old);
    assert_eq!(buffer.values()[0], next);
    assert_eq!(&backing[7..9], &next);
    // Give all spare slots to other recordings. A copied view can use
    // its backing's live alias. A new backing must produce null until
    // space returns, without affecting either existing reader.
    let pressure = host.new_list().unwrap();
    let mut check_state = State::default();
    check_state.set_heaps(&[views]);
    let checks = PixelPairs::<3>::new(host);
    for (i, slot) in [1, 4, 5].into_iter().enumerate() {
        check_state.set_constants(true, 1, &[slot ^ 1], 0);
        check_state.set_constants(true, 2, &[1], 0);
        assert!(check_state.bind(&pressure, &pipeline, true));
        dispatch(&pressure, &checks.resource, i as u64 * 8);
    }
    pressure.close();
    let pressure_read = || {
        host.execute(&[&pressure]);
        checks.values()
    };
    assert_eq!(pressure_read(), [next, [0; 2], [0; 2]]);
    // A deferred title copy replaces the selected descriptor between closed lists.
    let copy_view = host.new_list().unwrap();
    copy_view.defer_copy(
        (heap.start + 32) as *mut u8,
        (heap.start + 64) as *const u8,
        32,
    );
    copy_view.close();
    host.execute(&[&pressure, &copy_view, &list]);
    let before_copy = checks.values()[0];
    let after_copy = buffer.values()[0];
    let alternate = [backing[17], backing[18]];
    assert_eq!(
        (before_copy, after_copy),
        (next, alternate),
        "ordered descriptor retarget"
    );
    set_view(1, base, 12, 2);
    let ordered = PixelPairs::<2>::new(host);
    crate::list::test_copy_between_selected_reads(
        heap.start + 32,
        heap.start + 64,
        |list, state, offset| {
            if offset == 0 {
                state.set_heaps(&[views]);
                state.set_constants(true, 1, &[0], 0);
                state.set_constants(true, 2, &[1], 0);
            }
            assert!(state.bind(list, &pipeline, true));
            dispatch(list, &ordered.resource, offset);
        },
    );
    assert_eq!(
        ordered.values(),
        [next, alternate],
        "same title list descriptor retarget"
    );
    set_view(1, base, 12, 2);
    let host_heap = heap.host().unwrap();
    let mut held = Vec::new();
    for power in (0..=18).rev() {
        let n = 1 << power;
        while let Some(at) = host_heap.dynamic(n) {
            held.push((at, n));
        }
    }
    let mut third: Vec<u32> = (0..64).map(|i| 0x3210_0000 + i).collect();
    let third_base = third.as_mut_ptr() as u64;
    crate::shadow::register(third_base as usize, third_base, 256);
    let _third = ShadowGuard {
        resource: third_base as usize,
        address: third_base,
    };
    set_view(4, base, 32, 2);
    set_view(5, third_base, 12, 2);
    assert_eq!(pressure_read(), [next, [backing[17], backing[18]], [0; 2]]);
    for (at, n) in held {
        host_heap.release(at, n);
    }
    assert_eq!(
        pressure_read(),
        [next, [backing[17], backing[18]], [third[7], third[8]]]
    );
    if let Some((count, occupancy)) = benchmark {
        for slot in 4..4 + occupancy {
            set_view(u64::from(slot), base, 12, 2);
        }
        let host_heap = heap.host().unwrap();
        let measured = |project: bool| {
            let mut results = Vec::new();
            let mut visited = 0;
            for _ in 0..24 {
                let start = std::time::Instant::now();
                std::hint::black_box(heap.sync_direct_copies(true));
                if project {
                    assert!(host_heap.prepare_selected(heap, |_, start, len| {
                        visited += 1;
                        std::hint::black_box((start, len));
                    }));
                }
                results.push(start.elapsed().as_micros());
            }
            let cold = results.remove(0);
            results.sort_unstable();
            eprintln!(
                "heap requested={count} mirrored={} rows={occupancy} projection={project} visits={visited} metadata_bytes={} first_us={cold} warm_median_us={} warm_p95_us={} max_us={}",
                host_heap.count(),
                (u64::from(host_heap.count()) + 2) * 16,
                results[11],
                results[21],
                results[22]
            );
        };
        measured(false);
        measured(true);
    }
}
