use super::*;

mod device_tests {
    /// A list of ours is found by its name, with the debug layer's note or not.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_list_is_found_by_the_number_in_its_name() {
        assert_eq!(super::list_number("victory host list 7"), Some(7));
        assert_eq!(
            super::list_number("victory host list 158 (debug layer indirect)"),
            Some(158)
        );
        assert_eq!(super::list_number("victory host list 7x"), None);
        assert_eq!(super::list_number("victory host list 7 x"), None);
        assert_eq!(super::list_number("another list 7"), None);
        assert_eq!(super::list_number(""), None);
    }

    /// What is there of a range some of whose pages are not: those bytes,
    /// counted, and zeros for the rest, whatever the buffer held.
    #[test]
    fn what_is_there_is_read_and_the_rest_is_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const SIZE: usize = 0x8_0000;
        // SAFETY: a reservation of ours; only committed pages are written.
        unsafe {
            let at = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!at.is_null());
            // 0-0x1_1000 and 0x3_0000-0x4_0000 are there.
            for (from, len) in [(0, 0x1_1000), (0x3_0000, 0x1_0000)] {
                assert!(
                    !VirtualAlloc(at.add(from).cast(), len, MEM_COMMIT, PAGE_READWRITE).is_null()
                );
                at.add(from).write_bytes(0x5a, len);
            }
            // From an address inside a page, as textures are packed.
            let mut out = vec![0x77u8; SIZE - 0x300];
            assert_eq!(
                super::read_there(at.add(0x300), &mut out),
                0x1_1000 - 0x300 + 0x1_0000
            );
            for (i, &b) in out.iter().enumerate() {
                let from = i + 0x300;
                let there = from < 0x1_1000 || (0x3_0000..0x4_0000).contains(&from);
                assert_eq!(b, if there { 0x5a } else { 0 }, "byte {from:#x}");
            }
            // All there: one read.
            let mut out = vec![0x77u8; 0x1_0000];
            assert_eq!(super::read_there(at.add(0x1000), &mut out), 0x1_0000);
            assert!(out.iter().all(|&b| b == 0x5a));
            // Nothing there.
            let mut out = vec![0x77u8; 0x1_0000];
            assert_eq!(super::read_there(at.add(0x5_0000), &mut out), 0);
            assert!(out.iter().all(|&b| b == 0));
            assert_ne!(VirtualFree(at.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// Zeros for a title destination that went meanwhile are no fault.
    #[test]
    fn zeros_where_nothing_is_are_lost() {
        use windows_sys::Win32::System::Memory::*;
        // SAFETY: a reservation of ours, its first page committed.
        unsafe {
            let at =
                VirtualAlloc(std::ptr::null(), 1 << 16, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE).is_null());
            at.write_bytes(7, 4096);
            super::zero(at, 4096);
            assert!(std::slice::from_raw_parts(at, 4096).iter().all(|&b| b == 0));
            super::zero(at.add(4096), 4096);
            VirtualFree(at.cast(), 0, MEM_RELEASE);
        }
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(
                    VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                    at.cast()
                );
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(
                        VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old),
                        0
                    );
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(
                        VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                        at.cast()
                    );
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

mod readback_pool_tests {
    use super::*;

    #[test]
    fn readback_pool_leases_are_exclusive_until_retired_and_idle_bytes_are_bounded() {
        const CLASS: u64 = 64 << 10;
        let pool = ReadbackPool::new();
        let mut created = 0;
        let mut allocate = |bytes| {
            assert_eq!(bytes, CLASS);
            created += 1;
            Some(created)
        };
        let first = pool.take(32 << 10, CLASS, &mut allocate).unwrap();
        let second = pool.take(32 << 10, CLASS, &mut allocate).unwrap();
        assert_eq!(*first.resource(), 1);
        assert_eq!(*second.resource(), 2);
        // Inspecting the same recording again (as a repeated submission
        // does at landing) cannot make its still-owned buffer available.
        assert_eq!(*first.resource(), 1);
        let stats = first.stats();
        assert_eq!(
            (stats.created, stats.reused, stats.active_buffers),
            (2, 0, 2)
        );
        assert_eq!(
            (
                stats.active_bytes,
                stats.peak_active_bytes,
                stats.idle_bytes
            ),
            (2 * CLASS, 2 * CLASS, 0)
        );

        drop(first);
        let reused = pool.take(32 << 10, CLASS, &mut allocate).unwrap();
        assert_eq!(*reused.resource(), 1);
        assert_eq!(*second.resource(), 2);
        drop(reused);
        drop(second);
        let stats = pool.0.lock().unwrap().1;
        assert_eq!(
            (
                stats.created,
                stats.reused,
                stats.active_buffers,
                stats.active_bytes
            ),
            (2, 1, 0, 0)
        );
        assert_eq!((stats.idle_bytes, stats.peak_idle_bytes), (CLASS, CLASS));
    }

    #[test]
    fn readback_pool_uses_exact_unshared_allocations_when_disabled_or_over_the_class_limit() {
        let pool = ReadbackPool::new();
        for (size, limit) in [
            (32 << 10, 0),
            (32 << 10, 16 << 10),
            ((16 << 20) + 1, 256 << 20),
        ] {
            let lease = pool.take(size, limit, Some).unwrap();
            assert_eq!(*lease.resource(), size);
        }
        let stats = pool.0.lock().unwrap().1;
        assert_eq!(
            (
                stats.created,
                stats.reused,
                stats.idle_bytes,
                stats.active_buffers
            ),
            (3, 0, 0, 0)
        );
    }

    #[test]
    fn readback_pool_tries_original_size_after_a_rounded_allocation_fails() {
        let pool = ReadbackPool::new();
        let mut attempts = Vec::new();
        let lease = pool
            .take(32 << 10, 256 << 20, |size| {
                attempts.push(size);
                (size == (32 << 10)).then_some(size)
            })
            .unwrap();
        assert_eq!(attempts, [64 << 10, 32 << 10]);
        assert_eq!(lease.stats().allocation_failures, 1);
        assert_eq!(*lease.resource(), 32 << 10);
        drop(lease);
        assert!(pool.take(32 << 10, 256 << 20, |_| None).is_none());
        let stats = pool.0.lock().unwrap().1;
        assert_eq!(
            (stats.created, stats.allocation_failures, stats.idle_bytes),
            (1, 3, 0)
        );
    }

    #[test]
    fn readback_pool_keeps_closed_recordings_exclusive_until_reset() {
        const ROW: u64 = 256;
        const CLASS: u64 = 64 << 10;
        let host = host().expect("D3D12 is needed for the readback recording test");
        // SAFETY: a private queue and fence on the same device as the test lists.
        let (queue, fence): (ID3D12CommandQueue, ID3D12Fence) = unsafe {
            (
                host.device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .unwrap(),
                host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
            )
        };
        let submit = |lists: &[Option<ID3D12CommandList>], target| {
            // SAFETY: lists are closed and their resources are kept through completion.
            let signal = unsafe {
                queue.ExecuteCommandLists(lists);
                queue.Signal(&fence, target)
            };
            if signal.is_err() {
                return false;
            }
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                // SAFETY: a plain query of the test's private fence.
                let completed = unsafe { fence.GetCompletedValue() };
                if completed == u64::MAX || std::time::Instant::now() >= end {
                    return false;
                }
                if completed >= target {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let take = || -> TextureReadback {
            host.texture_readbacks
                .take(ROW, CLASS, |bytes| host.read_back_buffer(bytes))
                .expect("a pooled readback buffer")
        };
        let record =
            |source: &ID3D12Resource, lease: TextureReadback, dst: &mut [u8; ROW as usize]| {
                let list = host.new_list().unwrap();
                // SAFETY: a full initialized upload row, an open list, and a
                // live readback resource in COPY_DEST.
                unsafe {
                    list.list
                        .CopyBufferRegion(lease.resource(), 0, source, 0, ROW)
                };
                list.deferred()
                    .texture_backs
                    .push(crate::texback::buffer_back_for_test(lease, dst));
                list.close();
                list
            };
        let upload = |value: u8| {
            let (buffer, data) = host.upload_buffer(ROW).expect("a mapped upload row");
            assert!(!data.is_null());
            // SAFETY: the upload row is mapped and has no GPU users yet.
            unsafe { data.write_bytes(value, ROW as usize) };
            (buffer, data)
        };
        let (source_a, data_a) = upload(0x1a);
        let mut dst_a = [0; ROW as usize];
        let lease_a = take();
        let address_a = lease_a.resource().as_raw();
        let list_a = record(&source_a, lease_a, &mut dst_a);
        if !submit(&[Some(list_a.list.cast().unwrap())], 1) {
            // Keep potential GPU uses alive on the failure path.
            std::mem::forget((list_a, source_a, queue.clone(), fence.clone()));
            panic!("first readback fence did not complete");
        }
        read_backs(&list_a);
        assert_eq!(&dst_a[..4], &[0x1a; 4]);

        // The first fence did not retire the closed recording. A second
        // list must get a different destination before both run together.
        let lease_b = take();
        assert_ne!(lease_b.resource().as_raw(), address_a);
        let (source_b, _) = upload(0x2b);
        let mut dst_b = [0; ROW as usize];
        let list_b = record(&source_b, lease_b, &mut dst_b);
        // SAFETY: the previous GPU read of source_a has finished.
        unsafe { data_a.write_bytes(0x3c, ROW as usize) };
        let lists = [
            Some(list_a.list.cast().unwrap()),
            Some(list_b.list.cast().unwrap()),
        ];
        if !submit(&lists, 2) {
            std::mem::forget((
                list_a,
                list_b,
                source_a,
                source_b,
                queue.clone(),
                fence.clone(),
            ));
            panic!("second readback fence did not complete");
        }
        drop(lists);
        read_backs(&list_a);
        read_backs(&list_b);
        assert_eq!(&dst_a[..4], &[0x3c; 4]);
        assert_eq!(&dst_b[..4], &[0x2b; 4]);

        list_a.reset();
        let reassigned = take();
        assert_eq!(reassigned.resource().as_raw(), address_a);
    }
}

mod compare_coalescing_tests {
    use super::*;

    fn shadow(start: u64) -> std::sync::Arc<crate::shadow::Shadow> {
        crate::shadow::Shadow::unbacked(start, std::ptr::null_mut(), 4 * 65536, false)
    }

    fn range(
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        from: usize,
        to: usize,
        picked: bool,
        compared: bool,
    ) -> CardRange {
        CardRange {
            shadow: shadow.clone(),
            declared: (from, to),
            span: shadow.card_run(from, to),
            at: u32::MAX,
            compared,
            picked,
        }
    }

    #[test]
    fn shared_piece_flags_preserve_declared_bytes_modes_and_copies() {
        let a = shadow(0x7a10_0000);
        let b = shadow(0x7b10_0000);
        let mut ranges = vec![
            range(&a, 64, 80, false, true),
            range(&a, 4096, 4112, false, true),
            range(&a, 65536 + 128, 65536 + 144, false, true),
            range(&a, 3 * 65536 + 64, 3 * 65536 + 80, false, true),
            range(&b, 64, 80, false, true),
            range(&a, 64, 80, true, true),
            range(&a, 2 * 65536, 2 * 65536 + 16, false, false),
        ];
        let before: Vec<_> = ranges.iter().map(|r| (r.declared, r.span)).collect();
        let (runs, end) = compact_card_compares(&mut ranges, 17);
        assert_eq!(runs.len(), 4);
        assert_eq!(end, 22);
        assert_eq!(ranges[0].at, ranges[1].at);
        assert_eq!(ranges[2].at, ranges[0].at + 1);
        assert_ne!(
            ranges[4].at, ranges[0].at,
            "different copies keep separate flags"
        );
        assert_ne!(
            ranges[5].at, ranges[0].at,
            "observations keep separate baseline semantics"
        );
        assert_eq!(
            ranges[6].at,
            u32::MAX,
            "uncompared ranges still use unconditional readback"
        );
        assert_eq!(
            ranges
                .iter()
                .map(|r| (r.declared, r.span))
                .collect::<Vec<_>>(),
            before
        );
        assert!(
            !runs
                .iter()
                .filter(|r| std::sync::Arc::ptr_eq(&r.shadow, &a) && !r.picked)
                .any(|r| r.span.first <= 2 && 2 < r.span.first + r.span.pieces),
            "no undeclared piece is added"
        );
        assert_eq!(compact_card_compares(&mut [], 29).1, 29);
    }

    /// The relay updates a checksum's baseline as it compares. Two byte
    /// ranges in one piece must both see its first changed flag, rather than
    /// the second range comparing against the baseline just updated by the first.
    #[test]
    fn relay_compare_shares_changed_flags_and_updates_the_baseline_once() {
        let host = host().expect("real D3D12 device required");
        let work = host.make_card_work().unwrap();
        let (source, data) = host.upload_buffer(65536).unwrap();
        let hashes = host.uav_buffer(256).unwrap();
        let readback = host.read_back_buffer(256).unwrap();
        // SAFETY: upload buffer mapped for the test's lifetime.
        unsafe {
            std::ptr::write_bytes(data, 0, 65536);
        }
        let a = shadow(1 << 16); // Synthetic piece-aligned address; never dereferenced.
        let mut ranges = vec![
            range(&a, 64, 80, false, true),
            range(&a, 4096, 4112, false, true),
        ];
        let (runs, end) = compact_card_compares(&mut ranges, 0);
        assert_eq!(end, 1);
        let execute = |mode: u32| {
            let list = host.new_list().unwrap();
            // SAFETY: live resources and an open list; the compiled root signature.
            unsafe {
                list.list.SetComputeRootSignature(&work.root);
                list.list.SetPipelineState(&work.pso);
                list.list
                    .SetComputeRootShaderResourceView(1, source.GetGPUVirtualAddress());
                list.list
                    .SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
                list.list
                    .SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                if mode == 0 {
                    list.list.ResourceBarrier(&[
                        transition(
                            &hashes,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        ),
                        transition(
                            &work.differs.0,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        ),
                    ]);
                }
                list.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                            pResource: std::mem::ManuallyDrop::new(None),
                        }),
                    },
                    ..Default::default()
                }]);
                for run in &runs {
                    card_pass(&list.list, &run.span, 65536, run.at, mode);
                }
                list.list.ResourceBarrier(&[transition(
                    &work.differs.0,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list
                    .CopyBufferRegion(&readback, 0, &work.differs.0, 0, 4);
                list.list.ResourceBarrier(&[transition(
                    &work.differs.0,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
                list.list.Close().unwrap();
            }
            host.execute_sync_internal(&list);
            let mut mapped = std::ptr::null_mut();
            // SAFETY: completed GPU readback.
            unsafe {
                readback
                    .Map(
                        0,
                        Some(&D3D12_RANGE { Begin: 0, End: 4 }),
                        Some(&mut mapped),
                    )
                    .unwrap();
                let flag = mapped.cast::<u32>().read();
                readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
                flag
            }
        };
        execute(0);
        // Both disjoint declared ranges are changed after the baseline ran.
        // SAFETY: both offsets are inside the upload mapping, and `execute`
        // waits for the GPU before returning.
        unsafe {
            data.add(68).write(0x5a);
            data.add(4100).write(0xa5);
        }
        assert!(torn::flagged(execute(2)));
        assert_eq!(ranges[0].at, ranges[1].at);
        assert!(
            !torn::flagged(execute(2)),
            "baseline was updated for the next submission"
        );
        // Every byte of a piece participates, including a byte outside the
        // declared ranges. Readback bounds remain the original byte ranges.
        // SAFETY: this offset is in the same live mapping; the GPU is done reading it.
        unsafe {
            data.add(60000).write(0x44);
        }
        assert!(torn::flagged(execute(2)));
        assert_eq!(ranges[0].declared, (64, 80));
        assert_eq!(ranges[1].declared, (4096, 4112));
        // SAFETY: querying the status of the live D3D12 device.
        unsafe {
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    #[test]
    #[ignore = "GPU timing benchmark; run explicitly without the game"]
    fn checksum_coalescing_cost() {
        let host = host().expect("real D3D12 device required");
        let work = host.make_card_work().unwrap();
        const SIZE: u32 = 64 << 20;
        const DECLARATIONS: usize = 4096;
        let (upload, data) = host.upload_buffer(u64::from(SIZE)).unwrap();
        let source = host.uav_buffer(u64::from(SIZE)).unwrap();
        let hashes = host.uav_buffer(u64::from(SIZE / 65536) * 16).unwrap();
        let back = host.read_back_buffer(256).unwrap();
        let mut stamps: Option<ID3D12QueryHeap> = None;
        // SAFETY: a valid timestamp heap description and writable output slot.
        unsafe {
            host.device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        ..Default::default()
                    },
                    &mut stamps,
                )
                .unwrap();
        }
        let stamps = stamps.unwrap();
        // SAFETY: the upload is mapped for SIZE bytes and not yet in GPU use.
        unsafe {
            std::ptr::write_bytes(data, 0x5a, SIZE as usize);
        }
        let fill = host.new_list().unwrap();
        // SAFETY: the open list copies SIZE bytes between live buffers of
        // that size, with the destination transitioned around the copy.
        unsafe {
            fill.list.ResourceBarrier(&[transition(
                &source,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            fill.list
                .CopyBufferRegion(&source, 0, &upload, 0, u64::from(SIZE));
            fill.list.ResourceBarrier(&[transition(
                &source,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            )]);
            fill.list.Close().unwrap();
        }
        host.execute_sync_internal(&fill);
        let a = crate::shadow::Shadow::unbacked(
            128 << 20, // Synthetic piece-aligned address; never dereferenced.
            std::ptr::null_mut(),
            u64::from(SIZE),
            false,
        );
        let mut ranges: Vec<_> = (0..DECLARATIONS)
            .map(|i| {
                let from = i / 4 * 65536 + i % 4 * 256;
                range(&a, from, from + 16, false, true)
            })
            .collect();
        let original: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(i, r)| CardCompareRun {
                shadow: a.clone(),
                span: r.span,
                at: i as u32,
                picked: false,
            })
            .collect();
        let (joined, _) = compact_card_compares(&mut ranges, 0);
        // SAFETY: querying the timestamp frequency of the live direct queue.
        let frequency = unsafe { host.queue.GetTimestampFrequency() }.unwrap();
        let measure = |runs: &[CardCompareRun], initialize: bool| {
            let list = host.new_list().unwrap();
            // SAFETY: the open list binds live buffers sized for the runs.
            // The private query heap has two slots and `back` can hold both timestamps.
            unsafe {
                if initialize {
                    list.list.ResourceBarrier(&[
                        transition(
                            &hashes,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        ),
                        transition(
                            &work.differs.0,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        ),
                    ]);
                }
                list.list.SetComputeRootSignature(&work.root);
                list.list.SetPipelineState(&work.pso);
                list.list
                    .SetComputeRootShaderResourceView(1, source.GetGPUVirtualAddress());
                list.list
                    .SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
                list.list
                    .SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                list.list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                for run in runs {
                    card_pass(&list.list, &run.span, SIZE, run.at, 1);
                }
                list.list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                list.list
                    .ResolveQueryData(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &back, 0);
                list.list.Close().unwrap();
            }
            host.execute_sync_internal(&list);
            let mut data = std::ptr::null_mut();
            // SAFETY: the GPU has finished resolving two u64 timestamps.
            // The mapped readback range holds both and is used before unmapping.
            unsafe {
                back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 16 }), Some(&mut data))
                    .unwrap();
                let ticks = std::slice::from_raw_parts(data.cast::<u64>(), 2);
                let elapsed = ticks[1].saturating_sub(ticks[0]) as f64 * 1000.0 / frequency as f64;
                back.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
                elapsed
            }
        };
        measure(&original, true);
        measure(&joined, false);
        let mut old = Vec::new();
        let mut new = Vec::new();
        for i in 0..9 {
            if i % 2 == 0 {
                old.push(measure(&original, false));
                new.push(measure(&joined, false));
            } else {
                new.push(measure(&joined, false));
                old.push(measure(&original, false));
            }
        }
        old.sort_by(f64::total_cmp);
        new.sort_by(f64::total_cmp);
        eprintln!(
            "synthetic 64 MiB GPU checksum benchmark: {} -> {} dispatches, {:.4} -> {:.4} ms (median); this is not a full frame measurement",
            original.len(),
            joined.len(),
            old[4],
            new[4]
        );
        // SAFETY: querying the status of the live D3D12 device.
        unsafe {
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }
}

mod relay_tests {
    use super::{flagged_pieces, landable};

    /// Two flights and what follows each: nothing of the second lands
    /// before the card has run it, and nothing behind a flight lands
    /// before the flight.
    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        // Flight (fence 5), its write, a signal, flight (fence 6), its write.
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        // Signals with no flight ahead land at once; behind one they wait.
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        // After the first three landed, the rest when its turn comes.
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    /// A piece flagged under one range of a plan is fetched under every
    /// range that holds a part of it; pieces of other copies are not.
    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        // Copy a: piece 3 under two ranges, flagged under the first only;
        // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
        let flags = [
            (a, 3, true),
            (a, 4, false),
            (a, 3, false),
            (b, 3, false),
            (b, 9, true),
        ];
        let fetch = flagged_pieces(flags.into_iter());
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }
}

mod late_tests {
    use super::{continues, join_ranges, touches};

    /// Copies join only when both ends continue and the whole does not
    /// overlap itself.
    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        // (destination, source, bytes)
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        // A gap, or only one side continues.
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        // Together they would overlap: the second reads what the first wrote.
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        // They touch without overlapping.
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    /// Ranges are found by what they hold, not by where they start, and a
    /// range that only touches another's end does not touch it.
    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = vec![(40, 50), (10, 20), (7, 7)];
        join_ranges(&mut ranges, [(20, 25), (45, 60)]);
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }

    #[test]
    fn a_reset_list_refills_the_same_title_range_without_replaying_old_work() {
        use windows::Win32::Graphics::Direct3D12::D3D12_RANGE;

        let host = super::host().expect("D3D12 is needed for command-list reset");
        let list = host.new_list().unwrap();
        let readback = host.read_back_buffer(16).unwrap();
        let mut source = [0u8; 16];
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        for (i, value) in [0x39, 0xd2, 0x72].into_iter().enumerate() {
            source.fill(value);
            let count = ran.clone();
            list.defer_host(move || {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            let (upload, _, _) = list.scratch_at(16).unwrap();
            let from = list.fill_scratch(source.as_ptr(), 16).unwrap();
            // Read what the GPU saw, not only the recording's deduplication set.
            // SAFETY: the list is open and both scratch allocations fit in the
            // first upload chunk. The readback holds all 16 bytes.
            unsafe {
                let offset = from.checked_sub(upload.GetGPUVirtualAddress()).unwrap();
                list.list
                    .CopyBufferRegion(&readback, 0, &upload, offset, 16);
            }
            list.close();
            host.execute_sync_internal(&list);
            let mut data = std::ptr::null_mut();
            // SAFETY: the copy has finished and the mapped range is 16 bytes.
            unsafe {
                readback
                    .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 16 }), Some(&mut data))
                    .unwrap();
                let found = data.cast::<[u8; 16]>().read();
                readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
                assert_eq!(found, [value; 16]);
            }
            assert_eq!(ran.load(std::sync::atomic::Ordering::Relaxed), i as u32 + 1);
            list.reset();
        }
    }

    #[test]
    fn joined_submissions_do_not_upload_before_texture_readback_lands() {
        let host = super::host().expect("D3D12 is needed for the submission dependency test");
        let writer = host.new_list().unwrap();
        let reader = host.new_list().unwrap();
        let mut title_pixels = [0; 256];
        let mut other_pixels = [0u8; 4];
        let back = host
            .texture_readbacks
            .take(256, 0, |bytes| host.read_back_buffer(bytes))
            .unwrap();
        writer
            .deferred()
            .texture_backs
            .push(crate::texback::buffer_back_for_test(
                back,
                &mut title_pixels,
            ));
        let upload = |pixels: &[u8]| {
            let at = pixels.as_ptr() as usize;
            reader
                .plane_upload(256, (at, 4), move |dst| {
                    // SAFETY: the source arrays outlive the preparations and
                    // the upload resource has room for these four bytes.
                    unsafe { std::ptr::copy_nonoverlapping(at as *const u8, dst as *mut u8, 4) };
                })
                .unwrap();
        };
        upload(&other_pixels);
        assert!(super::joinable([&writer].into_iter(), &[&reader]));
        // A CPU copy in the preceding part can change these bytes too.
        // The plane refresh must not be hoisted across it on a joined send.
        let copy = host.new_list().unwrap();
        let source = [0x42u8; 4];
        copy.defer_copy(other_pixels.as_mut_ptr(), source.as_ptr(), source.len());
        assert!(!super::joinable([&writer, &copy].into_iter(), &[&reader]));
        upload(&title_pixels);
        assert!(!super::joinable([&writer].into_iter(), &[&reader]));

        // A CPU-expanded primitive reads its vertices at preparation too.
        let rectangle = host.new_list().unwrap();
        let vertex = title_pixels.as_ptr() as usize;
        let away = other_pixels.as_ptr() as usize;
        let mut vertices = [0u8; 4];
        let to = vertices.as_mut_ptr() as usize;
        let expand = move |at| crate::host::copy_mapped(to as *mut u8, at as *const u8, 4, false);
        rectangle.defer_host_read(away, 4, move || expand(away));
        assert!(super::joinable([&writer].into_iter(), &[&rectangle]));
        rectangle.defer_host_read(vertex, 4, move || expand(vertex));
        assert!(!super::joinable([&writer].into_iter(), &[&rectangle]));
        assert_eq!(
            super::blocked_by(
                &[(vertex, vertex + title_pixels.len())],
                &[&rectangle],
                false
            ),
            Some(super::WHY_READ),
        );
    }
}

mod wide_refresh_tests {
    use super::wide_refresh_enabled;

    #[test]
    fn parallel_refresh_is_default_on_multicore_and_can_be_overridden() {
        assert!(wide_refresh_enabled(None, Some(4)));
        assert!(wide_refresh_enabled(None, Some(16)));
        assert!(!wide_refresh_enabled(None, Some(2)));
        assert!(!wide_refresh_enabled(None, None));
        assert!(wide_refresh_enabled(Some("1"), Some(1)));
        assert!(!wide_refresh_enabled(Some("0"), Some(16)));
        assert!(wide_refresh_enabled(Some("auto"), Some(4)));
    }
}

mod front_tests {
    use super::{Touch, fold_touches, front_spans, join_keys};

    /// Random submissions of lists (refreshes and shaders' writes of a few
    /// copies, new parts now and then): each list folded on its own and the
    /// folds joined in order give what the whole submission's touches give,
    /// span for span, in the same order (renderer port #6's planning).
    #[test]
    fn lists_folded_apart_and_joined_give_the_submissions_spans() {
        let mut seed = 0x9e37_79b9_u64;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        for _ in 0..3000 {
            let lists = 1 + next(8);
            let copies = 1 + next(6);
            let mut parts = Vec::new();
            let mut whole: Vec<Touch> = Vec::new();
            let mut folds = Vec::new();
            for n in 0..lists {
                if n > 0 && next(4) == 0 {
                    parts.push(n);
                    whole.push(Touch::Part);
                }
                let mut mine: Vec<Touch> = Vec::new();
                for _ in 0..next(12) {
                    let key = 0x1000 * (1 + next(copies));
                    if next(5) == 0 {
                        mine.push(Touch::Write(key));
                        whole.push(Touch::Write(key));
                    } else {
                        let from = next(100);
                        let to = from + 1 + next(50);
                        mine.push(Touch::Refresh(key, from, to));
                        whole.push(Touch::Refresh(key, from, to));
                    }
                }
                folds.push(fold_touches(mine.into_iter()));
            }
            let want = front_spans(whole.into_iter());
            let got = join_keys(folds.iter().map(|f| f.as_slice()), &parts);
            assert_eq!(got, want, "parts {parts:?}");
        }
    }
}

mod written_spans_tests {
    use super::{Touch, front_spans, split, written_spans};

    #[test]
    fn front_refresh_stops_at_title_writes_and_tracks_each_parts_gpu_writes() {
        use Touch::{Nothing, Part, Refresh, Title, Write};
        let found = front_spans(
            [
                Refresh(1, 10, 20),
                Refresh(1, 12, 25),
                Refresh(2, 0, 5),
                Nothing,
                Write(1),
                // (Around what the part's own shaders write.)
                Refresh(1, 0, 100),
                Write(3),
                Refresh(3, 1, 2),
                Refresh(4, 7, 9),
                Part,
                // The next part's bytes under the first one's writes.
                Refresh(1, 30, 40),
                Write(4),
                Refresh(4, 0, 9),
                Title,
                Refresh(5, 100, 200),
                Part,
                Refresh(1, 200, 300),
            ]
            .into_iter(),
        );
        assert_eq!(found, [(1, 10, 40), (2, 0, 5), (4, 7, 9)]);
    }

    #[test]
    fn a_submission_is_cut_between_its_lists() {
        // Lists c and e are in neither half's order: they go with the first.
        let Ok([first, second]) = split(vec!['a', 'b', 'c', 'd', 'e'], vec![1, 3, 0], 1) else {
            panic!("no list is named twice");
        };
        let run = |(lists, order): &(Vec<char>, Vec<usize>)| {
            order.iter().map(|&i| lists[i]).collect::<String>()
        };
        assert_eq!((run(&first), run(&second)), ("b".into(), "da".into()));
        assert_eq!(first.0.len() + second.0.len(), 5);
        assert!(split(vec!['a', 'b'], vec![0, 1, 0], 2).is_err());
        assert!(split(vec!['a', 'b'], vec![0, 1], 0).is_err());
    }

    #[test]
    fn what_is_written_is_refreshed_ahead_up_to_its_write_and_to_title_writes() {
        use Touch::{Nothing, Refresh, Title, Write};
        let mut found = written_spans(
            [
                Refresh(1, 10, 20),
                Refresh(2, 0, 5),
                Nothing,
                Refresh(1, 30, 40),
                Write(1),
                Refresh(1, 0, 100),
                Write(3),
                Refresh(3, 1, 2),
                Refresh(4, 7, 9),
                Write(4),
                Title,
                Write(2),
            ]
            .into_iter(),
        );
        found.sort_unstable();
        assert_eq!(found, [(1, 10, 40), (4, 7, 9)]);
    }
}

mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(
            !text.contains("thing 4 done"),
            "the oldest entries are dropped"
        );
    }
}

mod spare {
    use super::{SPARE_KEPT, SPARE_MOST, Spare, spare_limit};

    /// Unset, the limit is as it was: 1 GiB with `keep_freed` on a PC of
    /// more than 32 GB, 128 MiB otherwise. An asked limit wins.
    #[test]
    fn spare_limit_is_as_before_unless_asked() {
        assert_eq!(SPARE_KEPT, 1 << 30);
        assert_eq!(SPARE_MOST, 128 << 20);
        assert_eq!(spare_limit(None, true, false), 1 << 30);
        assert_eq!(spare_limit(None, true, true), 128 << 20);
        assert_eq!(spare_limit(None, false, false), 128 << 20);
        assert_eq!(spare_limit(None, false, true), 128 << 20);
        for (keep, limited) in [(true, false), (true, true), (false, false), (false, true)] {
            assert_eq!(spare_limit(Some(64), keep, limited), 64 << 20);
            assert_eq!(spare_limit(Some(0), keep, limited), 0);
        }
        assert_eq!(spare_limit(Some(u64::MAX), true, false), u64::MAX);
    }

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

mod refresh_tests {
    use super::Batch;
    use crate::shadow::Shadow;
    use std::sync::Arc;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE,
        VirtualAlloc,
    };

    const PAGE: usize = 64 << 10;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// Title memory for the tests (never given back: copies of it stay in
    /// the guards): write-watched, or a view of a section.
    fn title(len: usize, view: bool) -> usize {
        // SAFETY: plain allocations.
        let at = unsafe {
            if view {
                let section = CreateFileMappingW(
                    -1isize as _,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    len as u32,
                    std::ptr::null(),
                );
                let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
                // As the runtime tells of it.
                crate::title_memory_section(section as usize, len as u64, true);
                crate::title_memory_remapped(at, len);
                crate::title_memory_view(at, len, section as usize, 0);
                at
            } else {
                VirtualAlloc(
                    std::ptr::null(),
                    len,
                    MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                ) as usize
            }
        };
        assert_ne!(at, 0);
        at
    }

    /// Title memory of `blocks` pages in a placeholder, one view to a page,
    /// and every fourth page not there: as a title that streams has it.
    fn with_holes(blocks: usize) -> usize {
        use windows_sys::Win32::System::Memory::{
            MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, VirtualAlloc2, VirtualFree,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a placeholder and a section of our own.
        unsafe {
            let at = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                blocks * PAGE,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(at, 0);
            let len = blocks * PAGE;
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                (len >> 32) as u32,
                len as u32,
                std::ptr::null(),
            );
            for block in (0..blocks).filter(|b| b % 4 != 3) {
                let to = at + block * PAGE;
                VirtualFree(to as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
                let view = MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    to as *const _,
                    (block * PAGE) as u64,
                    PAGE,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(view.Value as usize, to);
                crate::title_memory_remapped(to, PAGE);
            }
            at
        }
    }

    fn memory(len: usize) -> *mut u8 {
        // SAFETY: a plain allocation.
        let data = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>()
        };
        assert!(!data.is_null(), "test memory allocation failed");
        data
    }

    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as usize % n
        }
    }

    /// Prepares as a submission does: one asking for all the ranges, the
    /// refreshes in several flushes, the asking's end.
    fn submission(ranges: &[(Arc<Shadow>, usize, usize)], flushes: usize) {
        // One at a time, as submissions are.
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let asked: Vec<(usize, usize)> = ranges
            .iter()
            .filter(|r| !r.0.unwatched())
            .map(|r| (r.1, r.1 + r.2))
            .collect();
        crate::watch::ask_once(&asked);
        let mut batch = Batch::default();
        for part in ranges.chunks(ranges.len().div_ceil(flushes).max(1)) {
            for (shadow, at, len) in part {
                batch.add(shadow, *at, *len);
            }
            batch.flush();
        }
        crate::watch::ask_once_end();
    }

    /// Whatever is written where, by the title or by us, and whichever
    /// copies (of both kinds of memory, over one another, none on a page
    /// line) are refreshed in which ranges: a refreshed range holds what
    /// title memory holds.
    #[test]
    fn refreshed_ranges_hold_title_memory() {
        let mut random = Random(0x1234_5678_9ABC_DEF1);
        const LEN: usize = 24 * PAGE;
        let regions = [title(LEN, false), title(LEN, true)];
        let mut copies = Vec::new();
        for &region in &regions {
            for (from, to) in [
                (0, LEN),
                (100, LEN - 5000),
                (3 * PAGE + 777, 9 * PAGE + 10),
                (8 * PAGE, 8 * PAGE + 300),
                (PAGE, 20 * PAGE),
            ] {
                let data = memory(to - from);
                copies.push((
                    Shadow::unbacked((region + from) as u64, data, (to - from) as u64, false),
                    data,
                ));
            }
        }
        // SAFETY: callers pass offsets within this test's mapped regions,
        // with no concurrent writes.
        let byte = |at: usize| unsafe { (at as *const u8).read_volatile() };
        for round in 0..600 {
            for _ in 0..random.below(4) {
                let at = regions[random.below(2)] + random.below(LEN - 64);
                let n = 1 + random.below(60);
                if random.below(3) == 0 {
                    // What shaders wrote.
                    let was: Vec<u8> = (0..n).map(|i| byte(at + i)).collect();
                    let now: Vec<u8> = was
                        .iter()
                        .map(|b| {
                            if random.below(2) == 0 {
                                *b
                            } else {
                                random.below(256) as u8
                            }
                        })
                        .collect();
                    Shadow::put(at, &was, &now);
                    assert!(
                        now.iter().enumerate().all(|(i, &b)| byte(at + i) == b),
                        "not put"
                    );
                    continue;
                }
                for i in 0..n {
                    // SAFETY: `at + i` is within a test region and this thread
                    // writes it before starting the refresh.
                    unsafe { ((at + i) as *mut u8).write_volatile(random.below(256) as u8) };
                }
            }
            let mut ranges = Vec::new();
            for (shadow, _) in &copies {
                if random.below(3) == 0 {
                    continue;
                }
                let (start, len) = shadow.span();
                let (start, len) = (start as usize, len as usize);
                // Often the range of the time before, as a list's is.
                let from = if random.below(2) == 0 {
                    0
                } else {
                    random.below(len)
                };
                let n = if random.below(2) == 0 {
                    len - from
                } else {
                    1 + random.below(len - from)
                };
                ranges.push((shadow.clone(), start + from, n));
            }
            if ranges.is_empty() {
                continue;
            }
            let flushes = 1 + random.below(3);
            submission(&ranges, flushes);
            for (shadow, at, n) in &ranges {
                let data = copies.iter().find(|c| Arc::ptr_eq(&c.0, shadow)).unwrap().1;
                let start = shadow.span().0 as usize;
                // SAFETY: each range fits both its title mapping and its copy,
                // and all refresh workers have finished writing.
                let same = unsafe {
                    std::slice::from_raw_parts(*at as *const u8, *n)
                        == std::slice::from_raw_parts(data.add(at - start), *n)
                };
                assert!(
                    same,
                    "round {round}: {at:#x}+{n:#x} of a copy at {start:#x}; first byte {}",
                    byte(*at)
                );
            }
        }
    }

    #[test]
    fn a_shader_write_does_not_hide_new_title_bytes_beside_it() {
        let size = 2 * PAGE;
        let title = title(size, false);
        let data = memory(size);
        let shadow = Shadow::unbacked(title as u64, data, size as u64, false);
        let mut batch = Batch::default();
        batch.refresh_spans(
            vec![(shadow.clone(), title, title + size)],
            Vec::new(),
            crate::timeline::span("refresh test"),
        );
        batch.flush_one(&shadow);
        shadow.writing(title, 1);
        crate::shadow::take_snapshots();
        // SAFETY: two committed pages on each side of this test's copy.
        unsafe {
            data.write(0x42);
            (title as *mut u8).add(PAGE).write(0x71);
        }
        crate::watch::wrote(title + PAGE, 1);
        batch.add(&shadow, title, size);
        batch.flush();
        // SAFETY: the refresh has returned and both locations are mapped.
        let bytes = unsafe { (data.read(), data.add(PAGE).read()) };
        assert_eq!(bytes, (0x42, 0x71));
    }

    /// A copy in the card's memory, with the card's side and the sends to it
    /// played by the test.
    struct Carded {
        shadow: Arc<Shadow>,
        data: usize,
        start: usize,
        len: usize,
        /// The last send, the last send every fetch to come is behind, and
        /// the card's bytes: the queue's order is the order it is locked in.
        card: std::sync::Mutex<(u64, u64, Vec<u8>)>,
        /// Whether a flight that writes it has not landed.
        flying: std::sync::atomic::AtomicBool,
        /// The dword of each eight its shaders write.
        class: usize,
    }

    impl Carded {
        fn new(start: usize, len: usize, class: usize) -> Carded {
            let data = memory(len);
            Carded {
                shadow: Shadow::unbacked(start as u64, data, len as u64, true),
                data: data as usize,
                start,
                len,
                card: std::sync::Mutex::new((0, 0, vec![0; len])),
                flying: std::sync::atomic::AtomicBool::new(false),
                class,
            }
        }

        /// What changed on the CPU's side goes up.
        fn send(&self) {
            let mut card = self.card.lock().unwrap();
            card.0 += 1;
            let (number, merged) = (card.0, card.1);
            for (a, b) in self.shadow.send_up(number, merged) {
                // SAFETY: the copy's bytes; nothing writes those that are to
                // go up.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (self.data + a) as *const u8,
                        card.2[a..b].as_mut_ptr(),
                        b - a,
                    )
                };
            }
        }
    }

    /// Everything at once: the title writes its memory on two threads, the
    /// submitting thread puts its bytes in under shader writes
    /// (`Shadow::merge_under_writes`), refreshes copies that overlap (two in
    /// the card's memory, one not), sends up what changed and declares what
    /// the next flight's shaders write, and what flights wrote is landed
    /// meanwhile (`crate::land`, beside the guards). When all of it stands
    /// still and the copies are refreshed once more, each of them, and the
    /// card's side of those that have one, hold what title memory holds, and
    /// title memory what was written last.
    ///
    /// Into the first copy shaders write besides through a view nobody
    /// declared, words the title does not write: nothing that goes up ever
    /// puts an older value over those.
    ///
    /// (It fails in the first rounds if `landed_beside` notes nothing, or
    /// only takes the block out of the guards' clean ones and leaves its bit,
    /// or if a fault leaves the bit; in some thousand rounds,
    /// `VICTORY_TEST_ROUNDS`, if what is landed is noted before it is in.)
    #[test]
    fn landings_title_writes_and_light_refreshes_at_once() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        assert!(crate::safecopy::on());
        const LEN: usize = 24 * PAGE;
        let rounds: usize = std::env::var("VICTORY_TEST_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let region = title(LEN, true);
        // SAFETY: the view is page-aligned and live for all scoped threads.
        // Offsets name aligned dwords; title writers use atomics on their own
        // words, and the landing thread reads its words after updating them.
        let dword = |at: usize| unsafe { &*((region + at) as *const AtomicU32) };
        let carded = [
            Carded::new(region, LEN, 3),
            Carded::new(region + 4 * PAGE + 1000, LEN - 6 * PAGE - 4, 5),
        ];
        let plain = {
            let (start, len) = (region + 700, LEN - 9000);
            let data = memory(len);
            (
                Shadow::unbacked(start as u64, data, len as u64, false),
                start,
                len,
                data as usize,
            )
        };
        // What was last written so, by offset.
        let mut undeclared = std::collections::HashMap::new();
        let exact = std::env::var_os("VICTORY_SEND_CHANGED").is_some_and(|v| v != "0");
        let stop = AtomicBool::new(false);
        // Also when an assertion fails on one of the threads.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        // Held shared by who writes title memory, alone while it is looked at.
        let still = std::sync::RwLock::new(());
        let (fly, flights) = std::sync::mpsc::channel::<(usize, Vec<(usize, usize)>, u32)>();
        std::thread::scope(|s| {
            for t in 0..2usize {
                let (stop, still) = (&stop, &still);
                s.spawn(move || {
                    let mut random = Random(0x7777_1234_0000_0001 + t as u64 * 0x1111);
                    let mut value = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        {
                            let _moving = still.read().unwrap();
                            for _ in 0..3 {
                                value += 1;
                                // Of each eight dwords, one a thread.
                                let at = random.below(LEN / 32) * 32 + 4 * (1 + t);
                                dword(at).store(value, Ordering::Relaxed);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_micros(20));
                    }
                });
            }
            // Lands flights.
            {
                let (carded, stop) = (&carded, &stop);
                s.spawn(move || {
                    let _stop = Stop(stop);
                    while let Ok((which, ranges, value)) = flights.recv() {
                        let c = &carded[which];
                        // Fetched: the card's bytes, behind every send so far.
                        let (after, now) = {
                            let card = c.card.lock().unwrap();
                            (card.0, card.2.clone())
                        };
                        // By piece: 64 KiB of title memory.
                        let mut pieces = Vec::new();
                        for &(from, to) in &ranges {
                            let mut at = from;
                            while at < to {
                                let next = ((c.start + at) / PAGE * PAGE + PAGE - c.start).min(to);
                                pieces.push((at, next, now.as_ptr() as usize + at));
                                at = next;
                            }
                        }
                        let parts: Vec<_> = pieces.chunks(3).collect();
                        crate::land::each(parts.len(), &|i| {
                            let mut notes = crate::land::Notes::default();
                            // SAFETY: the bytes of `now`; each piece once.
                            unsafe { c.shadow.land_card(parts[i], after, &mut notes) };
                            notes.flush();
                        });
                        for &(from, to) in &ranges {
                            for at in
                                (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class)
                            {
                                // (The next fetch may have brought more.)
                                assert!(
                                    dword(c.start - region + at).load(Ordering::Relaxed) >= value,
                                    "not landed"
                                );
                            }
                        }
                        let done: Vec<_> = ranges
                            .iter()
                            .map(|&(a, b)| (c.start + a, c.start + b))
                            .collect();
                        c.shadow.copy_back_done(&done);
                        c.card.lock().unwrap().1 = after;
                        c.flying.store(false, Ordering::Release);
                    }
                });
            }
            let _stop = Stop(&stop);
            let mut random = Random(0xABCD_EF01_2345_6789);
            let check = |round: usize, random: &mut Random, quiet: bool| {
                for c in &carded {
                    if c.shadow.writes_pending() {
                        let mut runs = usize::MAX;
                        assert!(
                            !c.shadow.merge_under_writes(c.start, c.len, &mut runs),
                            "round {round}"
                        );
                    }
                }
                let mut ranges = Vec::new();
                for (shadow, start, len) in carded
                    .iter()
                    .map(|c| (&c.shadow, c.start, c.len))
                    .chain([(&plain.0, plain.1, plain.2)])
                {
                    // All of it, or a part; often the same part.
                    let (from, n) = match random.below(3) {
                        _ if quiet => (0, len),
                        0 => (0, len),
                        1 => (len / 3 / 4 * 4, len / 2 / 4 * 4),
                        _ => {
                            let from = random.below(len / 4) * 4;
                            (
                                from,
                                (4 + random.below((len - from) / 4) * 4).min(len - from),
                            )
                        }
                    };
                    ranges.push((shadow.clone(), start + from, n));
                }
                submission(&ranges, 1 + random.below(3));
                carded.iter().for_each(Carded::send);
            };
            let mut value = 1u32;
            for round in 0..rounds {
                check(round, &mut random, false);
                if exact {
                    let c = &carded[0];
                    let mut card = c.card.lock().unwrap();
                    for _ in 0..40 {
                        value += 1;
                        let at = random.below(LEN / 32) * 32 + 4 * 6;
                        card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                        undeclared.insert(at, value);
                    }
                }
                // The flight of this submission: its shaders' writes.
                for (which, c) in carded.iter().enumerate() {
                    if c.flying.swap(true, Ordering::Acquire) {
                        continue;
                    }
                    value += 1;
                    let mut ranges: Vec<(usize, usize)> = Vec::new();
                    let first = random.below(6) * PAGE / 2;
                    for k in 0..2 {
                        let from = first + k * 9 * PAGE + 4 * random.below(9000);
                        let to = (from + 4 * (1 + random.below(7 * PAGE / 4))).min(c.len);
                        if from < to {
                            ranges.push((from, to));
                        }
                    }
                    for &(from, to) in &ranges {
                        c.shadow.writing(c.start + from, to - from);
                    }
                    {
                        let mut card = c.card.lock().unwrap();
                        for &(from, to) in &ranges {
                            for at in
                                (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class)
                            {
                                card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                            }
                        }
                    }
                    fly.send((which, ranges, value)).unwrap();
                }
                if round % 20 != 19 {
                    continue;
                }
                // Still: no flight in the air, no title write.
                let _still = still.write().unwrap();
                while carded.iter().any(|c| c.flying.load(Ordering::Acquire)) {
                    assert!(!stop.load(Ordering::Relaxed), "the landing thread failed");
                    std::thread::yield_now();
                }
                check(round, &mut random, true);
                // SAFETY: the test's own memory, written by nobody now.
                let bytes = |at: usize, len: usize| unsafe {
                    std::slice::from_raw_parts(at as *const u8, len)
                };
                let first = |a: &[u8], b: &[u8]| a.iter().zip(b).position(|(x, y)| x != y);
                for c in &carded {
                    let (held, there) = (bytes(c.data, c.len), bytes(c.start, c.len));
                    assert_eq!(
                        first(held, there),
                        None,
                        "round {round}: a card copy's CPU side (class {})",
                        c.class
                    );
                    let card = c.card.lock().unwrap();
                    let mut side = card.2.clone();
                    if c.class == carded[0].class {
                        // (They may have been landed meanwhile, by a flight
                        // that declared their range, and may not.)
                        for (&at, &value) in &undeclared {
                            let on_card = u32::from_le_bytes(side[at..at + 4].try_into().unwrap());
                            assert_eq!(on_card, value, "round {round}: an older value at {at:#x}");
                            side[at..at + 4].copy_from_slice(&there[at..at + 4]);
                        }
                    }
                    assert_eq!(
                        first(&side, there),
                        None,
                        "round {round}: a card copy's card side (class {})",
                        c.class
                    );
                }
                assert_eq!(
                    first(bytes(plain.3, plain.2), bytes(plain.1, plain.2)),
                    None,
                    "round {round}: the other copy"
                );
            }
            drop(fly);
        });
    }

    /// Measure putting title bytes under overlapping shader writes in a
    /// synthetic copy. Run with `--ignored --nocapture`, and with
    /// `VICTORY_LIGHT_REFRESH=0` for the way it was.
    #[test]
    #[ignore]
    fn bytes_under_overlapping_ranges() {
        const BIG: usize = 192 << 20;
        const STRIDE: usize = PAGE / 8 - 32;
        let big = title(BIG, true);
        // SAFETY: `big` is a writable mapping of BIG bytes owned by the test.
        unsafe { std::ptr::write_bytes(big as *mut u8, 1, BIG) };
        let copy = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
        copy.update(big, BIG);
        copy.take_dirty();
        for i in 0..48 {
            copy.writing(big + i * STRIDE, BIG - i * STRIDE);
        }
        let mut best = f64::MAX;
        for round in 0..30 {
            for i in 0..16 {
                let offset = (i * 211 + round) % (BIG / PAGE) * PAGE + 64;
                // SAFETY: `offset` selects a byte within the live test mapping;
                // no refresh runs until these writes are done.
                unsafe { (big as *mut u8).add(offset).write_volatile(round as u8) };
            }
            let started = std::time::Instant::now();
            let mut runs = usize::MAX;
            assert!(!copy.merge_under_writes(big, BIG, &mut runs));
            best = best.min(started.elapsed().as_secs_f64() * 1e3);
            copy.take_dirty();
        }
        println!(
            "VICTORY_LIGHT_REFRESH {}: best {best:.3} ms",
            crate::watch::light()
        );
    }

    /// A range nothing wrote is known to be as it was, at its ends too, and
    /// is so no more once something wrote it.
    #[test]
    fn unchanged_ranges_are_known() {
        for view in [false, true] {
            let region = title(8 * PAGE, view);
            // (A page read for the first time counts as written.)
            // SAFETY: the test's own memory.
            unsafe { std::ptr::write_bytes(region as *mut u8, 1, 8 * PAGE) };
            let data = memory(8 * PAGE);
            let copy = Shadow::unbacked(region as u64, data, (8 * PAGE) as u64, false);
            // Ends in the middle of pages.
            let (at, len) = (region + 500, 5 * PAGE);
            let look = |expected: bool, what: &str| {
                // The submission's one asking, which takes two ranges. (Again
                // if a submission of another test ended it.)
                let mut known = false;
                for _ in 0..100 {
                    crate::watch::ask_once(&[
                        (at, at + len),
                        (region + 7 * PAGE, region + 8 * PAGE),
                    ]);
                    known = copy.nothing_new(at, len);
                    if known || !expected {
                        break;
                    }
                }
                assert_eq!(known, expected, "view {view}: {what}");
                let copied = copy.update(at, len);
                crate::watch::ask_once_end();
                copied
            };
            assert_eq!(look(false, "never taken"), len);
            assert_eq!(look(true, "taken"), 0);
            assert_eq!(look(true, "taken"), 0);
            // SAFETY: the test's own memory.
            unsafe { ((region + 600) as *mut u8).write_volatile(1) };
            // (The kernel names the 4 KiB, the guards only the block.)
            assert_eq!(
                look(false, "written at its end"),
                if view { PAGE } else { 4096 } - 500
            );
            assert_eq!(look(true, "taken again"), 0);
            // Outside the range, on its last page.
            // SAFETY: as above.
            unsafe { ((region + 5 * PAGE + 600) as *mut u8).write_volatile(1) };
            assert_eq!(look(false, "written on its last page"), 500);
            // A longer range was never taken whole.
            assert!(!copy.nothing_new(region, 6 * PAGE));
        }
    }

    /// Of a copy in the card's memory only the dwords title memory changed go
    /// up, and none beside them, not even between two in 64 bytes; a page
    /// never taken goes up whole.
    #[test]
    fn only_what_differs_goes_up() {
        let region = title(4 * PAGE, true);
        let data = memory(4 * PAGE);
        let copy = Shadow::unbacked(region as u64, data, (4 * PAGE) as u64, true);
        copy.update(region, 4 * PAGE);
        assert_eq!(
            copy.take_dirty(),
            [(0, 4 * PAGE)],
            "all of it, though zeros are no change"
        );
        // What the relay does: shader writes into title memory, and into the
        // CPU's side.
        crate::watch::landing(region + PAGE + 640, 64);
        // SAFETY: the test's own memory.
        unsafe {
            std::ptr::write_bytes((region + PAGE + 640) as *mut u8, 7, 64);
            std::ptr::write_bytes(data.add(PAGE + 640), 7, 64);
        }
        copy.update(region, 4 * PAGE);
        let again = copy.take_dirty();
        // SAFETY: as above.
        unsafe {
            ((region + 2 * PAGE + 129) as *mut u8).write_volatile(9);
            ((region + 2 * PAGE + 136) as *mut u32).write_volatile(9);
            ((region + 2 * PAGE + 140) as *mut u32).write_volatile(9);
            // The value that was there.
            ((region + 2 * PAGE + 132) as *mut u32).write_volatile(0);
        }
        copy.update(region, 4 * PAGE);
        let written = copy.take_dirty();
        if std::env::var_os("VICTORY_SEND_CHANGED").is_some_and(|v| v != "0") {
            assert_eq!(again, []);
            assert_eq!(
                written,
                [
                    (2 * PAGE + 128, 2 * PAGE + 132),
                    (2 * PAGE + 136, 2 * PAGE + 144)
                ]
            );
        } else {
            assert_eq!(again, [(PAGE, 2 * PAGE)]);
            assert_eq!(written, [(2 * PAGE, 3 * PAGE)]);
        }
        // SAFETY: as above.
        assert_eq!(unsafe { data.add(2 * PAGE + 129).read() }, 9);
    }

    /// The refreshes of a frame as run record-1 had them: 30 submissions, the
    /// first with a copy for each of 4,400 ranges, the others with 60 and a
    /// big copy that 55 landed pieces and 9 title writes opened, while a
    /// thread lands what shaders wrote. Run with `--ignored --nocapture`,
    /// and with `VICTORY_LIGHT_REFRESH=0` for the way it was.
    #[test]
    #[ignore]
    fn frame_of_refreshes() {
        let mut random = Random(0x9876_5432_1234_5671);
        const EACH: usize = 224 << 10;
        const BIG: usize = 174 << 20;
        let watched = title(3000 * EACH, false);
        // `VICTORY_BENCH_HOLES=0`: views without holes.
        let holes = std::env::var_os("VICTORY_BENCH_HOLES").is_none_or(|v| v != "0");
        let views = if holes {
            with_holes(1400 * EACH / PAGE)
        } else {
            title(1400 * EACH, true)
        };
        let there = |at: usize| !holes || (at - views) / PAGE % 4 != 3;
        let big = title(BIG, true);
        // SAFETY: the test's own memory.
        unsafe {
            std::ptr::write_bytes(watched as *mut u8, 1, 3000 * EACH);
            for at in (views..views + 1400 * EACH)
                .step_by(PAGE)
                .filter(|&at| there(at))
            {
                std::ptr::write_bytes(at as *mut u8, 1, PAGE);
            }
            std::ptr::write_bytes(big as *mut u8, 1, BIG);
        }
        let mut small = Vec::new();
        for i in 0..4400 {
            let (start, at) = if i < 3000 {
                (watched, i * EACH)
            } else {
                (views, (i - 3000) * EACH)
            };
            let data = memory(EACH);
            // The range views read, not all of the copy.
            small.push((
                Shadow::unbacked((start + at) as u64, data, EACH as u64, false),
                start + at + 4000,
                EACH - 9000,
            ));
        }
        let heap = (
            Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true),
            big,
            BIG,
        );
        // The same memory, as another copy: what is landed is landed through
        // it, and the refreshes are of the first.
        let lands = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
        let (mut best, mut asking) = ([f64::MAX; 2], [0f64; 2]);
        let (mut sum, mut landing) = (0.0, 0.0);
        const FRAMES: usize = 12;
        for frame in 0..FRAMES {
            let mut took = [0f64; 2];
            for i in 0..30 {
                // The title's writes, and the relay's beside the submission.
                for _ in 0..9 {
                    let at = views + random.below(1400 * EACH / 8) * 8;
                    if there(at) {
                        // SAFETY: the test's own memory.
                        unsafe { (at as *mut u64).write_volatile(frame as u64) };
                    }
                }
                for _ in 0..60 {
                    let at = watched + random.below(3000 * EACH / 8) * 8;
                    // SAFETY: the test's own memory.
                    unsafe { (at as *mut u64).write_volatile(frame as u64) };
                }
                let first = random.below(BIG / PAGE - 60);
                let lands = lands.clone();
                let land = std::thread::spawn(move || {
                    let now = vec![frame as u8 ^ i as u8 ^ 0x55; PAGE];
                    let started = std::time::Instant::now();
                    for pieces in (first..first + 55).collect::<Vec<_>>().chunks(4) {
                        let pieces: Vec<_> = pieces
                            .iter()
                            .map(|p| (p * PAGE + 8, (p + 1) * PAGE - 8, now.as_ptr() as usize))
                            .collect();
                        let mut notes = crate::land::Notes::default();
                        // SAFETY: the bytes of `now`; nothing else lands here.
                        unsafe { lands.land_card(&pieces, 0, &mut notes) };
                        notes.flush();
                    }
                    started.elapsed().as_secs_f64() * 1e3
                });
                let ranges: Vec<(Arc<Shadow>, usize, usize)> = if i == 0 {
                    small.iter().cloned().chain([heap.clone()]).collect()
                } else {
                    (0..60)
                        .map(|_| small[random.below(4400)].clone())
                        .chain([heap.clone()])
                        .collect()
                };
                let started = std::time::Instant::now();
                submission(&ranges, 3);
                took[usize::from(i != 0)] += started.elapsed().as_secs_f64() * 1e3;
                let put = land.join().unwrap();
                if frame >= 2 {
                    landing += put;
                }
                heap.0.take_dirty();
            }
            if frame >= 2 {
                sum += took[0] + took[1];
                for k in 0..2 {
                    best[k] = best[k].min(took[k]);
                    asking[k] += took[k];
                }
            }
        }
        println!(
            "VICTORY_LIGHT_REFRESH {}: a frame's refreshes {:.2} ms in the mean; the submission of all copies {:.2} (best {:.2}), the 29 \
             others {:.2} (best {:.2}); the landing thread's puts {:.2}",
            crate::watch::light(),
            sum / (FRAMES - 2) as f64,
            asking[0] / (FRAMES - 2) as f64,
            best[0],
            asking[1] / (FRAMES - 2) as f64,
            best[1],
            landing / (FRAMES - 2) as f64,
        );
        #[cfg(feature = "diagnostics")]
        println!(
            "per frame, as `crate::watch::asks`: {:?}",
            crate::watch::asks().map(|n| n / FRAMES as u64)
        );
    }
}

mod batch {
    // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        let vram_idle_ms = |memory| super::vram_idle_for(memory, false);
        let keeps = |memory| super::vram_idle_for(memory, true);
        let gib = 1024 * 1024 * 1024;
        assert_eq!(keeps(Some([100, 67, 0, 0])), 600_000);
        assert_eq!(keeps(Some([100, 68, 0, 0])), 30_000);
        assert_eq!(keeps(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(keeps(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(keeps(None), 5_000);
        // Normal (< 75%)
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        // Moderate (75%..85%)
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        // High (>= 85%, including over-budget)
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        // Missing or zero budget falls back to normal
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn textures_are_kept_while_the_card_has_room() {
        use super::vram_keep;
        let gib = 1 << 30;
        assert_eq!(vram_keep(100 * gib, 87 * gib).0, 600_000);
        // Past it: the longest idle ones, enough to come down to 85 %.
        let (ms, bytes) = vram_keep(100 * gib, 90 * gib);
        assert_eq!((ms, bytes), (5_000, 5 * gib));
        assert_eq!(vram_keep(100 * gib, 120 * gib), (1_500, 35 * gib));
        assert_eq!(vram_keep(0, 7).0, 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        let ram_idle_ms = |avail| super::ram_idle_for(avail, false);
        assert_eq!(super::ram_idle_for(Some(12 << 30), true), 300_000);
        assert_eq!(super::ram_idle_for(Some((12 << 30) - 1), true), 10_000);
        assert_eq!(super::ram_idle_for(None, true), 10_000);
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }
}
