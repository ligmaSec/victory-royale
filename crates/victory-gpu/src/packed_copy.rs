//! GPU copies between native packed color and its RGBA8 backing.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Error;

use crate::host::{HostList, host, transition};

const SOURCE: &str = include_str!("../shaders/packed_copy.hlsl");

struct Converter {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 pipeline and root-signature objects are free-threaded.
unsafe impl Send for Converter {}
// SAFETY: as above.
unsafe impl Sync for Converter {}

fn converter(device: &ID3D12Device) -> Option<&'static Converter> {
    static CONVERTER: OnceLock<Option<Converter>> = OnceLock::new();
    CONVERTER
        .get_or_init(|| match make_converter(device) {
            Ok(converter) => Some(converter),
            Err(error) => {
                tracing::warn!(%error, "packed texture copy pipeline creation failed");
                None
            }
        })
        .as_ref()
}

fn make_converter(device: &ID3D12Device) -> windows::core::Result<Converter> {
    let code = crate::hlsl::compile_compute("packed texture copy", SOURCE)?;
    let bytes = code.as_slice();
    // SAFETY: the slice holds the shader bytecode and its embedded root signature.
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
        Ok(Converter { root, pso })
    }
}

fn footprints(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    count: u32,
) -> Option<(Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>, u64)> {
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut bytes = 0;
    // SAFETY: the output array has room for all color subresources.
    unsafe {
        device.GetCopyableFootprints(
            desc,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            None,
            None,
            Some(&mut bytes),
        );
    }
    // The final odd-width packed row still needs a complete dword. An invalid
    // description returns UINT64_MAX and fails the same addition.
    let bytes = bytes.checked_add(3)? & !3;
    Some((footprints, bytes))
}

fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut result = None;
    // SAFETY: a committed default buffer with the requested state and flags.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut result,
        )?;
    }
    result.ok_or_else(Error::empty)
}

fn texture_location(texture: &ID3D12Resource, subresource: u32) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: subresource,
        },
    }
}

fn buffer_location(
    buffer: &ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: footprint,
        },
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Region {
    pub src_subresource: u32,
    pub dst_subresource: u32,
    pub offset: [u32; 3],
    pub source_box: Option<D3D12_BOX>,
}

struct Surface<'a> {
    texture: &'a ID3D12Resource,
    first_subresource: u32,
    layouts: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    bytes: u64,
}

impl<'a> Surface<'a> {
    fn new(
        device: &ID3D12Device,
        texture: &'a ID3D12Resource,
        desc: &D3D12_RESOURCE_DESC,
        first_subresource: u32,
        count: u32,
    ) -> Option<Self> {
        let (layouts, bytes) = footprints(device, desc, count)?;
        Some(Self {
            texture,
            first_subresource,
            layouts,
            bytes,
        })
    }
}

fn is_texture(desc: &D3D12_RESOURCE_DESC) -> bool {
    matches!(
        desc.Dimension,
        D3D12_RESOURCE_DIMENSION_TEXTURE1D
            | D3D12_RESOURCE_DIMENSION_TEXTURE2D
            | D3D12_RESOURCE_DIMENSION_TEXTURE3D
    )
}

fn subresource_count(desc: &D3D12_RESOURCE_DESC) -> u32 {
    let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(desc.DepthOrArraySize)
    };
    u32::from(desc.MipLevels) * layers
}

/// Extent of a physical color subresource, in texels rather than bytes.
pub(crate) fn subresource_extent(desc: &D3D12_RESOURCE_DESC, index: u32) -> Option<[u32; 3]> {
    if !is_texture(desc) || index >= subresource_count(desc) {
        return None;
    }
    let mip = index % u32::from(desc.MipLevels);
    let depth = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(desc.DepthOrArraySize)
    } else {
        1
    };
    Some([
        ((desc.Width as u32) >> mip).max(1),
        (desc.Height >> mip).max(1),
        (depth >> mip).max(1),
    ])
}

fn descriptions(
    to: &ID3D12Resource,
    from: &ID3D12Resource,
    logical: u32,
) -> Option<(D3D12_RESOURCE_DESC, D3D12_RESOURCE_DESC, u32)> {
    if !matches!(logical, 86 | 115) {
        return None;
    }
    // SAFETY: both resource interfaces are live.
    let (src, dst) = unsafe { (from.GetDesc(), to.GetDesc()) };
    if !is_texture(&src)
        || !is_texture(&dst)
        || src.SampleDesc.Count != 1
        || dst.SampleDesc.Count != 1
    {
        return None;
    }
    let mode = match (src.Format.0 as u32, dst.Format.0 as u32) {
        (packed, 28) if packed == logical => logical | 256,
        (28, packed) if packed == logical => logical,
        _ => return None,
    };
    Some((src, dst, mode))
}

fn region_desc(desc: &D3D12_RESOURCE_DESC, extent: [u32; 3]) -> D3D12_RESOURCE_DESC {
    let [width, height, depth] = extent;
    D3D12_RESOURCE_DESC {
        Dimension: desc.Dimension,
        Format: desc.Format,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: depth as u16,
        MipLevels: 1,
        SampleDesc: desc.SampleDesc,
        ..Default::default()
    }
}

/// Copies matching single-sample textures backed by two representations of
/// the same logical format. After successful setup, prepare records the
/// caller's transitions to COPY_SOURCE and COPY_DEST. A successful copy
/// replaces the list's native compute root signature and pipeline.
pub(crate) fn record(
    list: &HostList,
    to: &ID3D12Resource,
    from: &ID3D12Resource,
    logical: u32,
    prepare: impl FnOnce(),
) -> bool {
    let Some((src, dst, mode)) = descriptions(to, from, logical) else {
        return false;
    };
    copy_matching(list, to, from, (src, dst, mode), prepare)
}

/// A differently typed 16-bit view placed over a packed render target sees
/// the guest's packed bits. Its copy footprint uses the view's physical format.
pub(crate) fn record_alias(
    list: &HostList,
    to: &ID3D12Resource,
    from: &ID3D12Resource,
    logical: u32,
) -> bool {
    // SAFETY: both resources are retained by the command list.
    let (src, dst) = unsafe { (from.GetDesc(), to.GetDesc()) };
    if src.Format.0 as u32 != crate::resource::RGBA8_UNORM
        || crate::resource::element(dst.Format.0 as u32) != Some((2, 1))
        || src.SampleDesc.Count != 1
        || dst.SampleDesc.Count != 1
    {
        return false;
    }
    copy_matching(list, to, from, (src, dst, logical), || {})
}

fn copy_matching(
    list: &HostList,
    to: &ID3D12Resource,
    from: &ID3D12Resource,
    (src, dst, mode): (D3D12_RESOURCE_DESC, D3D12_RESOURCE_DESC, u32),
    prepare: impl FnOnce(),
) -> bool {
    if src.Dimension != dst.Dimension
        || src.Width != dst.Width
        || src.Height != dst.Height
        || src.DepthOrArraySize != dst.DepthOrArraySize
        || src.MipLevels != dst.MipLevels
    {
        return false;
    }
    let Some(host) = host() else { return false };
    let device = &host.device;
    let count = subresource_count(&src);
    let (Some(src), Some(dst)) = (
        Surface::new(device, from, &src, 0, count),
        Surface::new(device, to, &dst, 0, count),
    ) else {
        return false;
    };
    write(list, device, src, dst, mode, [0; 3], None, prepare)
}

/// Copies a selected region using the same packed conversion as a whole copy.
pub(crate) fn record_region(
    list: &HostList,
    to: &ID3D12Resource,
    from: &ID3D12Resource,
    logical: u32,
    region: Region,
    prepare: impl FnOnce(),
) -> bool {
    let Some((src, dst, mode)) = descriptions(to, from, logical) else {
        return false;
    };
    let (Some(src_extent), Some(dst_extent)) = (
        subresource_extent(&src, region.src_subresource),
        subresource_extent(&dst, region.dst_subresource),
    ) else {
        return false;
    };
    let copy_extent = match region.source_box {
        Some(b) => {
            let start = [b.left, b.top, b.front];
            let end = [b.right, b.bottom, b.back];
            if (0..3).any(|i| start[i] >= end[i] || end[i] > src_extent[i]) {
                return false;
            }
            std::array::from_fn(|i| end[i] - start[i])
        }
        None => src_extent,
    };
    if (0..3).any(|i| {
        region.offset[i]
            .checked_add(copy_extent[i])
            .is_none_or(|end| end > dst_extent[i])
    }) {
        return false;
    }
    let Some(host) = host() else { return false };
    let device = &host.device;
    let (Some(src), Some(dst)) = (
        Surface::new(
            device,
            from,
            &region_desc(&src, copy_extent),
            region.src_subresource,
            1,
        ),
        Surface::new(
            device,
            to,
            &region_desc(&dst, copy_extent),
            region.dst_subresource,
            1,
        ),
    ) else {
        return false;
    };
    write(
        list,
        device,
        src,
        dst,
        mode,
        region.offset,
        region.source_box,
        prepare,
    )
}

// Each 64-thread group converts two texels per thread in one row.
fn dispatch_shape(
    src: D3D12_SUBRESOURCE_FOOTPRINT,
    dst: D3D12_SUBRESOURCE_FOOTPRINT,
    mode: u32,
) -> ([u32; 4], [u32; 3]) {
    (
        [src.Width, src.RowPitch, dst.RowPitch, mode],
        [src.Width.div_ceil(128), src.Height, 1],
    )
}

#[allow(clippy::too_many_arguments)]
fn write(
    list: &HostList,
    device: &ID3D12Device,
    source: Surface<'_>,
    destination: Surface<'_>,
    mode: u32,
    [x, y, z]: [u32; 3],
    source_box: Option<D3D12_BOX>,
    prepare: impl FnOnce(),
) -> bool {
    let Some(converter) = converter(device) else {
        return false;
    };
    let (Ok(input), Ok(output)) = (
        buffer(
            device,
            source.bytes,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        ),
        buffer(
            device,
            destination.bytes,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
    ) else {
        return false;
    };

    prepare();
    let native = &list.list;
    let source_box = source_box.as_ref().map(std::ptr::from_ref);
    // SAFETY: an open direct list. Locations are compatible with the native
    // textures, the caller transitioned them, and all resources survive execution.
    unsafe {
        for (index, &footprint) in source.layouts.iter().enumerate() {
            let to = buffer_location(&input, footprint);
            let from = texture_location(source.texture, source.first_subresource + index as u32);
            native.CopyTextureRegion(&to, 0, 0, 0, &from, source_box);
        }
        native.ResourceBarrier(&[transition(
            &input,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
        native.SetComputeRootSignature(&converter.root);
        native.SetPipelineState(&converter.pso);
        let input_base = input.GetGPUVirtualAddress();
        let output_base = output.GetGPUVirtualAddress();
        for (input, output) in source.layouts.iter().zip(&destination.layouts) {
            let src = input.Footprint;
            let dst = output.Footprint;
            let (constants, [groups_x, groups_y, groups_z]) = dispatch_shape(src, dst, mode);
            native.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            // Bind one depth slice at a time so shader byte offsets fit in u32.
            let src_slice = u64::from(src.RowPitch) * u64::from(src.Height);
            let dst_slice = u64::from(dst.RowPitch) * u64::from(dst.Height);
            for slice in 0..u64::from(src.Depth) {
                native.SetComputeRootShaderResourceView(
                    1,
                    input_base + input.Offset + src_slice * slice,
                );
                native.SetComputeRootUnorderedAccessView(
                    2,
                    output_base + output.Offset + dst_slice * slice,
                );
                native.Dispatch(groups_x, groups_y, groups_z);
            }
        }
        native.ResourceBarrier(&[transition(
            &output,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        for (index, &footprint) in destination.layouts.iter().enumerate() {
            let to = texture_location(
                destination.texture,
                destination.first_subresource + index as u32,
            );
            let from = buffer_location(&output, footprint);
            native.CopyTextureRegion(&to, x, y, z, &from, None);
        }
    }
    list.keep(&input);
    list.keep(&output);
    list.keep(source.texture);
    list.keep(destination.texture);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{PACKED_16, RGBA8_UNORM, packed_test};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

    #[test]
    fn packed_copy_dispatch_uses_both_pitches_and_covers_odd_widths() {
        for (width, groups_x) in [(1, 1), (128, 1), (129, 2)] {
            for format in PACKED_16 {
                for (mode, src_pitch, dst_pitch) in [(format, 768, 512), (format | 256, 512, 768)] {
                    let src = D3D12_SUBRESOURCE_FOOTPRINT {
                        Width: width,
                        Height: 3,
                        Depth: 2,
                        RowPitch: src_pitch,
                        ..Default::default()
                    };
                    let dst = D3D12_SUBRESOURCE_FOOTPRINT {
                        RowPitch: dst_pitch,
                        ..src
                    };
                    assert_eq!(
                        dispatch_shape(src, dst, mode),
                        ([width, src_pitch, dst_pitch, mode], [groups_x, 3, 1]),
                    );
                }
            }
        }
    }

    fn make_texture(device: &ID3D12Device, format: u32, width: u32, height: u32) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(format as i32),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let mut out = None;
        // SAFETY: valid default 2D texture descriptor and output pointer.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut out,
                )
                .unwrap();
        }
        out.unwrap()
    }

    fn make_upload(
        device: &ID3D12Device,
        width: u32,
        height: u32,
        row_pitch: usize,
        bytes: &[u16],
    ) -> ID3D12Resource {
        let len = row_pitch * height as usize;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out = None;
        // SAFETY: valid upload buffer descriptor and output pointer.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut out,
                )
                .unwrap();
        }
        let buf: ID3D12Resource = out.unwrap();
        let mut mapped = std::ptr::null_mut();
        // SAFETY: newly created upload buffer spans `len` bytes.
        unsafe {
            buf.Map(0, None, Some(&mut mapped)).unwrap();
            let dst = std::slice::from_raw_parts_mut(mapped.cast::<u8>(), len);
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let at = y * row_pitch + x * 2;
                    dst[at..at + 2].copy_from_slice(&bytes[y * width as usize + x].to_le_bytes());
                }
            }
            buf.Unmap(0, None);
        }
        buf
    }

    fn readback_raw(
        host: &crate::host::Host,
        texture: &ID3D12Resource,
        format: u32,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let len = 256 * height as usize;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback = None;
        // SAFETY: valid readback buffer descriptor and output pointer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback: ID3D12Resource = readback.unwrap();
        let list = host.new_list().unwrap();
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(format as i32),
                Width: width,
                Height: height,
                Depth: 1,
                RowPitch: 256,
            },
        };
        // SAFETY: `texture` is in COPY_SOURCE and `readback` is in COPY_DEST.
        unsafe {
            list.list.CopyTextureRegion(
                &buffer_location(&readback, footprint),
                0,
                0,
                0,
                &texture_location(texture, 0),
                None,
            );
        }
        list.close();
        host.execute(&[&list]);
        let mut out = vec![0u8; len];
        let mut mapped = std::ptr::null_mut();
        // SAFETY: `host.execute` waited for GPU completion.
        unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE { Begin: 0, End: len }),
                    Some(&mut mapped),
                )
                .unwrap();
            out.copy_from_slice(std::slice::from_raw_parts(mapped.cast::<u8>(), len));
            readback.Unmap(0, None);
        }
        out
    }

    #[test]
    fn packed_copy_converts_between_packed_16_and_rgba8_on_any_adapter() {
        const W: u32 = 3;
        const H: u32 = 2;
        const VALUES: [u16; 6] = [0xfc00, 0x83e0, 0x001f, 0xaa53, 0x4210, 0xffff];
        let Some(host) = host() else {
            return;
        };
        for logical in PACKED_16 {
            let src = make_texture(&host.device, logical, W, H);
            let wide = make_texture(&host.device, RGBA8_UNORM, W, H);
            let dst = make_texture(&host.device, logical, W, H);
            let upload = make_upload(&host.device, W, H, 256, &VALUES);

            let list = host.new_list().unwrap();
            let fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT(logical as i32),
                    Width: W,
                    Height: H,
                    Depth: 1,
                    RowPitch: 256,
                },
            };
            // SAFETY: open direct list with live resources in valid states.
            unsafe {
                list.list.CopyTextureRegion(
                    &texture_location(&src, 0),
                    0,
                    0,
                    0,
                    &buffer_location(&upload, fp),
                    None,
                );
                list.list.ResourceBarrier(&[transition(
                    &src,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            assert!(record(&list, &wide, &src, logical, || {}));
            // SAFETY: transition `wide` from COPY_DEST to COPY_SOURCE for reverse copy.
            unsafe {
                list.list.ResourceBarrier(&[transition(
                    &wide,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            assert!(record(&list, &dst, &wide, logical, || {}));
            // SAFETY: transition `dst` from COPY_DEST to COPY_SOURCE for readback.
            unsafe {
                list.list.ResourceBarrier(&[transition(
                    &dst,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            list.close();
            host.execute(&[&list]);

            let wide_bytes = readback_raw(host, &wide, RGBA8_UNORM, W, H);
            packed_test::assert_texels(logical, RGBA8_UNORM, &wide_bytes, 256, W, H, |x, y| {
                VALUES[(y * W + x) as usize]
            });
            let dst_bytes = readback_raw(host, &dst, logical, W, H);
            packed_test::assert_texels(logical, logical, &dst_bytes, 256, W, H, |x, y| {
                VALUES[(y * W + x) as usize]
            });
            packed_test::assert_no_d3d12_errors(host);
        }
    }
}
