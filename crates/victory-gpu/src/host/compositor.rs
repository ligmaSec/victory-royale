//! Composites PresentX display planes using premultiplied alpha.
use super::*;
use crate::queue::MAX_PRESENT_PLANES;

/// Built-in fullscreen-triangle compositor for multi-plane `PresentX` calls
/// (e.g. Series S presenting a 1080p scene plane with a 4K UI overlay plane).
pub(super) struct Compositor {
    pub(super) format: DXGI_FORMAT,
    root: ID3D12RootSignature,
    opaque: ID3D12PipelineState,
    overlay: ID3D12PipelineState,
    rtv_heap: ID3D12DescriptorHeap,
    srv_heap: ID3D12DescriptorHeap,
    srv_stride: u32,
}

impl Compositor {
    pub(super) fn new(device: &ID3D12Device, format: DXGI_FORMAT) -> windows::core::Result<Self> {
        const SOURCE: &str = r#"
            Texture2D tex : register(t0);
            SamplerState smp : register(s0);
            void vs(uint id : SV_VertexID, out float4 pos : SV_Position, out float2 uv : TEXCOORD) {
                uv = float2((id << 1) & 2, id & 2);
                pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
            }
            float4 ps(float4 pos : SV_Position, float2 uv : TEXCOORD) : SV_Target {
                return tex.Sample(smp, uv);
            }
        "#;
        let compile = |entry: PCSTR, target: PCSTR| -> windows::core::Result<ID3DBlob> {
            let mut code: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            // SAFETY: static source and entry/target strings; out pointers ours.
            let res = unsafe {
                D3DCompile(
                    SOURCE.as_ptr().cast(),
                    SOURCE.len(),
                    s!("present_composite"),
                    None,
                    None,
                    entry,
                    target,
                    0,
                    0,
                    &mut code,
                    Some(&mut errors),
                )
            };
            if let Err(e) = res {
                if let Some(errors) = errors {
                    // SAFETY: the blob's own bytes.
                    let text = unsafe {
                        std::slice::from_raw_parts(
                            errors.GetBufferPointer().cast::<u8>(),
                            errors.GetBufferSize(),
                        )
                    };
                    tracing::warn!("present compositor: {}", String::from_utf8_lossy(text));
                }
                return Err(e);
            }
            code.ok_or_else(windows::core::Error::empty)
        };
        let vs = compile(s!("vs"), s!("vs_5_1"))?;
        let ps = compile(s!("ps"), s!("ps_5_1"))?;
        // SAFETY: compiled shader blobs live through PSO creation.
        unsafe {
            let vs_bytes =
                std::slice::from_raw_parts(vs.GetBufferPointer().cast::<u8>(), vs.GetBufferSize());
            let ps_bytes =
                std::slice::from_raw_parts(ps.GetBufferPointer().cast::<u8>(), ps.GetBufferSize());
            // Wine's D3DCompile does not embed a root signature in the shader blobs.
            let range = D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
                ..Default::default()
            };
            let parameter = D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &range,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            };
            let sampler = D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                MaxAnisotropy: 16,
                ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
                BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
                MaxLOD: f32::MAX,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
                ..Default::default()
            };
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 1,
                pParameters: &parameter,
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
                ..Default::default()
            };
            let mut signature = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut signature, None)?;
            let signature = signature.ok_or_else(windows::core::Error::empty)?;
            let signature = std::slice::from_raw_parts(
                signature.GetBufferPointer().cast::<u8>(),
                signature.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, signature)?;
            let make_pso = |blend: bool| -> windows::core::Result<ID3D12PipelineState> {
                let mut rt_blend = D3D12_RENDER_TARGET_BLEND_DESC {
                    RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    ..Default::default()
                };
                if blend {
                    rt_blend.BlendEnable = true.into();
                    rt_blend.SrcBlend = D3D12_BLEND_ONE;
                    rt_blend.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOp = D3D12_BLEND_OP_ADD;
                    rt_blend.SrcBlendAlpha = D3D12_BLEND_ONE;
                    rt_blend.DestBlendAlpha = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOpAlpha = D3D12_BLEND_OP_ADD;
                }
                let mut blend_desc = D3D12_BLEND_DESC::default();
                blend_desc.RenderTarget[0] = rt_blend;
                let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
                rtv_formats[0] = format;
                let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                    VS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: vs_bytes.as_ptr().cast(),
                        BytecodeLength: vs_bytes.len(),
                    },
                    PS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: ps_bytes.as_ptr().cast(),
                        BytecodeLength: ps_bytes.len(),
                    },
                    BlendState: blend_desc,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: false.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: rtv_formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                let pso = device.CreateGraphicsPipelineState(&desc);
                let _ = std::mem::ManuallyDrop::into_inner(desc.pRootSignature);
                pso
            };
            let opaque = make_pso(false)?;
            let overlay = make_pso(true)?;
            let rtv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })?;
            let srv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: MAX_PRESENT_PLANES as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let srv_stride =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            Ok(Compositor {
                format,
                root,
                opaque,
                overlay,
                rtv_heap,
                srv_heap,
                srv_stride,
            })
        }
    }
}

// A shader sampling float4 needs a filterable color view. Resolve a
// typeless texture in its own family and preserve typed numeric formats.
fn view_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        9 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        23 => DXGI_FORMAT_R10G10B10A2_UNORM,
        27 => DXGI_FORMAT_R8G8B8A8_UNORM,
        90 => DXGI_FORMAT_B8G8R8A8_UNORM,
        10 | 11 | 13 | 24 | 26 | 28 | 29 | 31 | 87 | 91 => format,
        _ => return None,
    })
}
pub(super) fn can_sample(texture: &ID3D12Resource) -> bool {
    // SAFETY: the caller holds the resource throughout presentation.
    let d = unsafe { texture.GetDesc() };
    d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && d.DepthOrArraySize == 1
        && d.SampleDesc.Count == 1
        && (d.Flags & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE).0 == 0
        && view_format(d.Format).is_some()
}
impl Compositor {
    /// Draw the planes onto the back buffer, leaving it in RENDER_TARGET
    /// and restoring the original state of each sampled source.
    pub(super) fn draw(
        &self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        back: &ID3D12Resource,
        planes: &[(&ID3D12Resource, u32)],
        width: u32,
        height: u32,
    ) {
        let format = self.format;
        // SAFETY: all resources and descriptor heaps stay live until the synchronous present completes.
        unsafe {
            let rtv = self.rtv_heap.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(
                back,
                Some(&D3D12_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_RTV {
                            MipSlice: 0,
                            PlaneSlice: 0,
                        },
                    },
                }),
                rtv,
            );
            let srv_cpu_base = self.srv_heap.GetCPUDescriptorHandleForHeapStart();
            let srv_gpu_base = self.srv_heap.GetGPUDescriptorHandleForHeapStart();
            let count = planes.len();
            let mut before = Vec::with_capacity(count + 1);
            let mut after = Vec::with_capacity(count);
            for (i, &(tex, state)) in planes[..count].iter().enumerate() {
                let d = tex.GetDesc();
                let srv_fmt = view_format(d.Format).expect("checked before composition");
                let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: srv_cpu_base.ptr + i * self.srv_stride as usize,
                };
                device.CreateShaderResourceView(
                    tex,
                    Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: srv_fmt,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                                PlaneSlice: 0,
                                ResourceMinLODClamp: 0.0,
                            },
                        },
                    }),
                    handle,
                );
                let state = D3D12_RESOURCE_STATES(state as i32);
                if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                    && planes[..i]
                        .iter()
                        .all(|(earlier, _)| earlier.as_raw() != tex.as_raw())
                {
                    before.push(transition(
                        tex,
                        state,
                        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    ));
                    after.push(transition(
                        tex,
                        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        state,
                    ));
                }
            }
            before.push(transition(
                back,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            ));
            list.ResourceBarrier(&before);
            list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }]);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            for i in 0..count {
                list.SetPipelineState(if i == 0 { &self.opaque } else { &self.overlay });
                list.SetGraphicsRootDescriptorTable(
                    0,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: srv_gpu_base.ptr + (i as u64) * u64::from(self.srv_stride),
                    },
                );
                list.DrawInstanced(3, 1, 0, 0);
            }

            if !after.is_empty() {
                list.ResourceBarrier(&after);
            }
        }
    }
}
