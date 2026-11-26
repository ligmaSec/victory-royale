//! DXIL emission (decision 0003, step 3): a console vertex, pixel, compute
//! or mesh shader as a PC DXIL container (SM 6.6) that host D3D12 accepts.
//! Compute shaders normally keep the console group dimensions and declare the
//! wave size when lane order matters. On narrower adapters, a 64-thread group
//! that only needs quad neighbors for sampling can use a flat host group with
//! the original per-thread coordinates reconstructed from the group index.
//!
//! What the host binds for a translated shader ([`HostShader`]):
//!
//! * **User data** at `b0, space 0` (32-bit root constants or a constant
//!   buffer, per stage): dword `index` holds a root parameter's value, as
//!   [`UserData`] lists. A root-constant dword is the constant. A
//!   descriptor table is the byte offset of its first descriptor in its
//!   heap: the descriptor index times 32 for CBV/SRV/UAV tables, times 16
//!   for sampler tables (the console's descriptor sizes; the code adds
//!   offsets in those units). The heap bases of dynamic resources are 0
//!   and not listed.
//! * **Descriptors** through SM 6.6 `ResourceDescriptorHeap[]` and
//!   `SamplerDescriptorHeap[]`, at the index the console address gives, so
//!   the host heaps keep the title's descriptor order. The root signature
//!   needs `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` and
//!   `SAMPLER_HEAP_DIRECTLY_INDEXED`.
//! * **Immediate samplers** as static samplers `s<n>, space 0`, `n` the
//!   position in [`HostShader::samplers`] (the console sampler words) and
//!   [`HostShader::static_samplers`] (the same decoded into a D3D12 static
//!   sampler description). Comparison sampling (added 2026-11-23): an
//!   immediate sampler it uses has a comparison filter there (bit 0x80);
//!   a heap sampler it uses must be a comparison sampler on the host.
//! * **Root CBVs and root SRVs** (raw buffers) as root descriptors
//!   `b<i>` / `t<i>` in [`ROOT_CBV_SPACE`], `i` the title's root parameter
//!   index ([`HostShader::root_cbvs`], [`HostShader::root_srvs`]); the
//!   address the title sets is the buffer's.
//! * **Vertex fetch** (added 2026-11-23). The title's mesh and
//!   vertex programs fetch vertices themselves with typed-format loads
//!   (`tbuffer_load_format_*`): the format (8_8_8_8 unorm, 16_16 snorm,
//!   2_10_10_10 unorm, 8_8_8_8 and 16_16_16_16 uint...) is in the
//!   instruction, so the translation reads raw bytes and converts them
//!   itself. Their descriptors are built around a root SRV (stride 1, no
//!   record limit): the host binds the root SRV `t<slot>` above with the
//!   address the title sets, nothing more. The vertex index, the stride
//!   and the attribute offset are the shader's own arithmetic on data in
//!   that buffer (per-stream headers), not descriptor fields.
//! * **Buffer views in the heap** are declared by how the shader reads
//!   them: raw (ByteAddressBuffer) for dword loads and stores, atomics and
//!   typed-format loads without an index; structured with the used-resource
//!   entry's stride or the bound view's stride when indexed; typed with four
//!   32-bit float channels for `buffer_load_format_*` and
//!   `buffer_store_format_*`. The host's view at
//!   that index must be of that kind and start at the console descriptor's
//!   base address (raw: `R32_TYPELESS` with the raw flag), whatever kind the
//!   title's view description says. Typed-format loads from heap
//!   descriptors (1,024 world compute programs, ray-tracing material
//!   resolves) read raw views this way.
//!
//! * **Texture atomics** use a scalar integer UAV. The host binds R32_UINT
//!   for 32-bit operations and R32G32_UINT for 64-bit operations. The latter
//!   require SM 6.6 atomic support on typed and descriptor-heap resources.
//!
//! Entry points: [`translate`] (container bytes as the title passes them
//! at pipeline creation → [`HostShader`]), [`emit`] (the same with the
//! earlier stages already run). The root constants of each stage must
//! cover [`HostShader::user_data_dwords`]. Stability: these types and the
//! binding scheme are what pipeline creation builds on; changes are
//! announced.
//! * **Signatures** as the console container has them (same semantics,
//!   registers and masks), so the input layout and stage linking are the
//!   title's; pixel outputs are plain 32-bit values.
//! * **Mesh shaders** ([`HwStage::Ms`], `ms_6_6`): user data and
//!   descriptors as above (the root constants visible to the mesh stage);
//!   the pipeline is a mesh pipeline (pipeline-state stream, no input
//!   layout) with the title's pixel shader. The thread group is the
//!   console's in lanes (`[numthreads]` in the DXIL), the vertex and
//!   primitive maxima its own, triangles only, no wave size required. The
//!   console numbers groups in x only: the draw path passes the title's
//!   count as `DispatchMesh(x, 1, 1)`. Per-primitive outputs (`PSG1`) are
//!   the console's, without `SV_PrimitiveID` and `SV_CullPrimitive` (the
//!   console never exports them). When the output signature exceeds the
//!   DXIL byte limit, the pipeline keeps only the varyings its pixel shader
//!   declares and the rasterizer system values.

mod bitcode;
#[cfg(test)]
mod bvh_tests;
mod container;
#[cfg(all(test, windows))]
mod diff_tests;
#[cfg(all(windows, any(test, feature = "host")))]
pub mod host;
#[cfg(all(test, windows))]
mod lane_mask_tests;
#[cfg(test)]
mod lds_tests;
mod lower;
#[cfg(test)]
mod mad_flush_tests;
#[cfg(test)]
mod mtbuf_tests;
mod rect;
#[cfg(test)]
mod root_uav_tests;
mod sampler;
pub use rect::rect_list_gs;
mod shader;
mod threading;
#[cfg(windows)]
pub mod validator;
#[cfg(all(test, windows))]
mod valu_tests;
#[cfg(all(test, windows))]
mod waterfall_tests;
mod wave;
#[cfg(all(test, windows))]
mod wave_prefix_tests;

use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, RootType};
use crate::{ShaderError, io, ir, resource, structure};

pub use sampler::StaticSampler;
pub use shader::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};

/// Resource heap indices at or past this go to slot 0 in translated
/// shaders (a negative or garbage index must not reach past the heap's
/// end). It is far above any real heap, and the host never makes a view
/// heap larger, so a valid index is never moved: 2^24.
pub const VIEW_HEAP_LIMIT: u32 = 1 << 24;

/// The same for sampler heap indices: the host's limit on every binding
/// tier.
pub const SAMPLER_HEAP_LIMIT: u32 = 2048;

/// The loop limit without an override: 2^20.
pub const DEFAULT_LOOP_LIMIT: u32 = lower::LOOP_LIMIT;

/// The iteration count after which a translated loop is left, per lane and
/// per entry into the loop: [`DEFAULT_LOOP_LIMIT`].
///
/// With the `diagnostics` feature, `VICTORY_SHADER_LOOP_LIMIT` (decimal or
/// `0x` hex, at least 1) sets it instead. A loop that hits the limit in
/// every lane of a full-screen pass still runs for seconds (2^20 times a
/// 4K screen is about 10^13 iterations); a much lower limit shows whether
/// such loops are what makes a frame slow. The value is part of the
/// translation, so the shader cache must key on it (`victory-gpu`'s does).
pub fn loop_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_loop_limit(std::env::var("VICTORY_SHADER_LOOP_LIMIT").ok().as_deref())
        } else {
            lower::LOOP_LIMIT
        }
    })
}

/// The clean-up passes that run over a translated module before it is
/// written (`bitcode::opt`): a set of its pass bits. Every pass is exact, so
/// the switch is for telling a lowering fault from a clean-up fault and for
/// measuring what the clean-up is worth.
///
/// With the `diagnostics` feature, `VICTORY_SHADER_OPT` (decimal or `0x`
/// hex) chooses them: `0` none; 1 dead code, 2 constants and identities, 4
/// one instruction for one value, 8 what a branch taken says about its
/// condition, 16 one read for one place within a block. Unset or unreadable:
/// every pass. The value is part of the translation, so the shader cache
/// must key on it (`victory-gpu`'s does).
pub fn shader_opt() -> u32 {
    static PASSES: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *PASSES.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_shader_opt(std::env::var("VICTORY_SHADER_OPT").ok().as_deref())
        } else {
            bitcode::opt::ALL
        }
    })
}

/// `VICTORY_SHADER_OPT`'s value; every pass when it is unset or
/// unreadable. Pure, for testing.
fn parse_shader_opt(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    parsed.map_or(bitcode::opt::ALL, |n| n & bitcode::opt::ALL)
}

/// `VICTORY_SHADER_LOOP_LIMIT`'s value; the default when it is unset,
/// unreadable or 0. Pure, for testing.
fn parse_loop_limit(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    match parsed {
        Some(n) if n > 0 => n,
        _ => lower::LOOP_LIMIT,
    }
}

/// A translated shader and what the host binds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShader {
    pub stage: HwStage,
    /// The DXIL container, hashed as D3D12 expects.
    pub dxil: Vec<u8>,
    /// The user-data dwords the shader reads.
    pub user_data: Vec<UserData>,
    /// Immediate samplers: the four sampler words of `s<n>`.
    pub samplers: Vec<[u32; 4]>,
    /// The same samplers decoded, `s<n>` at position n: the static sampler
    /// descriptions the host's root signature declares.
    pub static_samplers: Vec<StaticSampler>,
    /// Root CBVs the shader reads: root parameter indices, each bound as
    /// `b<index>` in [`ROOT_CBV_SPACE`].
    pub root_cbvs: Vec<u8>,
    /// Root SRVs (raw buffers) the shader reads: `t<index>` in
    /// [`ROOT_CBV_SPACE`].
    pub root_srvs: Vec<u8>,
    /// Root UAVs (raw buffers) the shader reads and writes: `u<index>` in
    /// [`ROOT_CBV_SPACE`].
    pub root_uavs: Vec<u8>,
}

/// `SFI0` feature bits of a shader that indexes the resource or the sampler
/// descriptor heap directly.
pub(crate) const SFI0_RESOURCE_HEAP: u64 = 0x200_0000;
pub(crate) const SFI0_SAMPLER_HEAP: u64 = 0x400_0000;

impl HostShader {
    /// The compute group dimensions recorded in our emitted PSV runtime info.
    /// This also works for shaders loaded from the on-disk translation cache.
    pub fn compute_threads(&self) -> Option<[u32; 3]> {
        if self.stage != HwStage::Cs {
            return None;
        }
        let container = Container::parse(&self.dxil).ok()?;
        let psv = container.part(FourCC::PSV0)?.data;
        let size = crate::dxbc::u32_at(psv, 0).ok()? as usize;
        let info = psv.get(4..)?.get(..size)?;
        let dims = info.get(36..48)?.as_chunks::<4>().0;
        Some([
            u32::from_le_bytes(dims[0]),
            u32::from_le_bytes(dims[1]),
            u32::from_le_bytes(dims[2]),
        ])
    }

    /// Whether the shader indexes a descriptor heap directly
    /// (`ResourceDescriptorHeap` / `SamplerDescriptorHeap`), which its root
    /// signature must then allow. A container without readable feature
    /// flags counts as one that does.
    pub fn indexes_heaps(&self) -> bool {
        let features = Container::parse(&self.dxil).ok().and_then(|c| {
            let data = c.part(FourCC::SFI0)?.data.get(..8)?;
            Some(u64::from_le_bytes(data.try_into().ok()?))
        });
        features.is_none_or(|f| f & (SFI0_RESOURCE_HEAP | SFI0_SAMPLER_HEAP) != 0)
    }

    /// Size of the user-data buffer the shader declares, in dwords (a
    /// multiple of 4; 0 when it reads none). Root constants must cover it.
    pub fn user_data_dwords(&self) -> u32 {
        let n = self
            .user_data
            .iter()
            .map(|u| u.index + if u.indexed_buffer { 3 } else { 1 })
            .max()
            .unwrap_or(0);
        n.next_multiple_of(4)
    }
}

/// One dword of user data and the root parameter that fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserData {
    /// The root constant selects a heap buffer. At `index` the host supplies
    /// the raw SRV slot shifted left four bits with its byte bias in the low
    /// bits, followed by the element stride and the logical length in bytes.
    pub indexed_buffer: bool,
    /// Dword in the user-data buffer (the console's user-data index).
    pub index: u32,
    /// Root parameter index in the title's root signature.
    pub slot: u8,
    /// `Constants` or `Table` (see the module notes for the value).
    pub root: RootType,
    /// Dword within the parameter.
    pub dword: u32,
}

/// A statically addressed descriptor whose structured stride can come from
/// the bound view. Offsets are in the console's descriptor-table dwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StructuredBinding {
    pub slot: u8,
    pub offset_dwords: u32,
    pub uav: bool,
}

// A required lookup discovers bindings. An optional lookup recognizes a
// binding already being specialized when lowering another access to it.
type ViewStride<'a> = &'a dyn Fn(StructuredBinding, bool) -> Option<u32>;

const UNKNOWN_STRUCTURED_STRIDE: &str = "a structured buffer of unknown stride";

/// Adapter-specific lowering choices for console compute waves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputeWaves {
    /// May issue a fixed 32-lane reduction.
    pub wave32: bool,
    /// The host has quad operations but cannot run a native 64-lane wave.
    pub narrow_quads: bool,
}

impl ComputeWaves {
    /// Preserve native Wave64 code when the adapter's maximum width is unknown.
    fn with_wave32(wave32: bool) -> Self {
        Self {
            wave32,
            narrow_quads: false,
        }
    }
}

/// Translates the vertex, pixel, compute or mesh program of a console
/// shader container, lowering eligible <=32-thread Wave64 reductions to
/// `WaveSize(32)`.
pub fn translate(container: &[u8]) -> Result<HostShader, ShaderError> {
    translate_with_wave32(container, true)
}

/// As [`translate`], controlling whether eligible <=32-thread Wave64 compute
/// reductions may lower with `WaveSize(32)`. Pass `false` on adapters that
/// only support 64-lane waves (`WaveLaneCountMin > 32`).
pub fn translate_with_wave32(
    container: &[u8],
    allow_wave32: bool,
) -> Result<HostShader, ShaderError> {
    translate_for_adapter::<false>(container, ComputeWaves::with_wave32(allow_wave32), None)
        .map(|(shader, _)| shader)
}

/// Translates using matching structured views for statically addressed table
/// entries with no usable stride in the shader. Returns the resource metadata
/// used for host binding alongside the translated shader. The resolver's
/// second argument is true for an access that requires a structured view.
pub fn translate_with_view_strides(
    container: &[u8],
    allow_wave32: bool,
    view_stride: ViewStride<'_>,
) -> Result<(HostShader, Vec<crate::UsedResource>), ShaderError> {
    translate_for_adapter::<false>(
        container,
        ComputeWaves::with_wave32(allow_wave32),
        Some(view_stride),
    )
    .map(|(shader, native)| (shader, native.used))
}

/// A host-supplied extent in threads for a partial compute group.
pub const DISPATCH_THREADS: RootType = RootType::Other(0x104);

/// Translate a compute shader whose last group may be incomplete. Each
/// supplied extent is bound through the emitted user-data metadata.
pub fn translate_partial_dispatch(
    container: &[u8],
    allow_wave32: bool,
    view_stride: ViewStride<'_>,
) -> Result<(HostShader, Vec<crate::UsedResource>), ShaderError> {
    translate_for_adapter::<true>(
        container,
        ComputeWaves::with_wave32(allow_wave32),
        Some(view_stride),
    )
    .map(|(shader, native)| (shader, native.used))
}

/// A precompiled compute module (`FSPX`): only GFX10 code, no DXIL to start from.
const FSPX_UNSUPPORTED: ShaderError = ShaderError::Unsupported {
    offset: 0,
    what: "FSPX compute module",
};

/// Translate for the adapter's actual wave widths, optionally resolving view
/// strides at bind time. Offline callers can keep using [`translate`].
pub fn translate_for_adapter<const PARTIAL: bool>(
    container: &[u8],
    waves: ComputeWaves,
    view_stride: Option<ViewStride<'_>>,
) -> Result<(HostShader, NativeProgram), ShaderError> {
    if container.starts_with(crate::dxbc::Fspx::MAGIC) {
        return Err(FSPX_UNSUPPORTED);
    }
    let c = Container::parse(container)?;
    let Some(dxil) = c.part(FourCC::DXIL) else {
        return Err(if c.fspx().is_some() {
            FSPX_UNSUPPORTED
        } else {
            ShaderError::Malformed("no DXIL part")
        });
    };
    let native = crate::native_programs(dxil.data)?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
            )
        })
        .ok_or(ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    let shader = translate_native::<PARTIAL>(&c, &native, waves, None, view_stride)?;
    Ok((shader, native))
}

/// Emits an oversized mesh shader with only the varyings its pipeline's
/// translated pixel shader declares. With no pixel shader, keeps rasterizer
/// system values. Returns `None` if the original signature fits the budget.
pub fn translate_mesh_for_pixel(
    c: &Container<'_>,
    native: &NativeProgram,
    pixel_dxil: Option<&[u8]>,
) -> Result<Option<HostShader>, ShaderError> {
    let crate::native::StageIo::Ms(ms) = &native.io else {
        return Err(ShaderError::Malformed("no mesh program"));
    };
    let mesh = shader::Mesh {
        max_vertices: ms.max_vertices,
        max_primitives: ms.max_primitives,
        topology: ms.topology,
    };
    let (outputs, prims) = (signature_part(c, FourCC::OSG1)?, primitive_signature(c)?);
    if mesh.output_bytes(&outputs, &prims) <= shader::Mesh::MAX_OUTPUT_BYTES {
        return Ok(None);
    }
    let mut pixel_inputs = Vec::new();
    if let Some(dxil) = pixel_dxil {
        pixel_inputs = signature_part(&Container::parse(dxil)?, FourCC::ISG1)?;
    }
    translate_native::<false>(
        c,
        native,
        ComputeWaves::with_wave32(true),
        Some(&pixel_inputs),
        None,
    )
    .map(Some)
}

fn translate_native<const PARTIAL: bool>(
    c: &Container<'_>,
    native: &NativeProgram,
    waves: ComputeWaves,
    pixel_inputs: Option<&[container::SigElement]>,
    view_stride: Option<ViewStride<'_>>,
) -> Result<HostShader, ShaderError> {
    // A compute program with root arguments past the 16 user SGPRs finds them
    // through the spill table `s[0:1]` points to.
    let p = ir::build_with(
        &native.patched_code(),
        native.wave64,
        native.vgprs,
        native.uses_spill_table(),
    )?;
    // Structurisation first: tracing per-lane descriptors needs it (its
    // failure is reported after the earlier stages').
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
    let io = io::raise(&p, native)?;
    let st = st?;
    let parts = Emission {
        c,
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
    };
    emit_with_options::<PARTIAL>(parts, waves, pixel_inputs, view_stride)
}

/// Emits a program whose earlier stages already ran.
pub fn emit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<HostShader, ShaderError> {
    emit_with_wave32(c, native, p, st, io, res, true)
}

/// As [`emit`], controlling whether eligible <=32-thread Wave64 compute
/// reductions may lower with `WaveSize(32)`.
pub fn emit_with_wave32(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
    allow_wave32: bool,
) -> Result<HostShader, ShaderError> {
    let parts = Emission {
        c,
        native,
        p,
        st,
        io,
        res,
    };
    emit_with_options::<false>(parts, ComputeWaves::with_wave32(allow_wave32), None, None)
}

struct Emission<'a> {
    c: &'a Container<'a>,
    native: &'a NativeProgram,
    p: &'a ir::Program,
    st: &'a structure::Structured,
    io: &'a io::Io,
    res: &'a resource::Resources,
}

#[cfg(test)]
fn emit_with_view_strides(
    parts: Emission<'_>,
    allow_wave32: bool,
    view_stride: Option<ViewStride<'_>>,
) -> Result<HostShader, ShaderError> {
    emit_with_options::<false>(
        parts,
        ComputeWaves::with_wave32(allow_wave32),
        None,
        view_stride,
    )
}

fn emit_with_options<const PARTIAL: bool>(
    parts: Emission<'_>,
    waves: ComputeWaves,
    pixel_inputs: Option<&[container::SigElement]>,
    view_stride: Option<ViewStride<'_>>,
) -> Result<HostShader, ShaderError> {
    let Emission {
        c, native, p, st, ..
    } = parts;
    let original = emit_program::<PARTIAL>(parts, waves, pixel_inputs, view_stride);
    // Retry unsupported programs only. Threading can separate an unknown
    // register value from its apparent use on a mutually exclusive path.
    if !matches!(original, Err(ShaderError::Unsupported { .. })) {
        return original;
    }
    let edges = threading::constant_edges(p, st);
    if edges.is_empty() {
        return original;
    }
    let p = ir::thread_edges(p, &edges)?;
    let st = structure::structure(&p)?;
    let io = io::raise(&p, native)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    emit_program::<PARTIAL>(
        Emission {
            c,
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
        },
        waves,
        pixel_inputs,
        view_stride,
    )
}

fn emit_program<const PARTIAL: bool>(
    parts: Emission<'_>,
    waves: ComputeWaves,
    pixel_inputs: Option<&[container::SigElement]>,
    view_stride: Option<ViewStride<'_>>,
) -> Result<HostShader, ShaderError> {
    let Emission {
        c,
        native,
        p,
        st,
        io,
        res,
    } = parts;
    let pixel = native.stage == HwStage::Ps;
    let mut inputs = signature_part(c, FourCC::ISG1)?;
    let mut outputs = signature_part(c, FourCC::OSG1)?;
    if pixel {
        // Coverage inputs use intrinsics. Sample index is a signature
        // element without a register. Other rasterizer inputs are packed.
        inputs.retain(|e| !matches!(e.kind, 14 | 15));
        let mut next = inputs
            .iter()
            .filter_map(|e| e.row)
            .max()
            .map_or(0, |r| r + 1);
        for e in &mut inputs {
            if e.kind == 12 {
                e.row = None;
                e.col = 0;
            } else if matches!(e.kind, 3..=5 | 10 | 13) && e.row.is_none() {
                e.row = Some(next);
                e.col = 0;
                next += 1;
            }
        }
        for e in &mut outputs {
            match e.kind {
                14 | 17..=20 => {
                    e.row = None;
                    e.col = 0;
                }
                16 => e.row = Some(e.index),
                _ => {}
            }
        }
    }
    // Interpolation as DXC declares it by default; pixel inputs that are
    // read get the mode their barycentrics say.
    let interpolated = if pixel { &mut inputs } else { &mut outputs };
    for e in interpolated {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            // Constant, as DXC declares it (the AMD compiler crashed on a
            // front face without a mode).
            (4 | 5 | 10 | 13, _) if pixel => 1,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
    }
    if let Some(psv) = c.part(FourCC::PSV0) {
        match native.stage {
            HwStage::Vs => container::producer_interpolation(psv.data, 1, &mut outputs),
            HwStage::Ms => container::producer_interpolation(psv.data, 13, &mut outputs),
            _ => {}
        }
    }
    // With a conservative depth output (SV_DepthLessEqual, GreaterEqual) DXIL
    // wants the position centroid (or per sample), as DXC declares it then.
    if pixel && outputs.iter().any(|e| matches!(e.kind, 18 | 19)) {
        for e in inputs.iter_mut().filter(|e| e.kind == 3) {
            e.interp = 5;
        }
    }
    let prims = primitive_signature(c)?;
    // A mesh group's id arrives multiplied by max(threads, vertices,
    // primitives) (the code divides by that); the thread count is in PSV0
    // from runtime info version 2 on.
    // shortcut: older containers fall back to the group's lane count, right
    // when the HLSL group fills its waves (the title's do).
    let mesh_stride = match &native.io {
        crate::native::StageIo::Ms(ms) => {
            let psv = c.part(FourCC::PSV0).map(|p| p.data);
            let threads = psv
                .filter(|d| crate::dxbc::u32_at(d, 0).is_ok_and(|n| n >= 48))
                .and_then(|d| crate::dxbc::u32_at(d, 4 + 36).ok());
            threads.map_or(ms.threads, |t| {
                t.max(ms.max_vertices).max(ms.max_primitives)
            })
        }
        _ => 0,
    };
    let l = lower::lower_with_waves::<PARTIAL>(
        &lower::Parts {
            native,
            p,
            st,
            io,
            res,
            inputs,
            outputs,
            prims,
            mesh_stride,
        },
        waves,
        pixel_inputs,
        view_stride,
    )?;
    Ok(HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .enumerate()
            .map(|(n, &w)| {
                let mut s = StaticSampler::from_words(w);
                if l.cmp_samplers >> n & 1 != 0 {
                    s.filter |= 0x80;
                }
                s
            })
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
    })
}

fn signature_part(
    c: &Container<'_>,
    part: FourCC,
) -> Result<Vec<container::SigElement>, ShaderError> {
    c.part(part)
        .map_or_else(|| Ok(Vec::new()), |part| signature(part.data))
}

fn primitive_signature(c: &Container<'_>) -> Result<Vec<container::SigElement>, ShaderError> {
    // The console never exports a primitive id or cull flag as a
    // parameter (it culls with the primitive export).
    let mut prims = signature_part(c, FourCC::PSG1)?;
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    for e in &mut prims {
        e.interp = 1;
    }
    Ok(prims)
}

/// The rows of an `ISG1`/`OSG1`/`PSG1` part.
fn signature(part: &[u8]) -> Result<Vec<container::SigElement>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    if base > part.len() || count > (part.len() - base) / 32 {
        return Err(ShaderError::Malformed("signature records out of bounds"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        let mask = *part
            .get(at + 24)
            .ok_or(ShaderError::Malformed("truncated"))?;
        if mask == 0 {
            continue;
        }
        let col = mask.trailing_zeros() as u8;
        let cols = mask.count_ones() as u8;
        if u16::from(mask) != ((1u16 << cols) - 1) << col || col + cols > 4 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a non-contiguous signature mask",
            });
        }
        if u32_at(part, at + 28)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a minimum-precision signature element",
            });
        }
        if u32_at(part, at)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a signature element of another stream",
            });
        }
        let kind = match u32_at(part, at + 12)? {
            0 => 0,
            1 => 3,
            2 => 6,
            3 => 7,
            4 => 4,
            5 => 5,
            6 => 1,
            7 => 10,
            8 => 2,
            9 => 13,
            10 => 12,
            64 => 16,
            65 => 17,
            66 => 14,
            67 => 19,
            68 => 18,
            69 => 20,
            70 => 15,
            25 => 30, // cull primitive
            _ => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "a system value in the signature",
                });
            }
        };
        // 16-bit elements are declared 32-bit; reading or writing one
        // converts (16-bit values travel packed on the console).
        let raw = u32_at(part, at + 16)?;
        let comp = match (kind, raw) {
            // DXIL declares the front face a uint (the validator insists).
            (13, _) => 5,
            (_, 1 | 4) => 5,
            (_, 2 | 5) => 4,
            (_, 3 | 6) => 9,
            (_, c) => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: match c {
                        0 => "a signature element of unknown type",
                        _ => "a 64-bit signature element",
                    },
                });
            }
        };
        let register = u32_at(part, at + 20)?;
        out.push(container::SigElement {
            name: String::from_utf8_lossy(name).into_owned(),
            index: u32_at(part, at + 8)?,
            kind,
            comp,
            interp: 0,
            row: (register != u32::MAX).then_some(register),
            col,
            cols,
            usage: 0,
            half: (4..=6).contains(&raw),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::bitcode::Attr;
    use super::lower::LOOP_LIMIT;
    use super::{parse_loop_limit, parse_shader_opt};

    #[test]
    fn fspx_modules_are_unsupported_not_missing_dxil() {
        let fspx = crate::dxbc::Fspx::assemble(&[0; 28], &[], &[0x55; 8], &[]);
        let wrapped = crate::dxbc::build(&[(crate::FourCC::ACCC, &fspx)]);
        for bytes in [&fspx, &wrapped] {
            assert_eq!(
                super::translate(bytes).unwrap_err(),
                crate::ShaderError::Unsupported {
                    offset: 0,
                    what: "FSPX compute module"
                }
            );
        }
        // Without an FSPX module it is still the missing DXIL part.
        let plain = crate::dxbc::build(&[(crate::FourCC::ACCC, b"other")]);
        assert_eq!(
            super::translate(&plain).unwrap_err(),
            crate::ShaderError::Malformed("no DXIL part")
        );
    }

    #[test]
    fn the_shader_opt_switch_parses_or_falls_back() {
        let all = super::bitcode::opt::ALL;
        assert_eq!(parse_shader_opt(None), all);
        assert_eq!(parse_shader_opt(Some("0")), 0);
        assert_eq!(parse_shader_opt(Some(" 1 ")), 1);
        assert_eq!(parse_shader_opt(Some("0x3")), 3);
        assert_eq!(parse_shader_opt(Some("0xffffffff")), all);
        assert_eq!(parse_shader_opt(Some("on")), all);
    }

    #[test]
    fn the_loop_limit_switch_parses_or_falls_back() {
        assert_eq!(parse_loop_limit(None), LOOP_LIMIT);
        assert_eq!(parse_loop_limit(Some("4096")), 4096);
        assert_eq!(parse_loop_limit(Some(" 0x1000 ")), 4096);
        assert_eq!(parse_loop_limit(Some("1")), 1);
        for bad in ["", "0", "-5", "lots", "0x", "4294967296"] {
            assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
        }
    }

    /// Heap indexing is read from the container's feature flags; without
    /// readable flags a shader is assumed to index heaps.
    #[test]
    fn heap_indexing_comes_from_the_feature_flags() {
        let shader = |parts: &[(FourCC, &[u8])]| HostShader {
            stage: HwStage::Ps,
            dxil: crate::dxbc::build(parts),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
        };
        let with = |features: u64| shader(&[(FourCC::SFI0, &features.to_le_bytes())]);
        assert!(!with(0x4000).indexes_heaps());
        assert!(with(0x4000 | SFI0_RESOURCE_HEAP).indexes_heaps());
        assert!(with(SFI0_SAMPLER_HEAP).indexes_heaps());
        assert!(shader(&[]).indexes_heaps());
        assert!(shader(&[(FourCC::SFI0, &[0; 4])]).indexes_heaps());
    }
    use super::container::SigElement;
    #[cfg(windows)]
    use super::shader::Shader;
    use super::*;
    use crate::native::{ComputeIo, StageIo};

    fn varying(name: &str, index: u32, row: u32) -> SigElement {
        SigElement {
            name: name.into(),
            index,
            kind: 0,
            comp: 9,
            interp: 2,
            row: Some(row),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        }
    }

    #[test]
    fn pixel_system_values_use_the_dxil_register_conventions() {
        let mut sample = varying("SV_SampleIndex", 0, 2);
        sample.kind = 12;
        sample.comp = 5;
        sample.cols = 1;
        sample.col = 2;
        let mut target = varying("SV_Target", 0, 3);
        target.kind = 16;
        let inputs = container::signature(&[sample], true);
        let outputs = container::signature(&[target], false);
        let source = crate::dxbc::build(&[(FourCC::ISG1, &inputs), (FourCC::OSG1, &outputs)]);
        let c = Container::parse(&source).unwrap();
        let words = [0x7E00_02F2u32, 0xF800_180F, 0, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 1,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
        };
        let host =
            translate_native::<false>(&c, &native, ComputeWaves::with_wave32(true), None, None)
                .unwrap();
        let c = Container::parse(&host.dxil).unwrap();
        let inputs = signature_part(&c, FourCC::ISG1).unwrap();
        let outputs = signature_part(&c, FourCC::OSG1).unwrap();
        assert_eq!((inputs[0].row, inputs[0].col), (None, 0));
        assert_eq!(outputs[0].row, Some(0));
        #[cfg(windows)]
        if let Some(mut v) = validator::Validator::for_test() {
            v.validate(&mut host.dxil.clone()).unwrap();
        }
    }

    /// `SPI_PS_INPUT_ADDR` bit 14 delivers the coverage mask in a VGPR. DXIL
    /// reads it with `dx.op.coverage.i32`, and `SV_Coverage` is not a pixel
    /// input there, so a console signature that lists it loses the element.
    #[test]
    fn pixel_sample_coverage_input_lowers_to_the_coverage_intrinsic() {
        let mut coverage = varying("SV_Coverage", 0, 0);
        coverage.kind = 14;
        coverage.comp = 5;
        coverage.row = None;
        coverage.cols = 1;
        let mut target = varying("SV_Target", 0, 3);
        target.kind = 16;
        let inputs = container::signature(&[coverage], true);
        let outputs = container::signature(&[target], false);
        let source = crate::dxbc::build(&[(FourCC::ISG1, &inputs), (FourCC::OSG1, &outputs)]);
        let c = Container::parse(&source).unwrap();
        // exp mrt0 v0, v0, v0, v0 (done); s_endpgm
        let words = [0xF800_180Fu32, 0, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 1,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 1 << 14,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
        };
        let host =
            translate_native::<false>(&c, &native, ComputeWaves::with_wave32(true), None, None)
                .unwrap();
        let c = Container::parse(&host.dxil).unwrap();
        let inputs = signature_part(&c, FourCC::ISG1).unwrap();
        assert!(inputs.iter().all(|e| e.kind != 14), "{inputs:?}");
        #[cfg(windows)]
        if let Some(mut v) = validator::Validator::for_test() {
            v.validate(&mut host.dxil.clone()).unwrap();
        }
    }

    #[test]
    fn mesh_reads_a_reused_scalar_only_after_its_guard() {
        use crate::native::MeshIo;

        let mut position = varying("SV_Position", 0, 0);
        position.kind = 3;
        let outputs = container::signature(&[position], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &outputs)]);
        let container = Container::parse(&bytes).unwrap();
        let build = |read_mask: bool| {
            let words = [
                0xD765_0000,
                0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
                0x7E0C_02F2, // v_mov_b32 v6, 1.0
                0x7D84_0081, // v_cmp_eq_u32 vcc, 1, v0
                0x020C_0CF3, // v_cndmask_b32 v6, -1.0, v6, vcc
                0x7E0E_02F2, // v_mov_b32 v7, 1.0
                0x7D84_0082, // v_cmp_eq_u32 vcc, 2, v0
                0x020E_0EF3, // v_cndmask_b32 v7, -1.0, v7, vcc
                0x7E10_02F0, // v_mov_b32 v8, 0.5
                0x7E12_02F2, // v_mov_b32 v9, 1.0
                0xF800_00CF,
                0x0908_0706,                        // exp pos0 v6, v7, v8, v9
                0x7E04_0505,                        // s2 = mesh group id
                0xBF06_8002,                        // s_cmp_eq_u32 s2, 0
                0xBF84_0003,                        // nonzero group goes to scalar assignment
                0xBE90_047E,                        // s_mov_b64 s[16:17], exec
                0xBE93_0380 | u32::from(read_mask), // s19 = 0 or 1 on the mask edge
                0xBF82_0003,                        // join
                0xBE91_03FF,
                0x0020_0400, // s17 = packed indices 0, 1, 2
                0xBE93_0381, // s19 = 1
                0xBF06_8113, // s_cmp_eq_u32 s19, 1
                0xBF84_0003, // skip export on a false guard
                0x7E02_0211, // v_mov_b32 v1, s17
                0xF800_0941,
                0x0000_0001, // exp prim v1, off, off, off done
                0xBF81_0000, // s_endpgm
            ];
            let native = NativeProgram {
                stage: HwStage::Ms,
                wave64: true,
                vgprs: 10,
                layout: vec![],
                used: vec![],
                io: StageIo::Ms(MeshIo {
                    outputs: vec![],
                    clip_cntl: 0,
                    threads: 32,
                    max_vertices: 32,
                    max_primitives: 32,
                    topology: 2,
                    lds_bytes: 0,
                }),
                code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
            };
            translate_native::<false>(
                &container,
                &native,
                ComputeWaves::with_wave32(true),
                None,
                None,
            )
        };
        let host = build(false).expect("register holds scalar data at the export");
        assert_eq!(host.stage, HwStage::Ms);
        #[cfg(windows)]
        if let Some(mut v) = validator::Validator::for_test() {
            v.validate(&mut host.dxil.clone()).unwrap();
        }
        let refusal = build(true).expect_err("the mask edge now reaches the export");
        assert!(
            matches!(
                refusal,
                ShaderError::Unsupported {
                    what: "a lane mask read as data",
                    ..
                }
            ),
            "{refusal}"
        );
        #[cfg(windows)]
        check_guarded_mesh_pixels(&host);
    }

    #[cfg(windows)]
    fn check_guarded_mesh_pixels(mesh: &HostShader) {
        use super::host::{Draw, Host};
        use windows::Win32::Graphics::Direct3D12::*;

        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped guarded mesh readback");
            return;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        // SAFETY: the feature query writes its own Options7 structure.
        let queried = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        if queried.is_err() || options.MeshShaderTier == D3D12_MESH_SHADER_TIER_NOT_SUPPORTED {
            eprintln!("no mesh shader support; skipped guarded mesh readback");
            return;
        }

        let ps = HostShader {
            dxil: constant_color(),
            ..super::host::stub_ps()
        };
        let (root, pipeline) = host.mesh_pipeline(mesh, &ps).unwrap();
        let draw = |mesh_groups| {
            host.draw(
                &root,
                &pipeline,
                &Draw {
                    roots: vec![],
                    textures: &[],
                    samplers: &[],
                    vertices: &[],
                    stride: 0,
                    size: (8, 8),
                    mesh_groups,
                },
            )
            .unwrap()
        };
        let clear = [0.0; 4];
        let color = [1.0, 0.5, 0.25, 1.0];
        // Group zero takes the mask edge and leaves its primitives culled.
        assert_eq!(draw(1), vec![clear; 64]);
        // Group one takes the scalar edge and emits the lower-left triangle.
        let pixels = draw(2);
        assert_eq!(pixels.len(), 64);
        for (i, pixel) in pixels.into_iter().enumerate() {
            let (x, y) = (i % 8, i / 8);
            if x < y {
                assert_eq!(pixel, color, "inside triangle at ({x}, {y})");
            } else if x > y {
                assert_eq!(pixel, clear, "outside triangle at ({x}, {y})");
            } else {
                assert!(pixel == clear || pixel == color, "edge at ({x}, {y})");
            }
        }
    }

    #[test]
    fn oversized_mesh_keeps_only_exports_needed_by_its_pixel_shader() {
        use crate::native::{MeshIo, Usage, VsOutput};

        let mut position = varying("SV_Position", 0, 0);
        position.kind = 3;
        let mut outputs = vec![position];
        outputs.extend((1..=8).map(|n| varying("VALUE", n, n)));
        let prims = [varying("VALUE", 9, 0), varying("VALUE", 10, 1)];
        let osg1 = container::signature(&outputs, false);
        let psg1 = container::signature(&prims, false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1), (FourCC::PSG1, &psg1)]);
        let c = Container::parse(&bytes).unwrap();

        let mut words = vec![0x7E00_02F2u32, 0xF800_00CF, 0]; // v_mov, exp pos0
        for param in 0..10 {
            words.extend([0xF800_020F | (param << 4), 0]); // exp paramN
        }
        words.push(0xBF81_0000);
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: false,
            vgprs: 1,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(MeshIo {
                outputs: (0..10)
                    .map(|param| VsOutput {
                        usage: Usage::Generic,
                        row: if param < 8 { param + 1 } else { param - 8 },
                        param: if param < 8 { param } else { param | 0x80 },
                        fp16: false,
                        pack2: false,
                    })
                    .collect(),
                clip_cntl: 0,
                threads: 32,
                max_vertices: 256,
                max_primitives: 32,
                topology: 2,
                lds_bytes: 0,
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        assert!(matches!(
            translate_native::<false>(&c, &native, ComputeWaves::with_wave32(true), None, None),
            Err(ShaderError::Unsupported {
                what: "a mesh output signature beyond the host's size limit",
                ..
            })
        ));
        // The pixel program declares all ten varyings but reads only the
        // eighth and tenth. The translated loads must follow their new IDs.
        let inputs: Vec<_> = (1..=10).map(|n| varying("value", n, n - 1)).collect();
        let mut target = varying("SV_Target", 0, 0);
        target.kind = 16;
        let isg1 = container::signature(&inputs, true);
        let target = container::signature(&[target], false);
        let pixel_bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &target)]);
        let pixel = NativeProgram {
            stage: HwStage::Ps,
            vgprs: 4,
            io: StageIo::Ps {
                inputs: [7, 9]
                    .into_iter()
                    .enumerate()
                    .map(|(attr, row)| crate::native::PsInput {
                        usage: Usage::Generic,
                        row,
                        mask: 15,
                        attr: attr as u8,
                        flat: true,
                        fp16: false,
                        pack2: false,
                        row2: row,
                    })
                    .collect(),
                input_addr: 0,
                db_shader_control: 0,
                patches: Vec::new(),
            },
            code: [
                0xC80A_0002u32, // v_interp_mov_f32 v2, p0, attr0.x
                0xC80E_0402,    // v_interp_mov_f32 v3, p0, attr1.x
                0xF800_180F,
                0x0302_0302, // exp mrt0, v2, v3, v2, v3
                0xBF81_0000,
            ]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect(),
            ..native.clone()
        };
        let ps = translate_native::<false>(
            &Container::parse(&pixel_bytes).unwrap(),
            &pixel,
            ComputeWaves::with_wave32(true),
            None,
            None,
        )
        .unwrap()
        .dxil;
        let consumer = Container::parse(&ps).unwrap();
        assert_eq!(
            signature_part(&consumer, FourCC::ISG1)
                .unwrap()
                .iter()
                .map(|e| e.index)
                .collect::<Vec<_>>(),
            [8, 10]
        );
        let linked = translate_mesh_for_pixel(&c, &native, Some(&ps))
            .unwrap()
            .unwrap();
        let host = Container::parse(&linked.dxil).unwrap();
        for (part, expected) in [(FourCC::OSG1, &[0, 8][..]), (FourCC::PSG1, &[10][..])] {
            let rows = signature_part(&host, part).unwrap();
            assert_eq!(rows.iter().map(|e| e.index).collect::<Vec<_>>(), expected);
            let raw = host.part(part).unwrap().data;
            assert!((0..rows.len()).all(|i| raw[8 + 32 * i + 25] == 0)); // Written mask.
        }

        // Standalone emission can also save space when the extra
        // declarations have no corresponding native exports. With no
        // parameter table, exp param0 is the hardware's dummy export.
        let mut position_only = native;
        let StageIo::Ms(ms) = &mut position_only.io else {
            unreachable!();
        };
        ms.outputs.clear();
        position_only.code = words[..5]
            .iter()
            .chain(words.last())
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let standalone = translate_native::<false>(
            &c,
            &position_only,
            ComputeWaves::with_wave32(true),
            None,
            None,
        )
        .unwrap();
        let host = Container::parse(&standalone.dxil).unwrap();
        assert_eq!(signature_part(&host, FourCC::OSG1).unwrap().len(), 1);
        assert!(signature_part(&host, FourCC::PSG1).unwrap().is_empty());
        #[cfg(windows)]
        if let Some(mut v) = validator::Validator::for_test() {
            let mut ps = ps;
            v.validate(&mut ps).unwrap();
            v.validate(&mut linked.dxil.clone()).unwrap();
            v.validate(&mut standalone.dxil.clone()).unwrap();
        }
    }

    /// A pixel shader writing SV_StencilRef (channel y of `exp mrtz`).
    /// DXC sets ShaderFlags.StencilRef (bit 11) for a pixel shader whose
    /// output signature has SV_StencilRef, and the feature bit
    /// D3D_SHADER_FEATURE_STENCIL_REF (0x200) in SFI0; the validator checks
    /// both against the module.
    #[test]
    fn a_stencil_export_declares_the_stencil_ref_feature() {
        let words = [
            0x7E00_02F2u32, // v_mov_b32 v0, 1.0
            0x7E02_0285,    // v_mov_b32 v1, 5
            0xF800_1883,
            0x0000_0100, // exp mrtz v0, v1, off, off done vm
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, comp| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp,
            interp: 0,
            row: None,
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        };
        let osg1 =
            container::signature(&[el("SV_Depth", 17, 9), el("SV_StencilRef", 20, 5)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        // dxil.dll, where there is one, checks the flags against the module.
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        let features = u64::from_le_bytes(sfi0.try_into().unwrap());
        assert_ne!(features & 0x200, 0, "SFI0 {features:#x}: no StencilRef");
    }

    /// A wave64 compute program that writes only EXEC's high dword
    /// (`s_mov_b32 exec_hi, 0`) before a vector instruction. The new mask is
    /// opaque and keyed by the unchanged low dword, so building its lane bit
    /// needed the mask's own value: lowering recursed until the stack
    /// overflowed, which takes the whole process down (found by
    /// random-input testing). Now the instruction fails like any other that
    /// cannot be expressed: its result is missing, and only a side effect
    /// that uses it would stop the translation.
    #[test]
    fn an_exec_high_write_does_not_overflow_the_stack() {
        // s_mov_b32 exec_hi, 0; v_mov_b32 v1, v0; s_endpgm
        let words = [0xBEFF_0380u32, 0x7E02_0300, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [64, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        if let Err(e) = emit(&c, &native, &p, &st, &io, &res) {
            assert!(e.to_string().contains("its own value"), "{e}");
        }
    }

    /// A wave32 compute program with the whole-wave section the console
    /// compiler makes of an inclusive OR prefix (`WaveMultiPrefixBitOr`
    /// over every lane, plus the lane's own value): row_shr DPP steps
    /// inside each row of 16, then lane 15 joined into the upper row. Each
    /// lane stores its result at its own offset of a raw UAV.
    fn or_prefix_cs() -> NativeProgram {
        const fn sop1(op: u32, sdst: u32, src0: u32) -> u32 {
            0xBE80_0000 | sdst << 16 | op << 8 | src0
        }
        const fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
            op << 25 | vdst << 17 | vsrc1 << 9 | src0
        }
        const fn vop3(op: u32, vdst: u32, src0: u32, src1: u32, src2: u32) -> [u32; 2] {
            [0x35 << 26 | op << 16 | vdst, src0 | src1 << 9 | src2 << 18]
        }
        /// v_or_b32_dpp v1, v1, v1 row_shr:n bound_ctrl:1.
        const fn or_shr(n: u32) -> [u32; 2] {
            [
                vop2(0x1C, 1, 0xFA, 1),
                1 | (0x110 | n) << 8 | 1 << 19 | 0xF << 24 | 0xF << 28,
            ]
        }
        let (exec_lo, v0, v1) = (126, 256, 257);
        let mut words = vec![
            sop1(0x03, 1, 0x80), // s_mov_b32 s1, 0
        ];
        // s_load_dwordx4 s[8:11], s[0:1], 0
        words.extend([0x3D << 26 | 0x02 << 18 | 8 << 6, 125 << 25]);
        words.push(sop1(0x3D, 2, 193)); // s_or_saveexec_b32 s2, -1
        words.extend(vop3(0x101, 1, 0x80, v0, 2)); // v_cndmask_b32 v1, 0, v0, s2
        for n in [1, 2, 4, 8] {
            words.extend(or_shr(n));
        }
        words.extend(vop3(0x360, 3, v1, 128 + 15, 0)); // v_readlane_b32 s3, v1, 15
        words.extend([sop1(0x03, exec_lo, 0xFF), 0xFFFF_0000]); // s_mov_b32 exec_lo, 0xffff0000
        words.push(vop2(0x1C, 1, 3, 1)); // v_or_b32 v1, s3, v1
        words.push(sop1(0x03, exec_lo, 2)); // s_mov_b32 exec_lo, s2
        words.push(vop2(0x1A, 2, 0x82, 0)); // v_lshlrev_b32 v2, 2, v0
        // buffer_store_dword v1, v2, s[8:11], 0 offen
        words.extend([
            0x38 << 26 | 0x1C << 18 | 1 << 12,
            0x80 << 24 | 2 << 16 | 1 << 8 | 2,
        ]);
        words.push(0xBF81_0000); // s_endpgm
        NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 4,
            layout: vec![crate::native::RootParam {
                root: crate::native::RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![crate::native::UsedResource {
                kind: crate::native::ResourceType::Uav,
                root: crate::native::RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 0,
                slot: 0,
            }],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 1 << 1, // one user SGPR: s0
                threads: [32, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        }
    }

    /// The OR prefix becomes `WaveMultiPrefixOp` (DXIL has no plain bitwise
    /// prefix) instead of failing the translation.
    #[test]
    fn a_bitwise_wave_prefix_is_a_multi_prefix() {
        let native = or_prefix_cs();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        let shapes: Vec<_> = (0..p.blocks.len())
            .flat_map(|b| wave::sections(&p, b).into_values())
            .flat_map(|s| s.shapes.into_values())
            .collect();
        assert!(
            shapes
                .iter()
                .any(|s| matches!(s, wave::Shape::Prefix(wave::Op::Or, _, _, 0, true))),
            "{shapes:?}"
        );
        assert!(!out.dxil.is_empty());
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
    }

    /// A pixel shader writing a constant colour.
    #[cfg(windows)]
    fn constant_color() -> Vec<u8> {
        let mut s = Shader::new(true);
        s.outputs.push(SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        });
        let b = s.entry;
        for (c, v) in [1.0f32, 0.5, 0.25, 1.0].into_iter().enumerate() {
            let args = [
                s.m.i32(0),
                s.m.i32(0),
                s.m.i8(c as u8),
                s.m.f32(v.to_bits()),
            ];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.m.terminate(b, super::bitcode::Term::Ret);
        s.finish()
    }

    #[test]
    #[cfg(windows)]
    fn stub_pipeline_is_created() {
        let Ok(host) = super::host::Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped");
            return;
        };
        let ps = super::HostShader {
            stage: crate::HwStage::Ps,
            dxil: constant_color(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
        };
        let file = |var: &str, stage| {
            std::env::var(var).ok().map(|p| super::HostShader {
                stage,
                dxil: std::fs::read(p).unwrap(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            })
        };
        let ps = file("VICTORY_REF_PS", crate::HwStage::Ps).unwrap_or(ps);
        let vs = file("VICTORY_REF_VS", crate::HwStage::Vs)
            .unwrap_or_else(|| super::host::stub_vs(&ps).unwrap());
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut vs.dxil.clone()).unwrap();
        }
        host.pipeline(&vs, &ps).unwrap();
        // vkd3d-proton does not enforce the DXIL container checksum. The
        // successful pipeline above is still exercised under Wine.
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: both names are NUL terminated, and the module is checked
        // before looking up its export. The export is never called.
        let under_wine = unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        };
        if under_wine {
            return;
        }
        // The runtime checks the container hash: no hash or a wrong one is
        // refused. (Recent runtimes also take sixteen 0x01 bytes, a bypass
        // value; ours works with older ones too.)
        let mut wrong = ps.dxil[4..20].to_vec();
        wrong[0] ^= 1;
        for digest in [&[0u8; 16][..], &wrong] {
            let mut other = ps.clone();
            other.dxil[4..20].copy_from_slice(digest);
            assert!(
                host.pipeline(&vs, &other).is_err(),
                "digest {digest:02x?} accepted"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn constant_color_validates() {
        let mut c = constant_color();
        let path = super::validator::Validator::default_path();
        let Ok(mut v) = super::validator::Validator::load(path.as_deref()) else {
            eprintln!("no dxil.dll; skipped");
            return;
        };
        let ours = c[4..20].to_vec();
        v.validate(&mut c).unwrap();
        assert_eq!(c[4..20], ours[..], "our hash differs from the validator's");
    }

    /// A pixel shader interpolating at sample positions (PERSP_SAMPLE
    /// barycentrics, `SPI_PS_INPUT_ADDR` bit 0) runs at sample frequency:
    /// DXC's PSV0 says so in PSInfo.SampleFrequency (runtime info byte 1)
    /// whenever an input is interpolated per sample.
    #[test]
    fn sample_interpolated_inputs_set_psv_sample_frequency() {
        let words = [
            0xC808_0000u32, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001,    // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
            0xBF81_0000,
        ];
        let psv_of = |input_addr: u32| {
            let native = super::NativeProgram {
                stage: super::HwStage::Ps,
                wave64: false,
                vgprs: 4,
                layout: vec![],
                used: vec![],
                io: crate::native::StageIo::Ps {
                    inputs: vec![crate::native::PsInput {
                        usage: crate::native::Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let el = |name: &str, kind, usage| SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage,
                half: false,
            };
            let isg1 = super::container::signature(&[el("TEXCOORD", 0, 0)], true);
            let osg1 = super::container::signature(&[el("SV_Target", 16, 15)], false);
            let bytes =
                crate::dxbc::build(&[(super::FourCC::ISG1, &isg1), (super::FourCC::OSG1, &osg1)]);
            let c = super::Container::parse(&bytes).unwrap();
            let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = crate::io::raise(&p, &native).unwrap();
            let out = super::emit(&c, &native, &p, &st, &io, &res).unwrap();
            // dxil.dll, where there is one, checks PSV0 against the module.
            #[cfg(windows)]
            if let Ok(mut v) = super::validator::Validator::load(
                super::validator::Validator::default_path().as_deref(),
            ) {
                v.validate(&mut out.dxil.clone()).unwrap();
            }
            let host = super::Container::parse(&out.dxil).unwrap();
            host.part(super::FourCC::PSV0).unwrap().data.to_vec()
        };
        // Runtime info starts after its size; PSInfo is {DepthOutput, SampleFrequency}.
        let center = psv_of(1 << 1);
        assert_eq!(center[4 + 1], 0, "center interpolation is per pixel");
        let sample = psv_of(1 << 0);
        assert_eq!(sample[4 + 1], 1, "PSInfo.SampleFrequency");
    }

    /// A `v_interp_p1*` that no second half completes is lowered only when
    /// nothing but second halves reads its register: a first half's result
    /// read as data (here exported) is refused, not exported as zero.
    #[test]
    fn a_first_interpolation_half_is_never_read_as_data() {
        let lower = |words: &[u32]| {
            let native = super::NativeProgram {
                stage: super::HwStage::Ps,
                wave64: false,
                vgprs: 4,
                layout: vec![],
                used: vec![],
                io: crate::native::StageIo::Ps {
                    inputs: vec![crate::native::PsInput {
                        usage: crate::native::Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr: 1 << 1,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let el = |name: &str, kind, usage| SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage,
                half: false,
            };
            let isg1 = super::container::signature(&[el("TEXCOORD", 0, 0)], true);
            let osg1 = super::container::signature(&[el("SV_Target", 16, 15)], false);
            let bytes =
                crate::dxbc::build(&[(super::FourCC::ISG1, &isg1), (super::FourCC::OSG1, &osg1)]);
            let c = super::Container::parse(&bytes).unwrap();
            let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = crate::io::raise(&p, &native).unwrap();
            super::emit(&c, &native, &p, &st, &io, &res).map(|_| ())
        };
        // exp mrt0, v2, v2, v2, v2 done vm; s_endpgm
        let export = [0xF800_180F, 0x0202_0202, 0xBF81_0000];
        // An extra first half into v3 that nothing reads still lowers.
        let unread = [&[0xC808_0000, 0xC80C_0000, 0xC809_0001][..], &export].concat();
        assert_eq!(lower(&unread), Ok(()));
        // A first half exported without its second half is refused.
        let lone = [&[0xC808_0000][..], &export].concat();
        let refused = lower(&lone).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("the first half of an interpolation"),
            "{refused}"
        );
    }

    /// A pixel shader reading channel z of an input whose element starts at
    /// w. Looking the channel up subtracted the element's first channel
    /// before checking the channel lies in it, so in a build with overflow
    /// checks (tests, debug) any read below an element that starts at z or
    /// w panicked, whichever row it was in (found by random-input testing).
    #[test]
    fn reading_a_channel_below_an_input_element_does_not_underflow() {
        // v_interp_mov_f32 v3, p0, attr0.z; s_endpgm
        let words = [0xC80E_0202u32, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1000,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let w_only = SigElement {
            name: "TEXCOORD".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 3,
            cols: 1,
            usage: 0,
            half: false,
        };
        let isg1 = super::container::signature(&[w_only], true);
        let bytes = crate::dxbc::build(&[(super::FourCC::ISG1, &isg1)]);
        let c = super::Container::parse(&bytes).unwrap();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        // Translated or refused with a reason; it must not panic.
        let _ = super::emit(&c, &native, &p, &st, &io, &res);
    }

    #[test]
    fn scalar_bitcmp_and_sext_execute_on_gpu() {
        let words: &[u32] = &[
            0xBE85_0380, // s_mov_b32 s5, 0
            0xF408_0202,
            0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
            0xBF0C_8000, // s_bitcmp0_b32 s0, 0
            0x850C_8081, // s_cselect_b32 s12, 1, 0
            0xBF0D_A100, // s_bitcmp1_b32 s0, 33
            0x850D_8081, // s_cselect_b32 s13, 1, 0
            0xBF0E_A200, // s_bitcmp0_b64 s[0:1], 34
            0x850E_8081, // s_cselect_b32 s14, 1, 0
            0xBF0F_BF00, // s_bitcmp1_b64 s[0:1], 63
            0x850F_8081, // s_cselect_b32 s15, 1, 0
            0xBF0F_BFC1, // s_bitcmp1_b64 -1, 63
            0x8510_8081, // s_cselect_b32 s16, 1, 0
            0xBE91_1902, // s_sext_i32_i8 s17, s2
            0xBE92_1A03, // s_sext_i32_i16 s18, s3
            0xBF0D_9F00, // s_bitcmp1_b32 s0, 31
            0xBE94_1903, // s_sext_i32_i8 s20, s3 (must preserve SCC)
            0x8513_8014, // s_cselect_b32 s19, s20, 0
            0x7E00_020C, // v_mov_b32 v0, s12
            0x7E02_020D, // v_mov_b32 v1, s13
            0x7E04_020E, // v_mov_b32 v2, s14
            0x7E06_020F, // v_mov_b32 v3, s15
            0x7E08_0280, // v_mov_b32 v4, 0
            0xE078_1000,
            0x8002_0004, // buffer_store_dwordx4 v[0:3], v4, s[8:11], 0 offen
            0x7E00_0210, // v_mov_b32 v0, s16
            0x7E02_0211, // v_mov_b32 v1, s17
            0x7E04_0212, // v_mov_b32 v2, s18
            0x7E06_0213, // v_mov_b32 v3, s19
            0x7E08_0290, // v_mov_b32 v4, 16
            0xE078_1000,
            0x8002_0004, // buffer_store_dwordx4 v[0:3], v4, s[8:11], 0 offen
            0xBF81_0000, // s_endpgm
        ];
        let native = crate::NativeProgram {
            stage: crate::HwStage::Cs,
            wave64: false,
            vgprs: 5,
            layout: vec![
                crate::RootParam {
                    root: crate::RootType::Constants,
                    table_offset_dwords: 0,
                    sgprs: 4,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                },
                crate::RootParam {
                    root: crate::RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 4,
                    slot: 1,
                },
            ],
            used: vec![crate::UsedResource {
                kind: crate::ResourceType::Uav,
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 4,
                slot: 1,
            }],
            io: crate::native::StageIo::Cs(crate::native::ComputeIo {
                rsrc2: 5 << 1,
                threads: [1, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        let l = super::lower::lower(&super::lower::Parts {
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
        .unwrap();
        #[cfg_attr(not(windows), allow(unused_mut, unused_variables))]
        let mut dxil = l.dxil;
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut dxil).unwrap();
        }
        #[cfg(windows)]
        if let Ok(host) = super::host::Host::new(false) {
            let cs = super::HostShader {
                stage: crate::HwStage::Cs,
                dxil,
                user_data: l.user_data,
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: l.root_cbvs,
                root_srvs: l.root_srvs,
                root_uavs: l.root_uavs,
            };
            let (root, pso) = host.compute_pipeline(&cs).unwrap();
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &super::host::Dispatch {
                        roots: vec![(
                            0,
                            super::host::Root::Constants(vec![
                                0x8000_0002,
                                0x8000_0004,
                                0x1234_5680,
                                0x1234_807F,
                                0,
                            ]),
                        )],
                        views: &[super::host::View::RawUav(64)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            assert_eq!(
                &out[0][..8],
                &[1, 1, 0, 1, 1, 0xFFFF_FF80, 0xFFFF_807F, 0x7F]
            );
        }
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use crate::native::{ComputeIo, RootParam, StageIo};

    #[test]
    fn heap_texture_atomic_widths() {
        // A synthetic compute shader loads a 2D UAV from a root table,
        // compares or adds at (0, 0), then uses the returned register(s).
        for (opcode, mask, wide) in [
            (16u32, 3, false),
            (16, 15, true),
            (17, 1, false),
            (17, 3, true),
        ] {
            let words = [
                0xBE81_0380, // s_mov_b32 s1, 0 (table address high)
                0xF40C_0200,
                0xFA00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
                0x7E00_0280,
                0x7E02_0280, // v0, v1 = coordinates
                0x7E04_0281,
                0x7E06_0282,
                0x7E08_0283,
                0x7E0A_0284, // v2..v5 = 1..4
                0xF000_2008 | opcode << 18 | mask << 8,
                0x0002_0200,
                // Use the old value in a second atomic at the same location.
                0xF044_0008 | (if wide { 3 } else { 1 }) << 8,
                0x0002_0200,
                0xBF81_0000,
            ];
            let native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 6,
                layout: vec![RootParam {
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                }],
                used: vec![],
                io: StageIo::Cs(ComputeIo {
                    rsrc2: 2,
                    threads: [32, 1, 1],
                }),
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let p = ir::build(&native.code, false, native.vgprs).unwrap();
            let bytes = crate::dxbc::build(&[]);
            let shader = emit(
                &Container::parse(&bytes).unwrap(),
                &native,
                &p,
                &structure::structure(&p).unwrap(),
                &io::raise(&p, &native).unwrap(),
                &resource::track(
                    &p,
                    &native.layout,
                    native.stage,
                    structure::structure(&p).ok().as_ref(),
                )
                .unwrap(),
            )
            .unwrap();
            let container = Container::parse(&shader.dxil).unwrap();
            let features = u64::from_le_bytes(
                container
                    .part(FourCC::SFI0)
                    .unwrap()
                    .data
                    .try_into()
                    .unwrap(),
            );
            let required = 0x1040_8000; // heap atomics, typed atomics, Int64Ops
            assert_eq!(features & required, if wide { required } else { 0 });
            #[cfg(windows)]
            if let Ok(mut v) =
                validator::Validator::load(validator::Validator::default_path().as_deref())
            {
                v.validate(&mut shader.dxil.clone()).unwrap();
            }
        }
    }
}
