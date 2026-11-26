//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! [`crate::hlsl`], with its root signature inside).
//! Non-indexed rectangle-list draws follow the clamp with GPU-side
//! rectangle expansion in [`crate::rectangles`].
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either. The budget word is reserved with a saturating compare-and-exchange
//! (modelled and tested in `indirect_budget`), so rejected records never
//! wrap it and reopen the budget.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};

const SOURCE: &str = include_str!("../shaders/argument_clamp.hlsl");

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    // Wine's d3dcompiler_47 refuses it; `compile_compute` then uses DXC.
    let bytes = crate::hlsl::compile_compute("argument clamp", SOURCE)?;
    let bytes = &bytes[..];
    // SAFETY: plain object creation; the root signature is inside the shader.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu`, leaving
/// the buffer in unordered-access state with a UAV barrier after the write
/// (for passes such as [`crate::rectangles`] that read the clamped records
/// in another compute shader). Replaces the list's compute root signature
/// and pipeline (callers bind theirs after). False if the clamp is not
/// available.
pub(crate) fn clamp_uav(
    list: &HostList,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
) -> bool {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    if !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
        return false;
    }
    let l = &list.list;
    let constants = [kind, stride, records, 0];
    // SAFETY: an open list; our root signature, pipeline and a live buffer
    // the caller's list holds.
    unsafe {
        // Earlier shader writes (the title's argument passes) first.
        l.ResourceBarrier(&[uav_barrier()]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        l.Dispatch(records.div_ceil(64), 1, 1);
        l.ResourceBarrier(&[uav_barrier()]);
    }
    true
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu` in
/// `buffer`, and leaves the buffer ready to be read as arguments. Replaces
/// the list's compute root signature and pipeline (callers bind theirs
/// after). False if the clamp is not available.
pub(crate) fn bound(
    list: &HostList,
    buffer: &ID3D12Resource,
    gpu: u64,
    spec: (u32, u32, u32),
) -> bool {
    if !clamp_uav(list, gpu, spec) {
        return false;
    }
    list.use_shadow(buffer, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    true
}

pub(crate) fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    #[test]
    fn unfilled_mesh_records_leave_the_budget_for_later_work() {
        use super::*;

        let Some(host) = host() else {
            return; // needs host D3D12 compute and buffer readback
        };
        const UNFILLED: usize = 16;
        const MESH: u32 = 10;
        const STRIDE: u32 = 12;
        let mut input = [[u32::MAX; 3]; UNFILLED + 2];
        // A non-sentinel oversized record still takes the ordinary bounds.
        input[UNFILLED] = [u32::MAX, 2, 3];
        input[UNFILLED + 1] = [17, 1, 1];
        let mut expected = [[0; 3]; UNFILLED + 2];
        expected[UNFILLED] = [65_535, 1, 1];
        expected[UNFILLED + 1] = input[UNFILLED + 1];
        let size = size_of_val(&input) as u64;

        for records_per_clamp in [1, UNFILLED] {
            let args = host.default_buffer(size).expect("argument buffer");
            let list = host.new_list().expect("command list");
            let (upload, mapped) = list.upload(host, size).expect("upload buffer");
            list.use_shadow(&args, D3D12_RESOURCE_STATE_COPY_DEST);
            // SAFETY: mapped upload of the correct size and live copy buffers on an open list.
            let gpu = unsafe {
                std::ptr::copy_nonoverlapping(
                    input.as_ptr().cast::<u8>(),
                    mapped,
                    size_of_val(&input),
                );
                list.list.CopyBufferRegion(&args, 0, &upload, 0, size);
                args.GetGPUVirtualAddress()
            };

            let clamp_and_read = |list: &HostList| {
                let clamp_at = |offset: usize, records: usize| {
                    list.use_shadow(&args, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
                    assert!(bound(
                        list,
                        &args,
                        gpu + offset as u64 * u64::from(STRIDE),
                        (MESH, STRIDE, records as u32),
                    ));
                };
                for offset in (0..UNFILLED).step_by(records_per_clamp) {
                    clamp_at(offset, records_per_clamp);
                }
                clamp_at(UNFILLED, 1);
                clamp_at(UNFILLED + 1, 1);

                let (readback, mapped) = list.readback(host, size).expect("readback buffer");
                list.use_shadow(&args, D3D12_RESOURCE_STATE_COPY_SOURCE);
                // SAFETY: the buffers have compatible states and room for the copy.
                unsafe { list.list.CopyBufferRegion(&readback, 0, &args, 0, size) };
                list.close();
                host.execute(&[list]);
                let mut actual = [[0; 3]; UNFILLED + 2];
                // SAFETY: execution finished and the mapped readback received the whole buffer.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        mapped,
                        actual.as_mut_ptr().cast::<u8>(),
                        size_of_val(&actual),
                    );
                }
                actual
            };
            assert_eq!(clamp_and_read(&list), expected);

            // No re-upload: the next list reads and bounds the same host shadow bytes.
            let replay = host.new_list().expect("replay command list");
            assert_eq!(clamp_and_read(&replay), expected);
        }
    }
}
