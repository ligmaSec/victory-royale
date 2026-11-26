//! Draws on the host GPU (decision 0007, step 3). A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_D16_UNORM, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_UNKNOWN,
};
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    pub(crate) address: u64,
    pub(crate) size: u32,
    pub(crate) stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

const SHADER_RESOURCE: u32 = 0x40 | 0x80;
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;

#[derive(Clone, Copy)]
struct DepthTarget {
    resource: usize,
    state: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StructuredBinding {
    slot: u8,
    entry: u32,
    stride: u32,
}

/// `HostShader` does not expose per-slot DXIL binding kinds and
/// `UsedResource::stride` is console metadata that can be nonzero for raw
/// views, so structured aliasing is scoped to verified fullscreen pairs whose
/// vertex shader declares a 64-byte structured SRV at root table 0, entry 0
/// while the title binds a raw buffer SRV.
fn fullscreen_structured_alias(vertex_hash: &str, pixel_hash: &str) -> Option<StructuredBinding> {
    match (vertex_hash, pixel_hash) {
        ("f7f331c8c5b3a0d8", "3bfe730f7e24ee1a") // Bink video
        | ("80cb958cde505e15", "f9bf13c6c099ccec") // Final sRGB write
        => Some(StructuredBinding {
            slot: 0,
            entry: 0,
            stride: 64,
        }),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum RawAccess {
    Read,
    ReadWrite,
}

/// A raw UAV exposes whole dwords, and D3D12 stores the element count in a u32.
fn writable_root_bytes(remaining: u64) -> Option<u64> {
    u32::try_from(remaining / 4).ok().map(|n| u64::from(n) * 4)
}

/// A raw view describes bytes of a particular host resource. The descriptor
/// slot keeps that resource alive for the recorded commands.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RawBufferRange {
    resource: usize,
    offset: u64,
    len: u64,
}

/// Snapshot of a resolved render-target view captured at `OMSetRenderTargets`
/// bind time, so a mid-list wait split reapplies the original view even if the
/// title later overwrites the descriptor slot in place.
struct BoundRtv {
    host: Option<ID3D12Resource>,
    desc: Option<D3D12_RENDER_TARGET_VIEW_DESC>,
}

/// Snapshot of a resolved depth-stencil view captured at `OMSetRenderTargets`
/// bind time.
struct BoundDsv {
    host: ID3D12Resource,
    desc: Option<D3D12_DEPTH_STENCIL_VIEW_DESC>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FreshTableKey {
    heap: usize,
    copy_gen: u64,
    slots: u32,
    records_hash: u64,
}

struct FreshTableEntry {
    table: u64,
    start_slot: u32,
    first: u32,
    /// Sorted offsets `k - start_slot` that held a tagged stale (released) view
    /// compiled as null, so unchanged stale slots reuse the rebuilt range.
    dead_slots: Vec<u32>,
}

/// Fast-path and incremental-sync cache for [`State::bindless_textures`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct BindlessCache {
    heap: usize,
    tex_gen: u64,
    state_gen: u64,
    targets_hash: u64,
    uses_heap_srv: bool,
    uses_heap_uav: bool,
    /// Cursor into `HostList::dirty_resources` after the last bindless sync.
    dirty_cursor: usize,
    /// Active graphics RTs / depth target skipped on the last bindless sync so
    /// they are re-checked once unbound even without another intervening copy.
    skipped_targets: Vec<usize>,
    /// Whether the cached bindless pass also sampled a read-only depth target.
    depth_sampled: bool,
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    widened_targets: [u32; 8],
    depth: Option<DepthTarget>,
    /// The current draw also samples its read-only depth target.
    depth_sampled: bool,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// Fixed-function state and resolved target snapshots, cached so a
    /// mid-list wait split can reapply them onto a fresh host list without
    /// re-reading mutable descriptor slots.
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend_factor: Option<[f32; 4]>,
    stencil_ref: Option<u32>,
    bound_rtvs: Vec<BoundRtv>,
    bound_dsv: Option<BoundDsv>,
    /// What the host list has: pipeline, root signature, heaps.
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    /// Tables made afresh in this list, keyed by title heap, `list.copy_gen()`,
    /// slot count, and hashed descriptor records. Partitioning by `copy_gen`
    /// preserves `Fill -> Copy -> Fill` ordering for CBV snapshots across
    /// intervening deferred buffer copies, and hits are verified by re-reading
    /// the range and checking view liveness so no per-table record vector is
    /// allocated.
    fresh_tables: std::collections::HashMap<FreshTableKey, FreshTableEntry>,
    /// The dynamic slots the list holds: title heap, first, count; given
    /// back when the list is reset or released.
    dynamic: Vec<(usize, u32, u32)>,
    /// A fresh table of the stage being bound could not be made.
    table_failed: bool,
    /// One slot per view and access kind in each heap, including null views.
    root_view_slots: std::collections::HashMap<(usize, RawAccess, Option<RawBufferRange>), u32>,
    /// Fast-path and incremental-sync cache for [`State::bindless_textures`].
    bindless_cache: Option<BindlessCache>,
    /// Number of full-heap bindless walks performed on this list (for tests).
    #[cfg(test)]
    bindless_full_walks: u64,
}

impl Drop for State {
    fn drop(&mut self) {
        for &(heap, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// (no host pipeline) since the last call.
pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Marks the last traced draw as not run on the host.
#[cfg(feature = "diagnostics")]
fn dropped(state: &State) {
    crate::pipeline::note_drop(state.pipeline as This, Some(&state.widened_targets));
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB,
/// other views as 1 MiB (both cut at their buffer's end).
fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow
        .left(address)
        .min(if srv { 1 << 20 } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    let (gpu, to) = list.scratch(len)?;
    list.defer_fill(to, address as *const u8, len as usize);
    Some(gpu)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    #[cfg(test)]
    pub(crate) fn current_pipeline(&self) -> This {
        self.pipeline as This
    }

    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// Takes the dynamic descriptor slots held by this recording, so a
    /// pending host list can keep them alive until its submission finishes.
    pub(crate) fn take_dynamic(&mut self) -> Vec<(usize, u32, u32)> {
        // A following segment must acquire its own slots before recording a bind.
        self.fresh_tables.clear();
        self.root_view_slots.clear();
        std::mem::take(&mut self.dynamic)
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_viewports(&mut self, list: &HostList, items: &[D3D12_VIEWPORT]) {
        self.viewports = items.to_vec();
        viewports(list, &self.viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors = rects.to_vec();
        scissors(list, &self.scissors);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend_factor = factor.copied();
        blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil_ref = Some(reference);
        stencil_ref(list, reference);
    }

    /// Reapplies fixed-function state and targets onto `new_list` after a
    /// mid-list wait split, moving dynamic descriptor slots onto `prev_list`
    /// so they stay alive until its submission finishes.
    pub(crate) fn reapply_host(&mut self, new_list: &HostList, prev_list: &HostList) {
        self.bindless_cache = None;
        prev_list.keep_dynamic(self.take_dynamic());
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        if self.topology != 0 {
            self.set_topology(new_list, self.topology);
        }
        if !self.viewports.is_empty() {
            viewports(new_list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            scissors(new_list, &self.scissors);
        }
        if let Some(factor) = self.blend_factor {
            blend_factor(new_list, Some(&factor));
        }
        if let Some(reference) = self.stencil_ref {
            stencil_ref(new_list, reference);
        }
        if !self.bound_rtvs.is_empty() || self.bound_dsv.is_some() {
            self.apply_targets(new_list, false);
        }
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self
            .vertex
            .iter_mut()
            .skip(start as usize)
            .take(count as usize)
        {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// Render targets and depth, resolved at bind time and set on the host list.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        self.targets.clear();
        self.widened_targets = [0; 8];
        self.bound_rtvs.clear();
        self.depth = None;
        self.bound_dsv = None;
        self.depth_sampled = false;
        if crate::host::host().is_none() {
            return;
        }
        list.mark_work();
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            if record.resource.is_null() {
                // A typed null RTV discards writes without changing the output slot.
                if let Some(desc) = record.desc() {
                    self.bound_rtvs.push(BoundRtv {
                        host: None,
                        desc: Some(desc),
                    });
                }
                continue;
            }
            let Some(texture) = crate::resource::host_resource(record.resource) else {
                continue;
            };
            if !crate::host::renders_to(texture) {
                once!(
                    resource = format_args!("{:#x}", record.resource as usize),
                    "render target the host cannot render to; bound as a null view"
                );
                self.bound_rtvs.push(BoundRtv {
                    host: None,
                    desc: Some(crate::host::null_target()),
                });
                continue;
            }
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            let desc = record.desc();
            if let Some(format) = self.widened_targets.get_mut(self.bound_rtvs.len())
                && desc.as_ref().is_none_or(|desc| {
                    desc.Format.0 == 0 || desc.Format.0 as u32 == crate::resource::RGBA8_UNORM
                })
            {
                *format = crate::resource::widened_packed_format(record.resource).unwrap_or(0);
            }
            self.bound_rtvs.push(BoundRtv {
                host: Some(texture.clone()),
                desc,
            });
        }
        if let Some(record) = crate::descriptor::dsv_record(depth)
            && let Some(texture) = crate::resource::host_resource(record.resource)
        {
            let desc = record.depth_desc();
            self.depth = Some(DepthTarget {
                resource: record.resource as usize,
                state: depth_state(texture, desc.as_ref()),
            });
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            self.bound_dsv = Some(BoundDsv {
                host: texture.clone(),
                desc,
            });
        }
        self.apply_targets(list, true);
    }

    /// Binds the cached RTV/DSV snapshot onto `list` without re-reading the
    /// title's descriptor slots, keeping each bound host texture alive until
    /// `list` resets or drops. When `mark_work` is false (reapplying state onto
    /// a newly split trailing list after a mid-list wait), the list's COM
    /// references are retained without pinning a retirement epoch until actual
    /// GPU work is recorded.
    fn apply_targets(&self, list: &HostList, mark_work: bool) {
        let Some(host) = crate::host::host() else {
            return;
        };
        if mark_work && (!self.bound_rtvs.is_empty() || self.bound_dsv.is_some()) {
            list.mark_work();
        }
        let rtvs: Vec<D3D12_CPU_DESCRIPTOR_HANDLE> = self
            .bound_rtvs
            .iter()
            .map(|rtv| {
                if let Some(tex) = &rtv.host {
                    if mark_work {
                        list.keep(tex);
                    } else {
                        list.keep_unmarked(tex);
                    }
                }
                host.rtv(rtv.host.as_ref(), rtv.desc.as_ref())
            })
            .collect();
        let dsv = self.bound_dsv.as_ref().map(|dsv| {
            if mark_work {
                list.keep(&dsv.host);
            } else {
                list.keep_unmarked(&dsv.host);
            }
            host.dsv(&dsv.host, dsv.desc.as_ref())
        });
        // SAFETY: an open list; the views were just made from live host resources.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    fn bound_pipeline(&self, compute: bool) -> Option<std::sync::Arc<HostPipeline>> {
        crate::pipeline::host::bound_pipeline(
            self.pipeline as This,
            (!compute).then_some(&self.widened_targets),
            |binding| {
                let Root::Table(base) = *self.roots_ref(compute).get(usize::from(binding.slot))?
                else {
                    return None;
                };
                let handle = base.checked_add(u64::from(binding.offset_dwords) * 4)?;
                let heap = self.heap_of(base)?;
                if heap.samplers() {
                    return None;
                }
                heap.index(handle)?;
                let view = crate::descriptor::view(handle)?;
                crate::descriptor::host::structured_stride(&view, binding.uav)
            },
        )
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = self.bound_pipeline(false) else {
            return false;
        };
        self.prepare_graphics(list, indexed, mesh, &p)
    }

    fn prepare_graphics(
        &mut self,
        list: &HostList,
        indexed: bool,
        mesh: bool,
        p: &HostPipeline,
    ) -> bool {
        if p.compute || p.mesh != mesh {
            once!(
                mesh,
                "a draw or mesh dispatch with another kind of pipeline"
            );
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            once!("a rectangle pipeline drawn with another topology");
            return false;
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        for (slot, (view, &v)) in views.iter_mut().zip(&self.vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = crate::shadow::find(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                StrideInBytes: v.stride,
            };
        }
        // SAFETY: an open list; the views name live host buffers.
        unsafe {
            if count > 0 {
                l.IASetVertexBuffers(0, Some(&views[..count]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::shadow::find(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        self.ensure_depth_state(list);
        true
    }

    fn ensure_depth_state(&self, list: &HostList) {
        if let Some(depth) = self.depth {
            let state = depth.state
                | if self.depth_sampled {
                    SHADER_RESOURCE
                } else {
                    0
                };
            crate::list::ensure_state(list, depth.resource as This, state);
        }
    }

    fn read_only_depth(&self, compute: bool) -> Option<usize> {
        self.depth
            .filter(|depth| !compute && depth.state == DEPTH_READ)
            .map(|depth| depth.resource)
    }

    /// An internal compute pass changed the host PSO and compute root signature.
    pub(crate) fn invalidate_compute_binding(&mut self) {
        self.host_pipeline = 0;
        self.host_roots[1] = 0;
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a table it reads could
    /// not be made.
    fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        self.bind_dispatch::<false>(list, p, compute, [0; 3])
    }

    fn bind_dispatch<const THREADS: bool>(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        compute: bool,
        dims: [u32; 3],
    ) -> bool {
        list.mark_work();
        self.depth_sampled = false;
        let l = &list.list;
        let needs_root_heap = self.heaps[0] == 0
            && p.stages.iter().any(|stage| {
                !stage.root_uavs.is_empty()
                    || (!stage.root_srvs.is_empty() && crate::pipeline::host::bounded_root_srvs())
            });
        if !self.bind_heaps(list, needs_root_heap) {
            return false;
        }
        // Mirror any descriptor slots the title copied directly in heap memory
        // before bindless texture tracking or root-table binding reads them.
        for key in self.heaps {
            if key != 0 {
                crate::descriptor::heap(key as This).sync_direct_copies(true);
            }
        }
        let pixel_hash = (!compute)
            .then(|| {
                p.stages
                    .iter()
                    .find(|s| s.shader.stage == "pixel")
                    .map(|s| s.shader.hash.as_str())
            })
            .flatten();
        self.bindless_textures(list, p, compute);
        for stage in &p.stages {
            let t = stage.translated();
            self.table_failed = false;
            let alias = (stage.shader.stage == "vertex")
                .then(|| {
                    pixel_hash.and_then(|ps| fullscreen_structured_alias(&stage.shader.hash, ps))
                })
                .flatten();
            let made = self.tables_with_strides(
                list,
                &t.used,
                compute,
                alias.as_slice(),
                &p.structured_strides,
            );
            if self.table_failed {
                static DROPPED_TABLES: AtomicU64 = AtomicU64::new(0);
                let n = DROPPED_TABLES.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::warn!(
                        dropped = n,
                        "a draw or dispatch dropped: no dynamic slots for its tables"
                    );
                }
                return false;
            }
            // Preparing a placed packed view can dispatch a conversion. Bind the
            // title PSO afterwards. Compute pipelines have a single shader stage,
            // and a compute conversion leaves graphics root arguments intact.
            self.bind_shader_objects(list, p, compute);
            if let Some((param, dwords)) = stage.user_data {
                let mut values = vec![0u32; dwords as usize];
                for u in &t.host.user_data {
                    if THREADS && u.root == victory_shader::emit::DISPATCH_THREADS {
                        values[u.index as usize] = dims[u.dword as usize];
                        continue;
                    }
                    if u.indexed_buffer {
                        let selected =
                            self.user_value(compute, u.slot, RootType::Constants, u.dword);
                        let Some(binding) = self.indexed_buffer(list, selected, compute) else {
                            return false;
                        };
                        values[u.index as usize..][..3].copy_from_slice(&binding);
                        continue;
                    }
                    let fresh = made
                        .iter()
                        .find(|m| m.0 == u.slot && u.root == RootType::Table);
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // An unset table is undefined: a null view instead.
                        if tabled
                            && !self.root_buffer_table(list, None, param, compute, RawAccess::Read)
                        {
                            return false;
                        }
                        continue;
                    };
                    if tabled {
                        if !self.root_buffer_table(
                            list,
                            Some(address),
                            param,
                            compute,
                            RawAccess::Read,
                        ) {
                            return false;
                        }
                        continue;
                    }
                    let Some(gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(slot, "a shader writes a root view the title did not set");
                        None
                    }
                };
                if !self.root_buffer_table(list, address, param, compute, RawAccess::ReadWrite) {
                    return false;
                }
            }
        }
        true
    }

    fn bind_shader_objects(&mut self, list: &HostList, p: &HostPipeline, compute: bool) {
        let l = &list.list;
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = p.pso.as_raw() as usize;
            list.used_pipeline(&p.name);
            // The title may release the pipeline before it submits the list.
            list.keep(&p.pso);
            list.keep(&p.root);
        }
    }

    fn prepare_view(
        &mut self,
        list: &HostList,
        u: &UsedResource,
        view: &View,
        depth: Option<usize>,
    ) {
        let samples_depth = readable(list, u, view, depth, || self.invalidate_compute_binding());
        self.depth_sampled |= samples_depth;
    }

    /// The host heap must precede the root signature, including when switching
    /// between the renderer's view heap and either of the title's heaps.
    fn bind_heaps(&mut self, list: &HostList, needs_root_heap: bool) -> bool {
        let mut keys = self.heaps;
        if keys[0] == 0 && needs_root_heap {
            keys[0] = crate::descriptor::ROOT_VIEW_HEAP;
        }
        if keys != self.host_heaps {
            let mut heaps = Vec::new();
            for key in keys {
                if key == 0 {
                    continue;
                }
                let Some(heap) = crate::descriptor::recording_heap(key) else {
                    once!("a bound descriptor heap could not be created on the host");
                    return false;
                };
                list.keep(&heap.heap);
                heaps.push(Some(heap.heap.clone()));
            }
            // SAFETY: an open list and shader-visible heaps retained for the recording.
            unsafe { list.list.SetDescriptorHeaps(&heaps) };
            self.host_heaps = keys;
            self.host_roots = [0; 2];
        }
        true
    }

    fn raw_slot(
        &mut self,
        host: &crate::descriptor::host::HostHeap,
        heap_id: usize,
        access: RawAccess,
        view: Option<(&ID3D12Resource, u64, u64)>,
    ) -> Option<u32> {
        let range = view.map(|(buffer, offset, len)| RawBufferRange {
            resource: buffer.as_raw() as usize,
            offset,
            len,
        });
        // A title address may now resolve to a different buffer. Earlier
        // commands still reference the old slot, so allocate on a miss.
        let index = match self.root_view_slots.entry((heap_id, access, range)) {
            std::collections::hash_map::Entry::Occupied(slot) => *slot.get(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let index = host.dynamic(1)?;
                match (access, view) {
                    (RawAccess::Read, Some((buffer, offset, len))) => {
                        host.raw_at(index, buffer, offset, len)
                    }
                    (RawAccess::ReadWrite, Some((buffer, offset, len))) => {
                        host.raw_uav_at(index, buffer, offset, len)
                    }
                    (RawAccess::Read, None) => host.null_raw_at(index),
                    (RawAccess::ReadWrite, None) => host.null_raw_uav_at(index),
                }
                self.dynamic.push((heap_id, index, 1));
                *slot.insert(index)
            }
        };
        Some(index)
    }

    fn indexed_buffer(
        &mut self,
        list: &HostList,
        selected: u32,
        compute: bool,
    ) -> Option<[u32; 3]> {
        let key = self.heaps[0];
        if key == 0 {
            return None;
        }
        let heap = crate::descriptor::heap(key as This);
        let host = heap.host()?;
        // The console forms this byte offset with a 32-bit shift by five.
        let Some(read) = heap.selected_buffer(selected & (u32::MAX >> 5)) else {
            return Some([self.raw_slot(host, key, RawAccess::Read, None)? << 4, 0, 0]);
        };
        let offset = read.shadow.offset(read.address);
        let bias = offset % 16;
        let index = self.raw_slot(
            host,
            key,
            RawAccess::Read,
            Some((&read.shadow.buffer, offset - bias, read.bytes + bias)),
        )?;
        let state = if compute {
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
        } else {
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
        };
        read.shadow
            .refresh_as(list, read.address, read.bytes, state);
        // VIEW_HEAP_LIMIT leaves room for the four-bit alignment bias.
        const { assert!(victory_shader::emit::VIEW_HEAP_LIMIT <= 1 << 28) };
        Some([(index << 4) | bias as u32, read.stride, read.bytes as u32])
    }

    /// Bind a raw view of the remaining host buffer. A writable view, its
    /// initial contents, and its copy-back all cover the same whole dwords.
    fn root_buffer_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        access: RawAccess,
    ) -> bool {
        let heap_id = self.host_heaps[0];
        let Some(host) = crate::descriptor::recording_heap(heap_id) else {
            once!("a raw root view without a host heap: the draw is dropped");
            return false;
        };
        let shadow = address
            .filter(|&a| a != 0)
            .and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
        let view = if let Some((a, s)) = &shadow {
            let offset = s.offset(*a);
            if offset.is_multiple_of(16) {
                let remaining = s.left(*a);
                let len = match access {
                    RawAccess::Read => remaining,
                    RawAccess::ReadWrite => {
                        let Some(len) = writable_root_bytes(remaining) else {
                            once!(
                                remaining,
                                "writable root buffer exceeds the D3D12 element count: the draw is dropped"
                            );
                            return false;
                        };
                        len
                    }
                };
                (len >= 4).then_some((&s.buffer, offset, len))
            } else {
                once!(
                    address = format_args!("{a:#x}"),
                    "an unaligned raw root view is bound as null"
                );
                None
            }
        } else {
            None
        };
        let Some(index) = self.raw_slot(host, heap_id, access, view) else {
            return false;
        };
        if let (Some((a, s)), Some((_, _, len))) = (&shadow, view) {
            let wanted = match access {
                RawAccess::Read if compute => D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                RawAccess::Read => {
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                        | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                }
                RawAccess::ReadWrite => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            };
            match access {
                RawAccess::Read => s.refresh_as(list, *a, len.min(1 << 20), wanted),
                RawAccess::ReadWrite => {
                    s.refresh_as(list, *a, len, wanted);
                    s.gpu_writes(list, *a, len);
                }
            }
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// Transitions (and uploads title-written) live bindless textures in the
    /// bound CBV/SRV/UAV heap when any stage indexes `ResourceHeap`.
    fn bindless_textures(&mut self, list: &HostList, p: &HostPipeline, compute: bool) {
        let mut uses_heap_srv = false;
        let mut uses_heap_uav = false;
        for stage in &p.stages {
            for u in &stage.translated().used {
                if u.root == RootType::ResourceHeap {
                    match u.kind {
                        ResourceType::Srv => uses_heap_srv = true,
                        ResourceType::Uav => uses_heap_uav = true,
                        _ => {}
                    }
                }
            }
        }
        if (!uses_heap_srv && !uses_heap_uav) || self.heaps[0] == 0 {
            return;
        }
        let Some(host) = crate::descriptor::heap(self.heaps[0] as This).host() else {
            return;
        };
        // Off-element aligned copies and other special buffer views are not
        // textures; refresh them every ResourceHeap draw/dispatch so heap-
        // indexed SRVs see title writes (#553).
        host.refresh_special_buffers(list);
        let targets_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            compute.hash(&mut h);
            if !compute {
                self.targets.hash(&mut h);
                if let Some(d) = self.depth {
                    d.resource.hash(&mut h);
                    d.state.hash(&mut h);
                }
            }
            h.finish()
        };
        let cur_tex_gen = host.tex_gen();
        let cur_state_gen = list.state_gen();
        if let Some(cache) = &self.bindless_cache
            && cache.heap == self.heaps[0]
            && cache.tex_gen == cur_tex_gen
            && cache.state_gen == cur_state_gen
            && cache.targets_hash == targets_hash
            && cache.uses_heap_srv == uses_heap_srv
            && cache.uses_heap_uav == uses_heap_uav
        {
            self.depth_sampled |= cache.depth_sampled;
            return;
        }
        let active_depth = (!compute).then_some(self.depth).flatten();
        // When the view heap and bindless SRV/UAV mode are unchanged, only
        // textures whose descriptor was newly written since `cache.tex_gen`,
        // textures transitioned on `list` since the last sync, targets skipped
        // on the previous draw, or the active depth target can need transitions
        // — O(|new_slots| + |dirty|) instead of walking every resident texture.
        let incremental = self
            .bindless_cache
            .as_ref()
            .filter(|cache| {
                cache.heap == self.heaps[0]
                    && cache.uses_heap_srv == uses_heap_srv
                    && cache.uses_heap_uav == uses_heap_uav
            })
            .and_then(|cache| {
                let (_, mut candidates) = list.dirty_since(cache.dirty_cursor);
                candidates.extend_from_slice(&cache.skipped_targets);
                if let Some(d) = active_depth {
                    candidates.push(d.resource);
                }
                host.pin_live_subset_since(cache.tex_gen, &candidates, uses_heap_srv, uses_heap_uav)
            });
        let (tex_gen, live_srvs, live_uavs) = match incremental {
            Some(subset) => subset,
            None => {
                #[cfg(test)]
                {
                    self.bindless_full_walks += 1;
                }
                host.live_textures(uses_heap_srv, uses_heap_uav)
            }
        };
        let is_active_rt = |r: usize| !compute && self.targets.contains(&r);
        // When a pipeline accesses both bindless SRVs and bindless UAVs, a
        // texture with both descriptor types resident in the heap is ambiguous:
        // preserve its SRV state only if the title explicitly transitioned it
        // for shader reading (not merely because an earlier bindless sweep put
        // it in SRV state).
        let srv_set: Option<std::collections::HashSet<usize>> =
            (uses_heap_srv && uses_heap_uav && live_srvs.len() > 64)
                .then(|| live_srvs.iter().map(|(h, _)| h.get() as usize).collect());
        let in_srvs = |r: usize| {
            uses_heap_srv
                && match &srv_set {
                    Some(set) => set.contains(&r),
                    None => live_srvs.iter().any(|(h, _)| h.get() as usize == r),
                }
        };
        let mut uav_transitioned: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if uses_heap_uav {
            const UNORDERED_ACCESS: u32 = 0x8;
            for held in &live_uavs {
                let r = held.get() as usize;
                if is_active_rt(r) || active_depth.is_some_and(|d| d.resource == r) {
                    continue;
                }
                if in_srvs(r)
                    && list
                        .explicit_state(held.get())
                        .is_some_and(|s| s & SHADER_RESOURCE != 0)
                {
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                crate::capture::note_target(held.get());
                crate::list::ensure_bindless_state(list, held.get(), UNORDERED_ACCESS);
                if uses_heap_srv {
                    uav_transitioned.insert(r);
                }
            }
        }
        let mut depth_sampled = false;
        if uses_heap_srv {
            for (held, first) in &live_srvs {
                let r = held.get() as usize;
                if is_active_rt(r) || uav_transitioned.contains(&r) {
                    continue;
                }
                if let Some(d) = active_depth
                    && d.resource == r
                {
                    if d.state == DEPTH_READ {
                        depth_sampled = true;
                        crate::list::ensure_bindless_state(
                            list,
                            held.get(),
                            DEPTH_READ | SHADER_RESOURCE,
                        );
                    }
                    continue;
                }
                if let Some(t) = crate::resource::host_resource(held.get()) {
                    if crate::texture::title_written(t) {
                        crate::texture::upload_from(list, held.get(), t, *first);
                    }
                    crate::list::ensure_bindless_state(list, held.get(), SHADER_RESOURCE);
                }
            }
        }
        let mut skipped_targets = if compute {
            Vec::new()
        } else {
            self.targets.clone()
        };
        if let Some(d) = active_depth {
            skipped_targets.push(d.resource);
        }
        self.depth_sampled |= depth_sampled;
        self.bindless_cache = Some(BindlessCache {
            heap: self.heaps[0],
            tex_gen,
            state_gen: list.state_gen(),
            targets_hash,
            uses_heap_srv,
            uses_heap_uav,
            dirty_cursor: list.dirty_len(),
            skipped_targets,
            depth_sampled,
        });
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch_mode<const THREADS: bool>(
        &mut self,
        list: &HostList,
        x: u32,
        y: u32,
        z: u32,
    ) {
        let dims = [x, y, z];
        #[cfg(feature = "diagnostics")]
        let describe = || {
            if THREADS {
                format!("thread dispatch {x}x{y}x{z}")
            } else {
                format!("dispatch {x}x{y}x{z}")
            }
        };
        if THREADS && dims.contains(&0) {
            return;
        }
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop(self.pipeline as This, None);
            self.frame_op(&describe(), true, false);
            return;
        }
        let Some(p) = self.bound_pipeline(true) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This, None);
            #[cfg(feature = "diagnostics")]
            self.frame_op(&describe(), true, false);
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            #[cfg(feature = "diagnostics")]
            self.frame_op_why(&describe(), true, false, Some("a graphics pipeline"));
            return;
        }
        let (p, [x, y, z]) = if THREADS {
            let Some(plan) = crate::pipeline::host::thread_dispatch(self.pipeline as This, p, dims)
            else {
                DROPPED.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                self.frame_op_why(
                    &describe(),
                    true,
                    false,
                    Some("invalid size or partial compute pipeline unavailable"),
                );
                return;
            };
            plan
        } else {
            (p, dims)
        };
        if !self.bind_dispatch::<THREADS>(list, &p, true, dims) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(describe(), true));
        #[cfg(feature = "diagnostics")]
        self.frame_op(&describe(), true, true);
        // SAFETY: an open list with everything set.
        unsafe { list.list.Dispatch(x, y, z) };
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
    }

    /// In frames listed for capture, at debug level: one line per draw or
    /// dispatch with its render targets and depth (named as in
    /// `VICTORY_DUMP_TARGETS` files) and whether it ran, else why it was
    /// dropped (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn frame_op(&self, kind: &str, compute: bool, ran: bool) {
        self.frame_op_why(kind, compute, ran, None);
    }

    /// [`State::frame_op`] with the reason a drop happened before a pipeline
    /// was looked at (else the pipeline's).
    #[cfg(feature = "diagnostics")]
    fn frame_op_why(&self, kind: &str, compute: bool, ran: bool, why: Option<&str>) {
        if !tracing::enabled!(tracing::Level::DEBUG)
            || !crate::capture::listed(crate::queue::presented() + 1)
        {
            return;
        }
        let targets: Vec<String> = if compute {
            Vec::new()
        } else {
            self.targets.iter().map(|t| format!("{t:#x}")).collect()
        };
        let outcome = if ran {
            "ran".to_owned()
        } else {
            match why {
                Some(why) => format!("DROPPED: {why}"),
                None => format!(
                    "DROPPED: {}",
                    crate::pipeline::drop_reason(self.pipeline as This)
                ),
            }
        };
        let text = self.op_text(kind.to_owned(), compute);
        tracing::debug!(
            "frame {} op: pipeline {:#x} topology {} targets {targets:?} depth {:#x} -> {outcome} || {text}",
            crate::queue::presented() + 1,
            self.pipeline,
            self.topology,
            self.depth.map_or(0, |d| d.resource)
        );
    }

    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (`table` values with the view the table starts at; `const` values
    /// with the view at that heap index, if one is, since dynamic resources
    /// may index the heap with one, though most are plain numbers), the
    /// host addresses of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = self.bound_pipeline(compute) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = self.user_value(compute, u.slot, u.root, u.dword);
                let table = u.root == RootType::Table;
                let (kind, index) = if table {
                    ("table", v / 32)
                } else {
                    ("const", v)
                };
                let _ = write!(text, " u{}={v:#x} {kind}", u.index);
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let at = if table { "heap" } else { "as heap index" };
                    let _ = write!(text, " [{at} {index}: {slot}]");
                }
            }
            for &(slot, _) in stage
                .root_cbvs
                .iter()
                .chain(&stage.root_srvs)
                .chain(&stage.root_uavs)
            {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        text
    }

    /// Runs a mesh dispatch of `x` groups on the host (the title's mesh
    /// programs are one-dimensional).
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        let ran = self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x, 1, 1) };
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        #[cfg(feature = "diagnostics")]
        self.frame_op("mesh", false, ran);
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self);
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&mut self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true)
            && !host.refresh_slot(index, list)
        {
            self.prepare_view(list, u, &view, self.read_only_depth(compute));
        }
    }

    /// Gives tables with shader resources or constants slots owned by the
    /// command list. Later title descriptor writes cannot change what a
    /// recorded draw samples. Constants also get 256-byte-aligned storage,
    /// filled at submission. Identical descriptors share slots within a list.
    #[cfg(test)]
    fn tables(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        compute: bool,
        aliases: &[StructuredBinding],
    ) -> Vec<(u8, u32)> {
        self.tables_with_strides(list, used, compute, aliases, &[])
    }

    fn tables_with_strides(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        compute: bool,
        aliases: &[StructuredBinding],
        structured_strides: &[(victory_shader::emit::StructuredBinding, u32)],
    ) -> Vec<(u8, u32)> {
        let read_only_depth = self.read_only_depth(compute);
        let tables = |u: &&UsedResource| u.root == RootType::Table;
        let mut fresh: Vec<u8> = used
            .iter()
            .filter(tables)
            .filter(|u| matches!(u.kind, ResourceType::Cbv | ResourceType::Srv))
            .map(|u| u.slot)
            .chain(aliases.iter().map(|a| a.slot))
            .chain(structured_strides.iter().map(|(binding, _)| binding.slot))
            .collect();
        // Tables reaching past a host heap smaller than the title's (the
        // host's limit, 1,000,000 views on NVIDIA, 2,048 samplers anywhere)
        // are made afresh too: the title's slots there have no host slot of
        // their own.
        for u in used.iter().filter(tables) {
            if fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                fresh.push(u.slot);
            }
        }
        fresh.sort_unstable();
        fresh.dedup();
        for u in used.iter().filter(tables) {
            if !fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        let mut made = Vec::new();
        for slot in fresh {
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            let entries: Vec<&UsedResource> = used
                .iter()
                .filter(tables)
                .filter(|u| u.slot == slot)
                .collect();
            let table_aliases: Vec<StructuredBinding> =
                aliases.iter().copied().filter(|a| a.slot == slot).collect();
            let table_strides: Vec<_> = structured_strides
                .iter()
                .filter(|(binding, _)| binding.slot == slot)
                .collect();
            let Some(heap) = self.heap_of(table) else {
                self.table_failed |= !table_strides.is_empty();
                continue;
            };
            let Some(host) = heap.host() else {
                self.table_failed |= !table_strides.is_empty();
                continue;
            };
            let step_dwords: u32 = if heap.samplers() { 4 } else { 8 };
            let step_bytes: u64 = u64::from(step_dwords) * 4;
            let min_slot = entries
                .iter()
                .map(|u| u.table_offset_dwords / step_dwords)
                .chain(table_aliases.iter().map(|a| a.entry))
                .chain(
                    table_strides
                        .iter()
                        .map(|(b, _)| b.offset_dwords / step_dwords),
                )
                .min()
                .unwrap_or(0);
            let max_slot = entries
                .iter()
                .map(|u| u.table_offset_dwords / step_dwords)
                .chain(table_aliases.iter().map(|a| a.entry))
                .chain(
                    table_strides
                        .iter()
                        .map(|(b, _)| b.offset_dwords / step_dwords),
                )
                .max()
                .unwrap_or(0);
            // `UsedResource.table_offset_dwords` records the base of each static
            // descriptor or dynamic array range in this table; translated shaders
            // only add non-negative dynamic offsets (`lower.rs::heap_index`), so
            // they never index below `min_slot`. Guarantee `first >= host.count()
            // >= start_slot` so `first - start_slot` cannot underflow when a host
            // heap is capped below `min_slot`.
            let start_slot = if host.count() >= min_slot {
                min_slot
            } else {
                0
            };
            debug_assert!(
                entries
                    .iter()
                    .all(|u| u.table_offset_dwords / step_dwords >= start_slot),
                "table entry offset below start_slot"
            );
            let n = max_slot - start_slot + 1;
            let last = u64::from(max_slot)
                .checked_mul(step_bytes)
                .and_then(|bytes| table.checked_add(bytes));
            if last.and_then(|end| heap.index(end)).is_none() {
                self.table_failed |= !table_strides.is_empty();
                for u in entries {
                    self.table_entry(list, u, compute);
                }
                continue;
            }
            // Reject impossible table sizes before hashing/scanning the range
            // (checks total dynamic capacity, not current free space, so valid
            // cache hits still succeed when the dynamic region is full).
            if n > host.dynamic_capacity() {
                self.table_failed = true;
                continue;
            }
            // Stream the exact descriptor records in `start_slot..=max_slot` into
            // a hash with no allocation; hits verify word-for-word equality and
            // view liveness before reusing the compiled range.
            let heap_ptr = std::ptr::from_ref(heap) as usize;
            let records_hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for k in start_slot..=max_slot {
                    let at = table + u64::from(k) * step_bytes;
                    // SAFETY: views are 32 bytes and samplers are 16 bytes, so `[u32; 3]`
                    // (12 bytes) fits safely inside every in-bounds slot in title memory.
                    let raw = unsafe { (at as *const [u32; 3]).read_unaligned() };
                    let dead = raw[0] == crate::descriptor::VIEW_TAG
                        && crate::descriptor::view(at).is_none();
                    raw.hash(&mut h);
                    dead.hash(&mut h);
                }
                for alias in &table_aliases {
                    (alias.entry - start_slot, alias.stride).hash(&mut h);
                }
                for (binding, stride) in &table_strides {
                    (
                        binding.offset_dwords / step_dwords - start_slot,
                        binding.uav,
                        stride,
                    )
                        .hash(&mut h);
                }
                h.finish()
            };
            let key = FreshTableKey {
                heap: heap_ptr,
                copy_gen: list.copy_gen(),
                slots: n,
                records_hash,
            };
            let offset_for = |first: u32| {
                first
                    .checked_sub(start_slot)
                    .and_then(|base| base.checked_mul(step_bytes as u32))
            };
            if let Some(entry) = self.fresh_tables.get(&key) {
                let (prev_table, prev_start, first) = (entry.table, entry.start_slot, entry.first);
                let mut reuse_ok = true;
                for i in 0..n {
                    let at = table + u64::from(start_slot + i) * step_bytes;
                    // SAFETY: verified in-bounds of `heap` above.
                    let raw = unsafe { (at as *const [u32; 3]).read_unaligned() };
                    if prev_table != table || prev_start != start_slot {
                        let prev_at = prev_table + u64::from(prev_start + i) * step_bytes;
                        // SAFETY: `prev_table` + `prev_start + i` was verified
                        // in-bounds of the same live `heap` when recorded.
                        let prev_raw = unsafe { (prev_at as *const [u32; 3]).read_unaligned() };
                        if raw != prev_raw {
                            reuse_ok = false;
                            break;
                        }
                    }
                    // Compare view liveness against what was compiled into `first`:
                    // a newly released view misses and rebuilds a null slot, while
                    // an unchanged stale slot already compiled as null reuses it.
                    let dead = raw[0] == crate::descriptor::VIEW_TAG
                        && crate::descriptor::view(at).is_none();
                    if dead != entry.dead_slots.binary_search(&i).is_ok() {
                        reuse_ok = false;
                        break;
                    }
                }
                if reuse_ok {
                    let Some(byte_offset) = offset_for(first) else {
                        self.table_failed = true;
                        continue;
                    };
                    for alias in &table_aliases {
                        if !entries.iter().any(|u| {
                            u.kind != ResourceType::Cbv
                                && u.table_offset_dwords / step_dwords == alias.entry
                        }) {
                            let _ = host.refresh_slot(first + (alias.entry - start_slot), list);
                        }
                    }
                    for u in entries.iter().filter(|u| u.kind != ResourceType::Cbv) {
                        let dyn_index = first + (u.table_offset_dwords / step_dwords - start_slot);
                        if !host.refresh_slot(dyn_index, list) {
                            let handle = table + u64::from(u.table_offset_dwords) * 4;
                            if let Some(view) = crate::descriptor::view(handle) {
                                self.prepare_view(list, u, &view, read_only_depth);
                            }
                        }
                    }
                    made.push((slot, byte_offset));
                    continue;
                }
            }
            let Some(first) = host.dynamic(n) else {
                // Without its fresh table the shader would read the title's
                // slots, which for constant buffer tables hold no constant
                // view: a texture or null descriptor read as a buffer gives
                // a wild address on AMD and the device is removed (#59).
                // The draw or dispatch is dropped instead.
                self.table_failed = true;
                continue;
            };
            let Some(byte_offset) = offset_for(first) else {
                host.release(first, n);
                self.table_failed = true;
                continue;
            };
            // Compile the whole compact range before publishing it in
            // `fresh_tables` / `dynamic`: a shader may index entries between
            // `start_slot` and `max_slot` not listed in `entries` (descriptor
            // arrays), and a transient `list.scratch` failure must not leave a
            // partial range cached for subsequent draws.
            let mut complete = true;
            let mut dead_slots = Vec::new();
            for k in start_slot..=max_slot {
                let index = first + (k - start_slot);
                let at = table + u64::from(k) * step_bytes;
                let view = crate::descriptor::view(at);
                if let Some((binding, stride)) = table_strides
                    .iter()
                    .find(|(b, _)| b.offset_dwords / step_dwords == k)
                {
                    // Check the same view that is copied to the list's heap.
                    // Failure to mirror it must not bind a null of another kind.
                    complete = view.is_some_and(|view| {
                        crate::descriptor::host::structured_stride(&view, binding.uav)
                            == Some(*stride)
                            && host.make_at(index, &view)
                    });
                    if !complete {
                        once!(
                            slot,
                            "a structured buffer view could not be bound at its required stride"
                        );
                        break;
                    }
                    continue;
                }
                match view {
                    Some(View::Constant(address, size)) if address != 0 => {
                        let Some((gpu, to)) = list.scratch(u64::from(size)) else {
                            complete = false;
                            break;
                        };
                        list.defer_fill(to, address as *const u8, size as usize);
                        host.constant_at(index, gpu, size);
                    }
                    Some(view) => {
                        let aliased = table_aliases.iter().any(|alias| {
                            alias.entry == k && host.make_structured_at(index, &view, alias.stride)
                        });
                        if !aliased {
                            host.make_at(index, &view);
                        }
                    }
                    None => {
                        host.make_null(index);
                        // SAFETY: verified in-bounds of `heap` above.
                        if unsafe { (at as *const u32).read_unaligned() }
                            == crate::descriptor::VIEW_TAG
                        {
                            dead_slots.push(k - start_slot);
                        }
                        if entries
                            .iter()
                            .any(|u| u.table_offset_dwords / step_dwords == k)
                        {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            if !complete {
                host.release(first, n);
                self.table_failed = true;
                continue;
            }
            for k in start_slot..=max_slot {
                let index = first + (k - start_slot);
                if !host.refresh_slot(index, list) {
                    let at = table + u64::from(k) * step_bytes;
                    if let Some(view) = crate::descriptor::view(at)
                        && !matches!(view, View::Constant(..))
                    {
                        for u in entries
                            .iter()
                            .filter(|u| u.table_offset_dwords / step_dwords == k)
                        {
                            self.prepare_view(list, u, &view, read_only_depth);
                        }
                    }
                }
            }
            self.fresh_tables.insert(
                key,
                FreshTableEntry {
                    table,
                    start_slot,
                    first,
                    dead_slots,
                },
            );
            self.dynamic.push((heap_ptr, first, n));
            made.push((slot, byte_offset));
        }
        made
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        // Listed frames: `frame_op` logs every operation instead.
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = !listed && TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_resource(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = self.bound_pipeline(compute) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let prepared = self.prepare(list, false);
        let rects = prepared && self.needs_rect_expansion();
        let ran = prepared && (!rects || self.rectangles(list, vertices, first));
        if ran {
            // Rectangles were expanded into their own vertex buffers (their
            // count checked to fit by `rectangles`).
            let (count, from) = if rects {
                (expanded_vertices(vertices).unwrap_or(0), 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        self.frame_op("draw", false, ran);
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self);
        }
    }

    fn needs_rect_expansion(&self) -> bool {
        self.topology == RECTANGLES && self.bound_pipeline(false).is_some_and(|p| !p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = self.bound_pipeline(false) else {
            return false;
        };
        // The draw runs `count / 3 * 6` vertices, a count the host takes in
        // 32 bits.
        if expanded_vertices(count).is_none() {
            once!(count, "too many rectangle vertices");
            return false;
        }
        let rects = u64::from(count / 3);
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let stride = u64::from(v.stride);
            let source = rect_source(first, count, v.stride, v.size)
                .filter(|_| v.address != 0)
                .map(|offset| v.address + offset)
                .filter(|&from| crate::shadow::find(from, rects * 3 * stride).is_some());
            let Some(from) = source else {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            };
            // The expanded vertices' view takes its size in 32 bits.
            let Ok(span) = u32::try_from(rects * 6 * stride) else {
                once!(slot, "rectangle vertices too many to expand");
                return false;
            };
            let Some((gpu, to)) = list.scratch(u64::from(span)) else {
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, stride as usize);
            let n = rects as usize;
            list.defer_host(move || {
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        std::slice::from_raw_parts(from as *const u8, n * 3 * stride),
                        std::slice::from_raw_parts_mut(to as *mut u8, n * 6 * stride),
                    )
                };
                crate::primitive::expand_rects(src, dst, stride, &elements);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: span,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        let prepared = self.prepare(list, true);
        let needs_expansion = prepared && self.needs_rect_expansion();
        if needs_expansion {
            once!("indexed rectangles not drawn");
        }
        let ran = prepared && !needs_expansion;
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        self.frame_op("draw_indexed", false, ran);
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            dropped(self);
        }
    }
}

impl State {
    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host, with the reason in
    /// `dropped` (the caller logs it once per signature).
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
        dropped: &mut &'static str,
    ) -> bool {
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            *dropped = "skipped shader";
            crate::pipeline::note_drop(
                self.pipeline as This,
                (kind != 2).then_some(&self.widened_targets),
            );
            self.frame_op(&format!("indirect kind {kind}"), kind == 2, false);
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find(arguments, len) else {
            *dropped = "arguments outside title buffers";
            why("arguments outside title buffers");
            #[cfg(feature = "diagnostics")]
            self.frame_op_why(
                &format!("indirect kind {kind}"),
                kind == DISPATCH,
                false,
                Some("arguments outside title buffers"),
            );
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    *dropped = "count outside title buffers";
                    why("count outside title buffers");
                    #[cfg(feature = "diagnostics")]
                    self.frame_op_why(
                        &format!("indirect kind {kind}"),
                        kind == DISPATCH,
                        false,
                        Some("count outside title buffers"),
                    );
                    return false;
                }
            },
            None => None,
        };
        // Pipeline creation can still be in flight. Select the rectangle path
        // only from a finished pipeline, whose geometry stage is now known.
        let fallback = if matches!(kind, DRAW | INDEXED) && self.topology == RECTANGLES {
            let Some(p) = self.bound_pipeline(false) else {
                *dropped = "no host pipeline for the rectangle path";
                #[cfg(feature = "diagnostics")]
                crate::pipeline::note_drop(
                    self.pipeline as This,
                    (kind != 2).then_some(&self.widened_targets),
                );
                return false;
            };
            (!p.rects).then_some(p)
        } else {
            None
        };
        if let Some(p) = fallback {
            if kind == INDEXED {
                once!(shaders = %p.name, "indirect indexed rectangles not drawn");
                *dropped = "indexed rectangles not supported";
                why("indexed rectangles not supported");
                return false;
            }
            if p.compute || p.mesh {
                *dropped = "pipeline mismatch";
                why(&format!(
                    "pipeline mismatch (compute {}, mesh {}, topology {})",
                    p.compute, p.mesh, self.topology
                ));
                return false;
            }
            let Some(signature) = host_signature(DRAW, 16) else {
                *dropped = "no host signature";
                why("no host signature");
                return false;
            };
            if max == 0 {
                DRAWN.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            let prepared = match crate::rectangles::expand(
                list,
                &p,
                &self.vertex,
                (&args, arguments, stride, max),
                counter.as_ref().map(|(s, at)| (s, *at)),
            ) {
                Ok(p) => p,
                Err(reason) => {
                    *dropped = "rectangle expansion failed";
                    why(reason);
                    return false;
                }
            };
            self.host_pipeline = 0;
            self.host_roots[1] = 0;
            if !self.prepare(list, false) {
                *dropped = "prepare failed for indirect rectangles";
                why("prepare failed for indirect rectangles");
                return false;
            }
            prepared.before_draw(list);
            #[cfg(feature = "diagnostics")]
            list.note_op(|| {
                self.op_text(
                    format!("indirect rect draw max {max} at {arguments:#x}"),
                    false,
                )
            });
            // SAFETY: an open list with everything set; `prepared.args_buf` is
            // list-owned, `counter` is kept alive by refresh, signature cached.
            unsafe {
                list.list.ExecuteIndirect(
                    &signature,
                    max,
                    &prepared.args_buf,
                    0,
                    counter.as_ref().map(|(s, _)| &s.buffer),
                    counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
                )
            };
            prepared.after_draw(list);
            DRAWN.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            *dropped = "no host signature";
            why("no host signature");
            #[cfg(feature = "diagnostics")]
            self.frame_op_why(
                &format!("indirect kind {kind}"),
                kind == DISPATCH,
                false,
                Some("no host signature"),
            );
            return false;
        };
        // Clamping charges the list budget and can empty subsequent records.
        // Only record it after the target pipeline is available.
        let Some(p) = self.bound_pipeline(kind == DISPATCH) else {
            *dropped = "no host pipeline";
            #[cfg(feature = "diagnostics")]
            {
                self.frame_op("indirect", kind == DISPATCH, false);
                crate::pipeline::note_why(format!(
                    "indirect kind {kind}: {}",
                    crate::pipeline::drop_reason(self.pipeline as This)
                ));
            }
            return false;
        };
        args.refresh_as(list, arguments, len, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        // Bounded first: the clamp binds a compute pipeline of its own.
        if !crate::indirect::bound(list, &args.buffer, args.gpu(arguments), (kind, stride, max)) {
            *dropped = "not bounded (no clamp, or arguments not 4-byte aligned)";
            why("not bounded (no clamp, or arguments not 4-byte aligned)");
            #[cfg(feature = "diagnostics")]
            self.frame_op_why(
                &format!("indirect kind {kind}"),
                kind == DISPATCH,
                false,
                Some("not bounded (no clamp, or arguments not 4-byte aligned)"),
            );
            return false;
        }
        if let Some((s, at)) = &counter {
            s.refresh_as(list, *at, 4, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
        }
        self.invalidate_compute_binding();
        let ran = match kind {
            DRAW | INDEXED | MESH => self.prepare_graphics(list, kind == INDEXED, kind == MESH, &p),
            DISPATCH => p.compute && self.bind(list, &p, true),
            _ => false,
        };
        #[cfg(feature = "diagnostics")]
        self.frame_op("indirect", kind == DISPATCH, ran);
        if !ran {
            *dropped = if kind == DISPATCH {
                "prepare/bind failed (compute)"
            } else {
                "prepare/bind failed"
            };
            #[cfg(feature = "diagnostics")]
            why(&format!(
                "prepare failed (compute {}, mesh {})",
                p.compute, p.mesh
            ));
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(
                format!("indirect kind {kind} max {max} at {arguments:#x}"),
                kind == DISPATCH,
            )
        });
        // SAFETY: an open list with everything set; the buffers are host
        // copies the list keeps alive (refresh), the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &args.buffer,
                args.offset(arguments),
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once.
fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

/// The tracker uses whole resources, so both planes must be read-only
/// when the depth format carries stencil.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format == DXGI_FORMAT_UNKNOWN {
        // SAFETY: a live texture; a default format inherits its format.
        unsafe { texture.GetDesc() }.Format
    } else {
        desc.Format
    };
    let mut required = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if format != DXGI_FORMAT_D16_UNORM && format != DXGI_FORMAT_D32_FLOAT {
        required |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & required == required {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need. Returns whether the view
/// also reads the draw's bound depth target.
fn readable(
    list: &HostList,
    u: &UsedResource,
    view: &View,
    read_only_depth: Option<usize>,
    on_dispatch: impl FnOnce(),
) -> bool {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let first_mip = crate::descriptor::host::placed_first_mip(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((address, len)) = crate::descriptor::host::buffer_range(view)
        && len > 0
        && let Some(shadow) = crate::shadow::find(address, len)
    {
        if u.kind == ResourceType::Uav {
            shadow.refresh_as(list, address, len, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            shadow.gpu_writes(list, address, len);
        } else {
            shadow.refresh(list, address, len);
        }
        // A shader view off its buffer copy's elements reads its own copy of
        // just its bytes: that one is a host copy like any other, so its own
        // refresh brings it up to date from title memory (which the last
        // submission's copy-back already wrote).
        if u.kind == ResourceType::Srv
            && let Some(aligned) = crate::shadow::aligned_existing(address, len)
        {
            aligned.refresh(list, address, len);
        }
    }
    match (view, u.kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::resource::alias_of(*resource as This).is_some_and(|source| {
                crate::texture::alias(list, *resource as This, source, on_dispatch)
            });
            if !aliased
                && let Some(t) = crate::resource::host_resource(*resource as This)
                && crate::texture::title_written(t)
            {
                crate::texture::upload_from(list, *resource as This, t, first_mip);
            }
            // Keep the same read bits the depth target will request at draw time.
            let samples_depth = read_only_depth == Some(*resource);
            let state = if samples_depth {
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if let Some(t) = crate::resource::host_resource(*resource as This) {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
            samples_depth
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            false
        }
        _ => false,
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_resource(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!(
                "{}x{} f{} flags {:#x}",
                d.Width, d.Height, d.Format.0, d.Flags.0
            )
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        // A buffer has no host texture: its title range, as for a shader view.
        Some(v @ View::Unordered(r, ..)) => texture(*r as This).map_or_else(
            || format!("uav buffer {:?}", crate::descriptor::host::buffer_range(v)),
            |t| format!("uav {t}"),
        ),
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// The host vertices `vertices` rectangle-list vertices become (six per
/// rectangle), if the count fits in 32 bits.
fn expanded_vertices(vertices: u32) -> Option<u32> {
    (vertices / 3).checked_mul(6)
}

/// The byte offset of vertex `first` in a buffer of `size` bytes holding
/// `stride`-byte vertices, if vertices `first..first + count` all lie in
/// it. Computed wide: `first + count` and the byte offsets overflow 32
/// bits with the values a title may pass.
fn rect_source(first: u32, count: u32, stride: u32, size: u32) -> Option<u64> {
    let stride = u64::from(stride);
    let end = (u64::from(first) + u64::from(count)).checked_mul(stride)?;
    (stride != 0 && end <= u64::from(size)).then(|| u64::from(first) * stride)
}

#[cfg(test)]
mod tests {
    use super::{expanded_vertices, rect_source};

    #[test]
    fn writable_root_extent_covers_every_representable_whole_dword() {
        let max = u64::from(u32::MAX) * 4;
        assert_eq!(super::writable_root_bytes(3), Some(0));
        assert_eq!(
            super::writable_root_bytes((1 << 20) + 7),
            Some((1 << 20) + 4)
        );
        assert_eq!(super::writable_root_bytes(max + 3), Some(max));
        assert_eq!(super::writable_root_bytes(max + 4), None);
    }

    #[test]
    fn bounded_root_dispatch_writes_then_reads_without_a_title_heap() {
        use super::*;
        let Some(host) = crate::host::host() else {
            eprintln!("no host D3D12 device; skipped bounded root dispatch");
            return;
        };
        let pipeline = |source| {
            let bytes = crate::hlsl::compile_compute("raw root test", source).unwrap();
            // SAFETY: synthetic shader bytecode contains its matching root signature.
            unsafe {
                let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &bytes).unwrap();
                let pso: ID3D12PipelineState = host
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })
                    .unwrap();
                (root, pso)
            }
        };
        let first = pipeline(
            r#"RWByteAddressBuffer dst : register(u0);
                [RootSignature("DescriptorTable(UAV(u0))")]
                [numthreads(1,1,1)] void main() { dst.Store(1048580, dst.Load(1048580) + 22); }"#,
        );
        let second = pipeline(
            r#"RWByteAddressBuffer dst : register(u0);
                ByteAddressBuffer src : register(t0);
                [RootSignature("DescriptorTable(UAV(u0)), DescriptorTable(SRV(t0))")]
                [numthreads(1,1,1)] void main() { dst.Store(8, src.Load(1048580) + 5); }"#,
        );
        // One dword beyond the old one-MiB view.
        const WORD: usize = (1 << 20) / 4 + 1;
        struct Registration(u64);
        impl Drop for Registration {
            fn drop(&mut self) {
                crate::shadow::unregister(self.0 as usize, self.0);
            }
        }
        let register = |words: &mut [u32]| {
            let addr = words.as_mut_ptr() as u64;
            crate::shadow::register(addr as usize, addr, std::mem::size_of_val(words) as u64);
            Registration(addr)
        };
        let mut source = vec![7; WORD + 3];
        let mut destination = [0; 4];
        let src = register(&mut source);
        let dst = register(&mut destination);
        let list = host.new_list().unwrap();
        let mut state = State::default();
        assert!(state.bind_heaps(&list, true));
        assert_eq!(state.host_heaps, [crate::descriptor::ROOT_VIEW_HEAP, 0]);
        assert_eq!(state.heaps, [0, 0]);
        for ((root, pso), dst, src) in [(&first, src.0, None), (&second, dst.0, Some(src.0))] {
            // SAFETY: an open list and matching live root signature and pipeline.
            unsafe {
                list.list.SetComputeRootSignature(root);
                list.list.SetPipelineState(pso);
            }
            assert!(state.root_buffer_table(&list, Some(dst), 0, true, RawAccess::ReadWrite));
            if let Some(src) = src {
                assert!(state.root_buffer_table(&list, Some(src), 1, true, RawAccess::Read));
            }
            // SAFETY: a single workgroup with the required tables bound.
            unsafe { list.list.Dispatch(1, 1, 1) };
        }
        assert!(state.bind_heaps(&list, false));
        assert_eq!(state.host_heaps, [0, 0]);
        list.close();
        host.execute(&[&list]);
        assert_eq!(source[WORD], 29);
        assert!(
            source[..WORD]
                .iter()
                .chain(&source[WORD + 1..])
                .all(|&word| word == 7)
        );
        assert_eq!(destination, [0, 0, 34, 0]);
        eprintln!("bounded root UAV to SRV dispatch readback verified");
    }

    #[test]
    fn bounded_root_slots_belong_to_the_recording_that_allocated_them() {
        let mut state = super::State::default();
        let heap = 1;
        state.fresh_tables.insert(
            super::FreshTableKey {
                heap,
                copy_gen: 0,
                slots: 1,
                records_hash: 11,
            },
            super::FreshTableEntry {
                table: 10,
                start_slot: 0,
                first: 4,
                dead_slots: Vec::new(),
            },
        );
        state
            .root_view_slots
            .insert((heap, super::RawAccess::Read, None), 5);
        state
            .root_view_slots
            .insert((heap, super::RawAccess::ReadWrite, None), 6);
        state
            .dynamic
            .extend([(heap, 4, 1), (heap, 5, 1), (heap, 6, 1)]);

        assert_eq!(state.root_view_slots.len(), 2);
        assert_eq!(
            state.take_dynamic(),
            [(heap, 4, 1), (heap, 5, 1), (heap, 6, 1)]
        );
        assert!(state.fresh_tables.is_empty());
        assert!(state.root_view_slots.is_empty());
        assert!(state.take_dynamic().is_empty());
    }

    #[test]
    fn rectangle_vertices_inside_their_buffer() {
        assert_eq!(rect_source(0, 3, 16, 48), Some(0));
        assert_eq!(rect_source(3, 3, 16, 96), Some(48));
        assert_eq!(rect_source(3, 3, 16, 95), None);
        assert_eq!(rect_source(0, 3, 0, 48), None);
    }

    /// `first + count` past 32 bits wrapped to a small count before, which
    /// passed the bounds check and read far outside the buffer.
    #[test]
    fn rectangle_bounds_do_not_wrap() {
        assert_eq!(rect_source(u32::MAX, 3, 16, u32::MAX), None);
        assert_eq!(rect_source(u32::MAX - 1, 3, 1, u32::MAX), None);
        assert_eq!(rect_source(u32::MAX, u32::MAX, u32::MAX, u32::MAX), None);
    }

    #[test]
    fn expanded_counts_fit_32_bits() {
        assert_eq!(expanded_vertices(3), Some(6));
        assert_eq!(expanded_vertices(7), Some(12));
        assert_eq!(expanded_vertices(u32::MAX / 6 * 3), Some(u32::MAX / 6 * 6));
        assert_eq!(expanded_vertices(u32::MAX), None);
    }
}

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

/// Fully initialized `[u32; 10]` encoding of a 2D `D3D12_SHADER_RESOURCE_VIEW_DESC`,
/// avoiding uninitialized struct padding or union tail bytes when `create_srv`
/// reads the descriptor as `[u32; 10]`.
#[cfg(test)]
fn tex2d_srv_words(format: DXGI_FORMAT, mip_levels: u32) -> [u32; 10] {
    const {
        assert!(size_of::<D3D12_SHADER_RESOURCE_VIEW_DESC>() == size_of::<[u32; 10]>());
        assert!(std::mem::offset_of!(D3D12_SHADER_RESOURCE_VIEW_DESC, Format) == 0);
        assert!(std::mem::offset_of!(D3D12_SHADER_RESOURCE_VIEW_DESC, ViewDimension) == 4);
        assert!(
            std::mem::offset_of!(D3D12_SHADER_RESOURCE_VIEW_DESC, Shader4ComponentMapping) == 8
        );
        assert!(std::mem::offset_of!(D3D12_SHADER_RESOURCE_VIEW_DESC, Anonymous) == 16);
    }
    [
        format.0 as u32,
        D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32,
        D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        0,          // padding before 8-byte-aligned union
        0,          // Texture2D.MostDetailedMip
        mip_levels, // Texture2D.MipLevels
        0,          // Texture2D.PlaneSlice
        0,          // Texture2D.ResourceMinLODClamp (0.0f)
        0,          // unused union tail
        0,          // unused union tail
    ]
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn shader_table_keeps_read_only_depth_state() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, DXGI_SAMPLE_DESC,
        };

        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_D32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ..Default::default()
        };
        let resource_iid = ID3D12Resource::IID;
        let mut depth = null;
        assert_eq!(
            crate::resource::create_committed(
                null,
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32,
                null.cast(),
                (&raw const resource_iid).cast(),
                &mut depth,
            ),
            crate::object::S_OK
        );
        #[repr(C)]
        struct ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC,
            placement: [u64; 2],
        }
        let desc = ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
            placement: [0; 2],
        };
        let heap_iid = ID3D12DescriptorHeap::IID;
        let mut views = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const desc).cast(),
                (&raw const heap_iid).cast(),
                &mut views,
            ),
            crate::object::S_OK
        );
        let srv_desc = tex2d_srv_words(DXGI_FORMAT_R32_FLOAT, 1);
        let srv = crate::descriptor::heap(views).start;
        crate::descriptor::create_srv(null, depth, &srv_desc, srv);
        let used = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };

        let shader_resource = (D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
            | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)
            .0 as u32;
        let combined = D3D12_RESOURCE_STATE_DEPTH_READ.0 as u32 | shader_resource;
        for (flags, unsampled_state, shader_state, draw_state) in [
            (
                D3D12_DSV_FLAG_READ_ONLY_DEPTH,
                DEPTH_READ,
                combined,
                combined,
            ),
            (
                D3D12_DSV_FLAG_NONE,
                DEPTH_WRITE,
                shader_resource,
                DEPTH_WRITE,
            ),
        ] {
            let list = host.new_list().expect("host list");
            let desc = [
                DXGI_FORMAT_D32_FLOAT.0 as u32,
                D3D12_DSV_DIMENSION_TEXTURE2D.0 as u32,
                flags.0 as u32,
                0,
                0,
                0,
            ];
            let mut slot = [0u64; 16];
            let dsv = slot.as_mut_ptr() as u64;
            crate::descriptor::create_dsv(null, depth, &desc, dsv);
            let mut state = State::default();
            state.set_targets(&list, &[], dsv);
            state.set_heaps(&[views]);
            state.set_table(false, 0, srv);
            crate::list::ensure_state(&list, depth, D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32);
            state.ensure_depth_state(&list);
            assert!(
                list.transition(depth, unsampled_state).is_none(),
                "unsampled depth state for {flags:?}"
            );
            state.tables(&list, &[used], false, &[]);
            assert!(
                list.transition(depth, shader_state).is_none(),
                "shader-table state for {flags:?}"
            );
            state.ensure_depth_state(&list);
            assert!(
                list.transition(depth, draw_state).is_none(),
                "depth state for {flags:?}"
            );
            list.close();
            host.execute(&[&list]);
        }
        let release = |object: This| {
            crate::object::slot::<extern "system" fn(This) -> u32>(object, 0x10)(object)
        };
        assert_eq!(release(views), 0);
        assert_eq!(release(depth), 0);
    }

    #[test]
    fn reapply_host_preserves_state_clears_fresh_tables_and_keeps_dynamic_on_prev() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
        };

        let Some(host) = crate::host::host() else {
            return;
        };
        let prev_list = host.new_list().expect("prev list");
        let new_list = host.new_list().expect("new list");
        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let make_tex = |format, flags, initial_state| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let iid = ID3D12Resource::IID;
            let mut res = null;
            assert_eq!(
                crate::resource::create_committed(
                    null,
                    (&raw const heap).cast(),
                    0,
                    (&raw const desc).cast(),
                    initial_state,
                    null.cast(),
                    (&raw const iid).cast(),
                    &mut res,
                ),
                crate::object::S_OK
            );
            res
        };
        let rt = make_tex(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
        let depth = make_tex(
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32,
        );

        let mut rtv_slot = [0u64; 16];
        let rtv_handle = rtv_slot.as_mut_ptr() as u64;
        crate::descriptor::create_rtv(null, rt, null.cast(), rtv_handle);
        let mut dsv_slot = [0u64; 16];
        let dsv_handle = dsv_slot.as_mut_ptr() as u64;
        crate::descriptor::create_dsv(null, depth, null.cast(), dsv_handle);

        // Create a sampler heap so dynamic slots are constrained and measurable.
        #[repr(C)]
        struct ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC,
            placement: [u64; 2],
        }
        let desc = ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                NumDescriptors: 4096,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
            placement: [0; 2],
        };
        let heap_iid = ID3D12DescriptorHeap::IID;
        let mut sampler_heap = null;
        assert_eq!(
            crate::descriptor::create_heap(
                null,
                (&raw const desc).cast(),
                (&raw const heap_iid).cast(),
                &mut sampler_heap,
            ),
            crate::object::S_OK
        );
        let title_heap = crate::descriptor::heap(sampler_heap);
        let host_heap = title_heap.host().expect("host sampler heap");
        let dyn_slot = host_heap.dynamic(200).expect("dynamic sampler slots");

        let mut state = State::default();
        let vp = [D3D12_VIEWPORT {
            Width: 64.0,
            Height: 32.0,
            MaxDepth: 1.0,
            ..Default::default()
        }];
        let sc = [RECT {
            left: 1,
            top: 2,
            right: 64,
            bottom: 32,
        }];
        let blend = [0.25, 0.5, 0.75, 1.0];
        state.set_topology(&prev_list, 4);
        state.set_viewports(&prev_list, &vp);
        state.set_scissors(&prev_list, &sc);
        state.set_blend_factor(&prev_list, Some(&blend));
        state.set_stencil_ref(&prev_list, 9);
        state.set_targets(&prev_list, &[rtv_handle], dsv_handle);
        let host_rt = crate::resource::host_resource(rt).expect("host rt").clone();
        let host_depth = crate::resource::host_resource(depth)
            .expect("host depth")
            .clone();
        // Overwrite the title's RTV/DSV descriptor slots in place before the
        // mid-list wait split; `reapply_host` must restore the views captured
        // at bind time rather than re-reading the mutated slots.
        rtv_slot.fill(0);
        dsv_slot.fill(0);
        state.host_pipeline = 0x1234;
        state.host_roots = [0x11, 0x22];
        state.host_heaps = [0x33, 0x44];
        state.fresh_tables.insert(
            FreshTableKey {
                heap: 0xdead,
                copy_gen: 0,
                slots: 1,
                records_hash: 0xbeef,
            },
            FreshTableEntry {
                table: 0,
                start_slot: 0,
                first: 1,
                dead_slots: Vec::new(),
            },
        );
        state
            .dynamic
            .push((std::ptr::from_ref(title_heap) as usize, dyn_slot, 200));

        state.reapply_host(&new_list, &prev_list);

        assert!(state.fresh_tables.is_empty());
        assert!(state.dynamic.is_empty());
        assert_eq!(state.host_pipeline, 0);
        assert_eq!(state.host_roots, [0; 2]);
        assert_eq!(state.host_heaps, [0; 2]);
        assert_eq!(state.topology, 4);
        assert_eq!(state.viewports.len(), 1);
        assert_eq!(state.viewports[0].Width, 64.0);
        assert_eq!(state.scissors.len(), 1);
        assert_eq!(state.scissors[0].left, 1);
        assert_eq!(state.blend_factor, Some(blend));
        assert_eq!(state.stencil_ref, Some(9));
        assert_eq!(state.bound_rtvs.len(), 1);
        assert_eq!(state.bound_rtvs[0].host.as_ref(), Some(&host_rt));
        assert_eq!(state.bound_dsv.as_ref().map(|d| &d.host), Some(&host_depth));
        assert!(new_list.kept_contains(&host_rt));
        assert!(new_list.kept_contains(&host_depth));
        assert!(
            !new_list.has_epoch(),
            "reapply_host must not pin an epoch on an empty trailing list"
        );
        assert_eq!(state.targets, vec![rt as usize]);
        assert_eq!(state.depth.map(|d| d.resource), Some(depth as usize));

        // Dynamic slots moved onto `prev_list` stay reserved until `prev_list` drops.
        let rest = host_heap.dynamic(200);
        assert!(rest.is_none(), "kept dynamic slots should still be held");

        prev_list.close();
        new_list.close();
        host.execute(&[&prev_list, &new_list]);
        drop(prev_list);
        assert!(
            host_heap.dynamic(200).is_some(),
            "dropping prev_list must release kept dynamic slots"
        );

        let release = |object: This| {
            crate::object::slot::<extern "system" fn(This) -> u32>(object, 0x10)(object)
        };
        assert_eq!(release(sampler_heap), 0);
        assert_eq!(release(depth), 0);
        assert_eq!(release(rt), 0);
    }

    #[test]
    fn set_targets_keeps_released_rtv_and_dsv_textures_alive_until_list_reset() {
        let _retire = crate::host::isolated_retire_scope();
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
        };

        let Some(host) = crate::host::host() else {
            return;
        };
        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let resource_iid = ID3D12Resource::IID;
        let create_tex = |format, flags, initial_state| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let mut tex = null;
            assert_eq!(
                crate::resource::create_committed(
                    null,
                    (&raw const heap).cast(),
                    0,
                    (&raw const desc).cast(),
                    initial_state,
                    null.cast(),
                    (&raw const resource_iid).cast(),
                    &mut tex,
                ),
                crate::object::S_OK
            );
            tex
        };
        let release = |o: This| crate::object::slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let rtv_tex = create_tex(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
        let dsv_tex = create_tex(
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32,
        );

        let mut rtv_slot = [0u64; 16];
        let mut dsv_slot = [0u64; 16];
        let rtv_handle = rtv_slot.as_mut_ptr() as u64;
        let dsv_handle = dsv_slot.as_mut_ptr() as u64;
        crate::descriptor::create_rtv(null, rtv_tex, null.cast(), rtv_handle);
        crate::descriptor::create_dsv(null, dsv_tex, null.cast(), dsv_handle);

        let rtv_weak =
            std::sync::Arc::downgrade(&crate::resource::host_copy(rtv_tex).expect("rtv host copy"));
        let dsv_weak =
            std::sync::Arc::downgrade(&crate::resource::host_copy(dsv_tex).expect("dsv host copy"));

        let list = host.new_list().expect("host list");
        let mut state = State::default();
        let _ = host.log_messages();
        let errors_before = host.d3d12_errors();
        state.set_targets(&list, &[rtv_handle], dsv_handle);
        assert!(
            list.kept_contains(crate::resource::host_resource(rtv_tex).expect("rtv host resource")),
            "set_targets must retain the host RTV COM object directly on the list"
        );
        assert!(
            list.kept_contains(crate::resource::host_resource(dsv_tex).expect("dsv host resource")),
            "set_targets must retain the host DSV COM object directly on the list"
        );

        // Release both title textures before executing the recorded list.
        assert_eq!(release(rtv_tex), 0);
        assert_eq!(release(dsv_tex), 0);
        assert!(
            rtv_weak.strong_count() > 0,
            "set_targets must keep host RTV texture alive while list is open"
        );
        assert!(
            dsv_weak.strong_count() > 0,
            "set_targets must keep host DSV texture alive while list is open"
        );

        list.close();
        host.execute(&[&list]);
        // SAFETY: plain device-removed query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(
            host.d3d12_errors(),
            errors_before,
            "D3D12 debug layer reported errors on set_targets execution"
        );
        assert!(
            rtv_weak.strong_count() > 0,
            "host RTV texture must stay alive until list resets"
        );
        assert!(
            dsv_weak.strong_count() > 0,
            "host DSV texture must stay alive until list resets"
        );

        list.reset();
        assert_eq!(rtv_weak.strong_count(), 0);
        assert_eq!(dsv_weak.strong_count(), 0);

        // Dropping a recorded list without resetting also releases the kept
        // target textures cleanly.
        let rtv_tex2 = create_tex(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
        crate::descriptor::create_rtv(null, rtv_tex2, null.cast(), rtv_handle);
        let rtv_weak2 = std::sync::Arc::downgrade(
            &crate::resource::host_copy(rtv_tex2).expect("rtv2 host copy"),
        );
        let mut state2 = State::default();
        state2.set_targets(&list, &[rtv_handle], 0);
        assert_eq!(release(rtv_tex2), 0);
        assert!(rtv_weak2.strong_count() > 0);
        list.close();
        host.execute(&[&list]);
        assert_eq!(host.d3d12_errors(), errors_before);
        assert!(rtv_weak2.strong_count() > 0);
        drop(list);
        assert_eq!(rtv_weak2.strong_count(), 0);
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_FLOAT, DXGI_SAMPLE_DESC,
    };

    struct TitleObject(This);

    impl Drop for TitleObject {
        fn drop(&mut self) {
            let release = crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10);
            release(self.0);
        }
    }

    fn create_title_heap(kind: D3D12_DESCRIPTOR_HEAP_TYPE, count: u32) -> TitleObject {
        #[repr(C)]
        struct PlacedDesc {
            desc: D3D12_DESCRIPTOR_HEAP_DESC,
            placement: [u64; 2],
        }
        let desc = PlacedDesc {
            desc: D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
            placement: [0; 2],
        };
        let iid = ID3D12DescriptorHeap::IID;
        let mut out = std::ptr::null();
        assert_eq!(
            crate::descriptor::create_heap(
                std::ptr::null(),
                (&raw const desc).cast(),
                (&raw const iid).cast(),
                &mut out,
            ),
            crate::object::S_OK
        );
        TitleObject(out)
    }

    fn create_title_texture(
        width: u64,
        height: u32,
        format: DXGI_FORMAT,
        flags: D3D12_RESOURCE_FLAGS,
        initial_state: D3D12_RESOURCE_STATES,
    ) -> TitleObject {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: flags,
            ..Default::default()
        };
        let iid = ID3D12Resource::IID;
        let mut out = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(
                std::ptr::null(),
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                initial_state.0 as u32,
                std::ptr::null(),
                (&raw const iid).cast(),
                &mut out,
            ),
            crate::object::S_OK
        );
        TitleObject(out)
    }

    fn create_sampler_at(desc: &D3D12_SAMPLER_DESC, handle: u64) {
        const _: () = assert!(size_of::<D3D12_SAMPLER_DESC>() == size_of::<[u32; 13]>());
        // SAFETY: `D3D12_SAMPLER_DESC` is 13 32-bit words with no padding.
        let words = unsafe {
            std::ptr::from_ref(desc)
                .cast::<[u32; 13]>()
                .read_unaligned()
        };
        crate::descriptor::create_sampler(std::ptr::null(), &words, handle);
    }

    fn used_table(kind: ResourceType, slot: u8, table_slot: u32, step_dwords: u32) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: table_slot * step_dwords,
            lower_bound: 0,
            stride: 0,
            dwords: step_dwords as u8,
            user_data: 0,
            slot,
        }
    }

    fn sample_via_host_heaps(
        host: &crate::host::Host,
        view_heap: &ID3D12DescriptorHeap,
        view_slot: u32,
        sampler_heap: &ID3D12DescriptorHeap,
        sampler_slot: u32,
        uv: [f32; 2],
    ) -> [f32; 4] {
        let code = crate::hlsl::compile_compute(
            "table probe",
            include_str!("../shaders/table_probe.hlsl"),
        )
        .expect("compile sampler readback shader");
        // SAFETY: root signature and compute PSO creation on live device.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root: ID3D12RootSignature = host
                .device
                .CreateRootSignature(0, &code)
                .expect("compute root signature");
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            let pso = host
                .device
                .CreateComputePipelineState(&desc)
                .expect("compute pipeline");
            (root, pso)
        };
        let make_buffer = |kind: D3D12_HEAP_TYPE, flags, state| -> ID3D12Resource {
            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
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
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain committed buffer creation.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap_props,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut out,
                    )
                    .expect("create probe buffer");
            }
            out.expect("probe buffer")
        };
        let uav_buf = make_buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let readback = make_buffer(
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let list = host.new_list().expect("probe list");
        let l = &list.list;
        // SAFETY: live device, heaps, root signature, PSO, and buffers on an open list.
        unsafe {
            let view_step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let sampler_step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER);
            let mut view_gpu = view_heap.GetGPUDescriptorHandleForHeapStart();
            view_gpu.ptr += u64::from(view_slot) * u64::from(view_step);
            let mut sampler_gpu = sampler_heap.GetGPUDescriptorHandleForHeapStart();
            sampler_gpu.ptr += u64::from(sampler_slot) * u64::from(sampler_step);

            l.SetDescriptorHeaps(&[Some(view_heap.clone()), Some(sampler_heap.clone())]);
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            let uv_bits = [uv[0].to_bits(), uv[1].to_bits()];
            l.SetComputeRoot32BitConstants(0, 2, uv_bits.as_ptr().cast(), 0);
            l.SetComputeRootDescriptorTable(1, view_gpu);
            l.SetComputeRootDescriptorTable(2, sampler_gpu);
            l.SetComputeRootUnorderedAccessView(3, uav_buf.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[crate::host::transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            l.CopyResource(&readback, &uav_buf);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: `host.execute` completed; `readback` holds at least 16 bytes.
        unsafe {
            readback
                .Map(0, None, Some(&mut mapped))
                .expect("map probe readback");
            let val = mapped.cast::<[f32; 4]>().read();
            readback.Unmap(0, None);
            val
        }
    }

    #[test]
    fn compact_range_allocates_used_slots_only_and_mirrors_view() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let tex = create_title_texture(
            4,
            4,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let init_list = host.new_list().expect("init list");
        let host_tex = crate::resource::host_resource(tex.0).expect("host tex");
        crate::list::ensure_state(
            &init_list,
            tex.0,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
        // SAFETY: open list and live texture RTV.
        unsafe {
            init_list.list.ClearRenderTargetView(
                host.rtv(Some(host_tex), None),
                &[0.0, 1.0, 0.0, 1.0],
                None,
            );
        }
        init_list.close();
        host.execute(&[&init_list]);

        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1024);
        let samplers = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 4);
        let view_heap = crate::descriptor::heap(views.0);
        let sampler_heap = crate::descriptor::heap(samplers.0);
        let view_host = view_heap.host().expect("host view heap");
        let sampler_host = sampler_heap.host().expect("host sampler heap");

        let table = view_heap.start;
        crate::descriptor::create_srv(std::ptr::null(), tex.0, std::ptr::null(), table + 500 * 32);
        let clamp_sampler = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            ..Default::default()
        };
        create_sampler_at(&clamp_sampler, sampler_heap.start);
        assert!(sampler_host.mirror(0, sampler_heap.start, true).is_some());

        let list = host.new_list().expect("table list");
        let mut state = State::default();
        state.set_heaps(&[views.0, samplers.0]);
        state.set_table(false, 0, table);
        let used = used_table(ResourceType::Srv, 0, 500, 8);
        let made = state.tables(&list, &[used], false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        let (dyn_heap, first, n) = state.dynamic[0];
        assert_eq!(dyn_heap, std::ptr::from_ref(view_heap) as usize);
        assert_eq!(n, 1, "must allocate only 1 compact slot for slot 500");
        assert!(first >= view_host.count());
        assert_eq!(made, vec![(0, (first - 500) * 32)]);

        // Confirm HostHeap handed out exactly 1 dynamic slot.
        let next_slot = view_host.dynamic(1).expect("next dynamic slot");
        assert_eq!(next_slot, first + 1);
        view_host.release(next_slot, 1);

        // Execute `list` so `tex` transitions to SHADER_RESOURCE, then sample via `first`.
        list.close();
        host.execute(&[&list]);
        let sampled = sample_via_host_heaps(
            host,
            &view_host.heap,
            made[0].1 / 32 + 500,
            &sampler_host.heap,
            0,
            [0.5, 0.5],
        );
        assert_eq!(sampled, [0.0, 1.0, 0.0, 1.0]);

        // A direct index outside the four title slots can still reach the last
        // host slot. Its fallback sampler clamps to the green edge texel.
        let last = victory_shader::emit::SAMPLER_HEAP_LIMIT - 1;
        // SAFETY: this is the live heap about to be bound.
        assert!(last < unsafe { sampler_host.heap.GetDesc() }.NumDescriptors);
        let padded = sample_via_host_heaps(
            host,
            &view_host.heap,
            first,
            &sampler_host.heap,
            last,
            [1.25, 0.5],
        );
        assert_eq!(padded, [0.0, 1.0, 0.0, 1.0]);
        eprintln!("sampler readback matched at slots 0 and {last}");
    }

    #[test]
    fn exact_content_dedupe_shares_slots_across_distinct_table_addresses() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let depth = create_title_texture(
            4,
            4,
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        let srv_desc = tex2d_srv_words(DXGI_FORMAT_R32_FLOAT, 1);
        let dsv_desc = [
            DXGI_FORMAT_D32_FLOAT.0 as u32,
            D3D12_DSV_DIMENSION_TEXTURE2D.0 as u32,
            D3D12_DSV_FLAG_READ_ONLY_DEPTH.0 as u32,
            0,
            0,
            0,
        ];
        let mut dsv_slot = [0u64; 16];
        let dsv = dsv_slot.as_mut_ptr() as u64;
        crate::descriptor::create_dsv(std::ptr::null(), depth.0, &dsv_desc, dsv);

        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1024);
        let views_other = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 64);
        let view_heap = crate::descriptor::heap(views.0);
        let other_heap = crate::descriptor::heap(views_other.0);
        let view_host = view_heap.host().expect("host view heap");
        let other_host = other_heap.host().expect("other host view heap");

        let table1 = view_heap.start + 100 * 32;
        let table2 = view_heap.start + 300 * 32;
        let table_other = other_heap.start;
        crate::descriptor::create_srv(std::ptr::null(), depth.0, &srv_desc, table1 + 10 * 32);
        crate::descriptor::create_srv(std::ptr::null(), depth.0, &srv_desc, table2 + 25 * 32);
        crate::descriptor::create_srv(std::ptr::null(), depth.0, &srv_desc, table_other + 10 * 32);

        let list1 = host.new_list().expect("list1");
        let mut state = State::default();
        state.set_targets(&list1, &[], dsv);
        state.set_heaps(&[views.0]);
        state.set_table(false, 0, table1);

        let used1 = used_table(ResourceType::Srv, 0, 10, 8);
        let made1 = state.tables(&list1, &[used1], false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        assert_eq!(state.fresh_tables.len(), 1);
        let (_, first, n) = state.dynamic[0];
        assert_eq!(n, 1);
        assert!(first >= view_host.count());
        assert_eq!(made1, vec![(0, (first - 10) * 32)]);
        assert!(state.depth_sampled);

        // Second table at a different title address with identical descriptor content.
        let list2 = host.new_list().expect("list2");
        state.depth_sampled = false;
        state.set_table(false, 0, table2);
        let used2 = used_table(ResourceType::Srv, 0, 25, 8);
        let made2 = state.tables(&list2, &[used2], false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1, "must reuse dynamic allocation");
        assert_eq!(
            state.fresh_tables.len(),
            1,
            "must dedupe across table addresses"
        );
        assert_eq!(made2, vec![(0, (first - 25) * 32)]);
        assert!(state.depth_sampled, "cache hit must still run readable");

        let shader_resource = (D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
            | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)
            .0 as u32;
        let combined = D3D12_RESOURCE_STATE_DEPTH_READ.0 as u32 | shader_resource;
        assert!(
            list2.transition(depth.0, combined).is_none(),
            "cache hit must record depth read + shader resource transition on list2"
        );

        // Identical descriptor content in a different heap must NOT share slots across heaps.
        state.set_heaps(&[views_other.0]);
        state.set_table(false, 0, table_other);
        let made_other = state.tables(&list2, &[used1], false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 2);
        assert_eq!(state.fresh_tables.len(), 2);
        let (other_ptr, other_first, other_n) = state.dynamic[1];
        assert_eq!(other_ptr, std::ptr::from_ref(other_heap) as usize);
        assert_eq!(other_n, 1);
        assert!(other_first >= other_host.count());
        assert_eq!(made_other, vec![(0, (other_first - 10) * 32)]);

        list1.close();
        list2.close();
        host.execute(&[&list1, &list2]);
    }

    #[test]
    fn sampler_heap_rebuild_uses_16_byte_stride_for_adjacent_slots() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let tex = create_title_texture(
            2,
            1,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let init_list = host.new_list().expect("init list");
        let host_tex = crate::resource::host_resource(tex.0).expect("host tex");
        crate::list::ensure_state(
            &init_list,
            tex.0,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
        // Left pixel = Red, right pixel = Blue.
        let left = RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        };
        let right = RECT {
            left: 1,
            top: 0,
            right: 2,
            bottom: 1,
        };
        // SAFETY: open list, live texture RTV, and valid sub-rectangles.
        unsafe {
            let rtv = host.rtv(Some(host_tex), None);
            init_list
                .list
                .ClearRenderTargetView(rtv, &[1.0, 0.0, 0.0, 1.0], Some(&[left]));
            init_list
                .list
                .ClearRenderTargetView(rtv, &[0.0, 0.0, 1.0, 1.0], Some(&[right]));
        }
        crate::list::ensure_state(&init_list, tex.0, SHADER_RESOURCE);
        init_list.close();
        host.execute(&[&init_list]);

        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 4);
        let samplers = create_title_heap(
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
            4 * D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE,
        );
        let view_heap = crate::descriptor::heap(views.0);
        let sampler_heap = crate::descriptor::heap(samplers.0);
        let view_host = view_heap.host().expect("host view heap");
        let sampler_host = sampler_heap.host().expect("host sampler heap");
        assert!(sampler_host.count() < 4 * D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE);

        crate::descriptor::create_srv(std::ptr::null(), tex.0, std::ptr::null(), view_heap.start);
        assert!(view_host.mirror(0, view_heap.start, true).is_some());

        // Place table near the host sampler cap so slots 4 and 5 sit past `host.count()`.
        let table = sampler_heap.start + u64::from(sampler_host.count() - 2) * 16;
        let clamp_sampler = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            ..Default::default()
        };
        let border_green_sampler = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_BORDER,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_BORDER,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_BORDER,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            BorderColor: [0.0, 1.0, 0.0, 1.0],
            ..Default::default()
        };
        create_sampler_at(&clamp_sampler, table + 4 * 16);
        create_sampler_at(&border_green_sampler, table + 5 * 16);

        let list = host.new_list().expect("sampler table list");
        let mut state = State::default();
        state.set_heaps(&[views.0, samplers.0]);
        state.set_table(false, 1, table);
        let used = [
            used_table(ResourceType::Sampler, 1, 4, 4),
            used_table(ResourceType::Sampler, 1, 5, 4),
        ];
        let made = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        let (_, first, n) = state.dynamic[0];
        assert_eq!(n, 2, "two adjacent sampler slots");
        assert!(first >= sampler_host.count());
        assert_eq!(made, vec![(1, (first - 4) * 16)]);

        list.close();
        host.execute(&[&list]);

        let sampled_clamp = sample_via_host_heaps(
            host,
            &view_host.heap,
            0,
            &sampler_host.heap,
            made[0].1 / 16 + 4,
            [1.25, 0.5],
        );
        assert_eq!(sampled_clamp, [0.0, 0.0, 1.0, 1.0]);

        let sampled_border = sample_via_host_heaps(
            host,
            &view_host.heap,
            0,
            &sampler_host.heap,
            made[0].1 / 16 + 5,
            [1.25, 0.5],
        );
        assert_eq!(sampled_border, [0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn capped_heap_fallback_starts_at_zero_without_underflow() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let samplers = create_title_heap(
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
            4 * D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE,
        );
        let sampler_heap = crate::descriptor::heap(samplers.0);
        let sampler_host = sampler_heap.host().expect("host sampler heap");
        let cap = sampler_host.count();
        let table = sampler_heap.start;
        let desc = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
            ..Default::default()
        };
        create_sampler_at(&desc, table + u64::from(cap) * 16);
        create_sampler_at(&desc, table + u64::from(cap + 1) * 16);

        // Boundary: `min_slot == host.count()` keeps `start_slot = min_slot` with `first >= cap`.
        {
            let list = host.new_list().expect("boundary list");
            let mut state = State::default();
            state.set_heaps(&[samplers.0]);
            state.set_table(false, 0, table);
            let used = used_table(ResourceType::Sampler, 0, cap, 4);
            let made = state.tables(&list, &[used], false, &[]);
            assert!(!state.table_failed);
            assert_eq!(state.dynamic.len(), 1);
            let (_, first, n) = state.dynamic[0];
            assert_eq!(n, 1);
            assert!(first >= cap);
            assert_eq!(made, vec![(0, (first - cap) * 16)]);
            list.close();
            host.execute(&[&list]);
        }

        // Capped fallback: `host.count() < min_slot` resets `start_slot = 0` instead of
        // underflowing `first - start_slot`; `n = cap + 2` exceeds `SAMPLER_DYNAMIC`, so
        // allocation cleanly fails with `table_failed`.
        {
            let list = host.new_list().expect("fallback list");
            let mut state = State::default();
            state.set_heaps(&[samplers.0]);
            state.set_table(false, 0, table);
            let used = used_table(ResourceType::Sampler, 0, cap + 1, 4);
            let made = state.tables(&list, &[used], false, &[]);
            assert!(state.table_failed);
            assert!(made.is_empty());
            assert!(state.dynamic.is_empty());
            list.close();
            host.execute(&[&list]);
        }
    }

    #[test]
    fn multiple_used_resources_at_same_slot_all_invoke_readable() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let uav_tex = create_title_texture(
            4,
            4,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let depth = create_title_texture(
            4,
            4,
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        let dsv_desc = [
            DXGI_FORMAT_D32_FLOAT.0 as u32,
            D3D12_DSV_DIMENSION_TEXTURE2D.0 as u32,
            D3D12_DSV_FLAG_READ_ONLY_DEPTH.0 as u32,
            0,
            0,
            0,
        ];
        let mut dsv_slot = [0u64; 16];
        let dsv = dsv_slot.as_mut_ptr() as u64;
        crate::descriptor::create_dsv(std::ptr::null(), depth.0, &dsv_desc, dsv);

        let depth_srv_desc = tex2d_srv_words(DXGI_FORMAT_R32_FLOAT, 1);

        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 64);
        let view_heap = crate::descriptor::heap(views.0);
        let table0 = view_heap.start;
        let table1 = view_heap.start + 16 * 32;
        crate::descriptor::create_uav(
            std::ptr::null(),
            uav_tex.0,
            std::ptr::null(),
            std::ptr::null(),
            table0,
        );
        crate::descriptor::create_srv(std::ptr::null(), depth.0, &depth_srv_desc, table1);

        let list = host.new_list().expect("multi-used list");
        crate::list::ensure_state(&list, uav_tex.0, D3D12_RESOURCE_STATE_COMMON.0 as u32);
        let mut state = State::default();
        state.set_targets(&list, &[], dsv);
        state.set_heaps(&[views.0]);
        state.set_table(false, 0, table0);
        state.set_table(false, 1, table1);

        let used = [
            // Slot 0 has SRV before UAV: both must invoke `readable` so UAV transitions `uav_tex`.
            used_table(ResourceType::Srv, 0, 0, 8),
            used_table(ResourceType::Uav, 0, 0, 8),
            // Slot 1 has CBV before SRV: both must invoke `readable` so SRV sets `depth_sampled`.
            used_table(ResourceType::Cbv, 1, 0, 8),
            used_table(ResourceType::Srv, 1, 0, 8),
        ];
        let made = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(made.len(), 2);
        assert!(
            list.transition(uav_tex.0, D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32)
                .is_none(),
            "UAV entry sharing a table slot with an SRV entry must still transition the texture"
        );
        assert!(
            state.depth_sampled,
            "SRV entry sharing a table slot with a CBV entry must still mark depth_sampled"
        );

        list.close();
        host.execute(&[&list]);
    }

    #[test]
    fn released_view_invalidates_cached_fresh_table_and_rebuilds_fresh_range() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let tex = create_title_texture(
            4,
            4,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 64);
        let view_heap = crate::descriptor::heap(views.0);
        let table = view_heap.start;
        crate::descriptor::create_srv(std::ptr::null(), tex.0, std::ptr::null(), table);

        let list = host.new_list().expect("liveness list");
        let mut state = State::default();
        state.set_heaps(&[views.0]);
        state.set_table(false, 0, table);
        let used = [used_table(ResourceType::Srv, 0, 0, 8)];

        let made1 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        let first1 = state.dynamic[0].1;
        assert_eq!(made1, vec![(0, first1 * 32)]);

        // Release the title texture: its raw handle words stay in `table`, but
        // `descriptor::forget` bumped the view's generation so the cached range
        // must not be reused.
        drop(tex);
        let made2 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(
            state.dynamic.len(),
            2,
            "stale released view must trigger a fresh allocation while keeping the earlier range"
        );
        let first2 = state.dynamic[1].1;
        assert_ne!(first2, first1);
        assert_eq!(made2, vec![(0, first2 * 32)]);

        // Subsequent identical draws with the same unchanged stale slot must
        // reuse the rebuilt null range rather than leaking new dynamic slots.
        let made3 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(
            state.dynamic.len(),
            2,
            "unchanged stale slot must reuse its rebuilt null range"
        );
        assert_eq!(made3, made2);

        list.close();
        host.execute(&[&list]);
    }

    #[test]
    fn oversized_table_exceeding_dynamic_capacity_fails_before_allocating() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        // A capped sampler heap has 256 dynamic slots (`SAMPLER_DYNAMIC`) and
        // `host.count() == 1792`. A fresh table starting past `host.count()`
        // whose span exceeds `dynamic_capacity()` must be rejected cleanly
        // before hashing or allocating.
        let samplers = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 4096);
        let sampler_heap = crate::descriptor::heap(samplers.0);
        let sampler_host = sampler_heap.host().expect("host sampler heap");
        let cap = sampler_host.dynamic_capacity();
        assert!(cap > 0);
        let base_slot = sampler_host.count() + 8;
        let table = sampler_heap.start + u64::from(base_slot) * 16;

        let list = host.new_list().expect("oversized list");
        let mut state = State::default();
        state.set_heaps(&[std::ptr::null(), samplers.0]);
        state.set_table(false, 0, table);
        // Span `0..=cap` has `n = cap + 1 > dynamic_capacity()`.
        let used = [
            used_table(ResourceType::Sampler, 0, 0, 0),
            used_table(ResourceType::Sampler, 0, 1, cap * 4),
        ];
        let made = state.tables(&list, &used, false, &[]);
        assert!(state.table_failed);
        assert!(made.is_empty());
        assert!(state.fresh_tables.is_empty());
        assert!(state.dynamic.is_empty());
    }

    #[test]
    fn intervening_defer_copy_allocates_fresh_cbv_snapshot() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let views = create_title_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 64);
        let view_heap = crate::descriptor::heap(views.0);
        let mut cb_dst = [1u8; 256];
        let cb_src = [2u8; 256];
        let cb_addr = cb_dst.as_ptr() as u64;
        let desc = [cb_addr as u32, (cb_addr >> 32) as u32, 256, 0];

        let table1 = view_heap.start;
        let table2 = view_heap.start + 8 * 32;
        crate::descriptor::create_cbv(std::ptr::null(), &desc, table1);
        // Copy the exact CBV handle words into `table2` so both tables have
        // identical descriptor content.
        // SAFETY: both `table1` and `table2` are valid, distinct 32-byte slots in `views`.
        unsafe {
            std::ptr::copy_nonoverlapping(table1 as *const u8, table2 as *mut u8, 32);
        }

        let list = host.new_list().expect("cbv defer_copy list");
        let mut state = State::default();
        state.set_heaps(&[views.0]);
        state.set_table(false, 0, table1);
        let used = [used_table(ResourceType::Cbv, 0, 0, 0)];

        let made1 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        let first1 = state.dynamic[0].1;
        assert_eq!(made1, vec![(0, first1 * 32)]);

        // Without an intervening deferred copy, identical table content at
        // `table2` reuses `first1`.
        state.set_table(false, 0, table2);
        let made2 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 1);
        assert_eq!(made2, vec![(0, first1 * 32)]);

        // An intervening `defer_copy` (e.g. `CopyBufferRegion` into the CBV)
        // must partition the cache so the next draw allocates a fresh CBV
        // snapshot (`Fill -> Copy -> Fill`).
        list.defer_copy(cb_dst.as_mut_ptr(), cb_src.as_ptr(), cb_src.len());
        let made3 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 2);
        let first2 = state.dynamic[1].1;
        assert_ne!(first2, first1);
        assert_eq!(made3, vec![(0, first2 * 32)]);

        // Subsequent draws with no further copies reuse `first2`.
        state.set_table(false, 0, table1);
        let made4 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 2);
        assert_eq!(made4, vec![(0, first2 * 32)]);

        // An intervening `defer` (e.g. `CopyMemoryToMemoryX` writing title
        // memory) must also bump `copy_gen` and allocate a fresh CBV snapshot.
        list.defer(|| {});
        let made5 = state.tables(&list, &used, false, &[]);
        assert!(!state.table_failed);
        assert_eq!(state.dynamic.len(), 3);
        let first3 = state.dynamic[2].1;
        assert_ne!(first3, first2);
        assert_eq!(made5, vec![(0, first3 * 32)]);

        list.close();
        host.execute(&[&list]);
        assert_eq!(cb_dst, cb_src);
    }
}

#[cfg(test)]
mod bindless_tests {
    use super::*;
    use crate::pipeline::Shader;
    use crate::pipeline::host::{HostPipeline, Stage};
    use victory_shader::HwStage;
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::core::s;

    struct OwnedObject(This);

    impl Drop for OwnedObject {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let release = crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10);
                release(self.0);
            }
        }
    }

    fn make_texture(format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS) -> OwnedObject {
        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: flags,
            ..Default::default()
        };
        let iid = ID3D12Resource::IID;
        let mut out = null;
        assert_eq!(
            crate::resource::create_committed(
                null,
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                0,
                null.cast(),
                (&raw const iid).cast(),
                &mut out,
            ),
            crate::object::S_OK
        );
        OwnedObject(out)
    }

    fn make_view_heap(count: u32) -> OwnedObject {
        #[repr(C)]
        struct ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC,
            placement: [u64; 2],
        }
        let desc = ViewHeap {
            desc: D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: count,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
            placement: [0; 2],
        };
        let iid = ID3D12DescriptorHeap::IID;
        let mut out = std::ptr::null();
        assert_eq!(
            crate::descriptor::create_heap(
                std::ptr::null(),
                (&raw const desc).cast(),
                (&raw const iid).cast(),
                &mut out,
            ),
            crate::object::S_OK
        );
        OwnedObject(out)
    }

    fn compile_hlsl(
        source: &str,
        entry: windows::core::PCSTR,
        target: windows::core::PCSTR,
    ) -> ID3DBlob {
        let mut code = None;
        // SAFETY: source and entry/target strings are live; code is our output.
        unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                s!("bindless-test"),
                None,
                None,
                entry,
                target,
                0,
                0,
                &mut code,
                None,
            )
            .expect("compile test shader");
        }
        code.expect("shader blob")
    }

    fn make_test_pipelines(device: &ID3D12Device) -> (HostPipeline, HostPipeline, HostPipeline) {
        let cs_src = r#"
            [RootSignature("RootFlags(0)")]
            [numthreads(1, 1, 1)]
            void main() {}
        "#;
        let gfx_src = r#"
            [RootSignature("RootFlags(0)")]
            float4 vs(uint id : SV_VertexID) : SV_Position { return 0; }
            float4 ps() : SV_Target0 { return 1; }
        "#;
        let cs = compile_hlsl(cs_src, s!("main"), s!("cs_5_1"));
        let vs = compile_hlsl(gfx_src, s!("vs"), s!("vs_5_1"));
        let ps = compile_hlsl(gfx_src, s!("ps"), s!("ps_5_1"));

        // SAFETY: compiled blobs contain embedded root signatures and valid DXBC.
        let (cs_root, cs_pso, gfx_root, gfx_pso) = unsafe {
            let bytes = |b: &ID3DBlob| {
                std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
            };
            let bc = |b: &ID3DBlob| D3D12_SHADER_BYTECODE {
                pShaderBytecode: b.GetBufferPointer(),
                BytecodeLength: b.GetBufferSize(),
            };
            let cs_root: ID3D12RootSignature =
                device.CreateRootSignature(0, bytes(&cs)).expect("cs root");
            let cs_pso: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&cs_root),
                    CS: bc(&cs),
                    ..Default::default()
                })
                .expect("cs pso");
            let gfx_root: ID3D12RootSignature =
                device.CreateRootSignature(0, bytes(&vs)).expect("gfx root");
            let mut gfx_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&gfx_root),
                VS: bc(&vs),
                PS: bc(&ps),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            gfx_desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
            let gfx_pso: ID3D12PipelineState = device
                .CreateGraphicsPipelineState(&gfx_desc)
                .expect("gfx pso");
            (cs_root, cs_pso, gfx_root, gfx_pso)
        };

        let used_srv = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::ResourceHeap,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        let used_uav = UsedResource {
            kind: ResourceType::Uav,
            ..used_srv
        };

        let gfx_srv_uav = HostPipeline {
            compute_threads: None,
            structured_strides: Vec::new(),
            compute: false,
            mesh: false,
            rects: false,
            pso: gfx_pso.clone(),
            root: gfx_root.clone(),
            stages: vec![Stage {
                shader: Shader::for_test("pixel", HwStage::Ps, vec![used_srv, used_uav]),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            }],
            slots: 0,
            inputs: Vec::new(),
            name: "gfx_srv_uav".into(),
        };
        let gfx_srv_only = HostPipeline {
            compute_threads: None,
            structured_strides: Vec::new(),
            compute: false,
            mesh: false,
            rects: false,
            pso: gfx_pso,
            root: gfx_root,
            stages: vec![Stage {
                shader: Shader::for_test("pixel", HwStage::Ps, vec![used_srv]),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            }],
            slots: 0,
            inputs: Vec::new(),
            name: "gfx_srv_only".into(),
        };
        let compute_srv = HostPipeline {
            compute_threads: Some([1; 3]),
            structured_strides: Vec::new(),
            compute: true,
            mesh: false,
            rects: false,
            pso: cs_pso,
            root: cs_root,
            stages: vec![Stage {
                shader: Shader::for_test("compute", HwStage::Cs, vec![used_srv]),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            }],
            slots: 0,
            inputs: Vec::new(),
            name: "compute_srv".into(),
        };
        (gfx_srv_uav, gfx_srv_only, compute_srv)
    }

    #[test]
    fn bindless_textures_transitions_uploads_skips_targets_and_caches() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU
        };
        let _ = host.log_messages();
        let (gfx_srv_uav, gfx_srv_only, compute_srv) = make_test_pipelines(&host.device);

        let rt = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        );
        let depth = make_texture(
            DXGI_FORMAT_D32_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        );
        let uav_tex = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        );
        // Title-written texture (flags = 0) with non-zero bytes in title memory.
        let tw_tex = make_texture(DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_NONE);
        let (tw_mem, tw_desc) = crate::resource::texture_memory(tw_tex.0).unwrap();
        let tw_size = crate::resource::texture_layout(&tw_desc).unwrap().size as usize;
        // SAFETY: title memory for `tw_tex` holds at least `tw_size` writable bytes.
        unsafe { std::slice::from_raw_parts_mut(tw_mem.cast_mut(), tw_size).fill(0x5a) };
        let srv_tex = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        );

        let heap = make_view_heap(16);
        let start = crate::descriptor::heap(heap.0).start;
        let slot_at = |i: u64| start + i * 32;
        let null = std::ptr::null();
        crate::descriptor::create_srv(null, rt.0, null.cast(), slot_at(0));
        crate::descriptor::create_srv(null, depth.0, null.cast(), slot_at(1));
        // `uav_tex` is bound as BOTH SRV and UAV in the heap to test UAV precedence.
        crate::descriptor::create_srv(null, uav_tex.0, null.cast(), slot_at(2));
        crate::descriptor::create_uav(null, uav_tex.0, null, null.cast(), slot_at(3));
        crate::descriptor::create_srv(null, tw_tex.0, null.cast(), slot_at(4));
        crate::descriptor::create_srv(null, srv_tex.0, null.cast(), slot_at(5));
        crate::descriptor::create_uav(null, depth.0, null, null.cast(), slot_at(6));

        let list = host.new_list().expect("host list");
        let mut rtv_slot = [0u64; 16];
        let mut dsv_slot = [0u64; 16];
        crate::descriptor::create_rtv(null, rt.0, null.cast(), rtv_slot.as_mut_ptr() as u64);
        let dsv_write_desc = [
            DXGI_FORMAT_D32_FLOAT.0 as u32,
            D3D12_DSV_DIMENSION_TEXTURE2D.0 as u32,
            D3D12_DSV_FLAG_NONE.0 as u32,
            0,
            0,
            0,
        ];
        crate::descriptor::create_dsv(null, depth.0, &dsv_write_desc, dsv_slot.as_mut_ptr() as u64);

        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;

        // Put active RT in COPY_SOURCE and active depth in DEPTH_WRITE before bind.
        crate::list::ensure_state(&list, rt.0, COPY_SOURCE);
        crate::list::ensure_state(&list, depth.0, DEPTH_WRITE);

        let mut state = State::default();
        state.set_heaps(&[heap.0]);
        state.set_targets(&list, &[rtv_slot.as_ptr() as u64], dsv_slot.as_ptr() as u64);

        // 1. Graphics bind with ResourceHeap SRV + UAV and DEPTH_WRITE.
        assert!(state.bind(&list, &gfx_srv_uav, false));
        assert!(
            !state.depth_sampled,
            "DEPTH_WRITE must not mark depth_sampled"
        );
        assert!(
            list.transition(rt.0, COPY_SOURCE).is_none(),
            "active RT must be skipped"
        );
        assert!(
            list.transition(depth.0, DEPTH_WRITE).is_none(),
            "active DEPTH_WRITE target must be skipped"
        );
        assert!(
            list.transition(uav_tex.0, UNORDERED_ACCESS).is_none(),
            "UAV must take precedence over SRV when uses_heap_uav is true"
        );
        assert!(
            list.covers_texture(tw_tex.0 as usize, 0),
            "title_written SRV must be uploaded"
        );
        assert!(
            list.transition(tw_tex.0, SHADER_RESOURCE).is_none(),
            "title_written SRV must end in SHADER_RESOURCE"
        );
        assert!(list.transition(srv_tex.0, SHADER_RESOURCE).is_none());

        // 2. Consecutive identical bind hits bindless_cache without state changes.
        assert_eq!(state.bindless_full_walks, 1);
        let cached = state.bindless_cache.clone().expect("cache populated");
        let gen_before = list.state_gen();
        assert!(state.bind(&list, &gfx_srv_uav, false));
        assert_eq!(list.state_gen(), gen_before);
        assert_eq!(state.bindless_cache, Some(cached.clone()));
        assert_eq!(state.bindless_full_walks, 1);

        // 3. Intervening copy/clear bumps state_gen and uses the incremental
        // dirty-resource fast path (no full-heap walk).
        crate::list::ensure_state(&list, srv_tex.0, COPY_DEST);
        assert!(list.state_gen() > gen_before);
        assert!(state.bind(&list, &gfx_srv_uav, false));
        assert_eq!(
            state.bindless_full_walks, 1,
            "intervening state change with unchanged heap must use incremental dirty sync"
        );
        assert!(
            list.transition(srv_tex.0, SHADER_RESOURCE).is_none(),
            "incremental sync must re-transition dirtied srv_tex back to SHADER_RESOURCE"
        );
        assert_ne!(state.bindless_cache, Some(cached));

        // 3b. Writing a brand-new bindless SRV descriptor mid-list bumps
        // `tex_gen` and syncs only the newly added slot without another full walk.
        let late_srv = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        );
        crate::descriptor::create_srv(null, late_srv.0, null.cast(), slot_at(6));
        assert!(state.bind(&list, &gfx_srv_uav, false));
        assert_eq!(
            state.bindless_full_walks, 1,
            "writing a new bindless descriptor mid-list must use incremental change-ring sync"
        );
        assert_eq!(list.tracked_state(late_srv.0), Some(SHADER_RESOURCE));

        // 4. Graphics bind with DEPTH_READ sets depth_sampled and keeps it on cache hit.
        let dsv_read_desc = [
            DXGI_FORMAT_D32_FLOAT.0 as u32,
            D3D12_DSV_DIMENSION_TEXTURE2D.0 as u32,
            D3D12_DSV_FLAG_READ_ONLY_DEPTH.0 as u32,
            0,
            0,
            0,
        ];
        crate::descriptor::create_dsv(null, depth.0, &dsv_read_desc, dsv_slot.as_mut_ptr() as u64);
        state.set_targets(&list, &[rtv_slot.as_ptr() as u64], dsv_slot.as_ptr() as u64);
        assert!(state.bind(&list, &gfx_srv_only, false));
        assert!(
            state.depth_sampled,
            "DEPTH_READ in live_srvs sets depth_sampled"
        );
        assert!(
            list.transition(depth.0, DEPTH_READ | SHADER_RESOURCE)
                .is_none()
        );
        assert!(
            list.transition(uav_tex.0, SHADER_RESOURCE).is_none(),
            "when uses_heap_uav is false, uav_tex transitions to SHADER_RESOURCE"
        );
        // Second identical bind hits cache and MUST preserve `depth_sampled = true`.
        let gen_read = list.state_gen();
        assert!(state.bind(&list, &gfx_srv_only, false));
        assert_eq!(list.state_gen(), gen_read);
        assert!(
            state.depth_sampled,
            "cache hit must preserve depth_sampled for DEPTH_READ"
        );

        // 5. Compute dispatch ignores graphics RTs/depth and transitions previously
        // skipped `rt` to SRV incrementally without another full-heap walk.
        let walks_before_compute = state.bindless_full_walks;
        assert!(state.bind(&list, &compute_srv, true));
        assert_eq!(state.bindless_full_walks, walks_before_compute);
        assert!(!state.depth_sampled);
        assert!(
            list.transition(rt.0, SHADER_RESOURCE).is_none(),
            "compute dispatch must transition graphics RT to SHADER_RESOURCE"
        );

        // 6. State::reset clears bindless_cache.
        state.reset(null);
        assert!(state.bindless_cache.is_none());

        let _ = host.log_messages();
        let errors_before = host.d3d12_errors();
        list.close();
        host.execute(&[&list]);
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(host.d3d12_errors(), errors_before);
    }

    #[test]
    fn bindless_textures_uses_hashset_when_over_64_uavs_are_live() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let _ = host.log_messages();
        let (gfx_srv_uav, _, _) = make_test_pipelines(&host.device);
        const UAVS: u32 = 70;
        const SRVS: u32 = 70;
        let heap = make_view_heap(UAVS + SRVS + 8);
        let start = crate::descriptor::heap(heap.0).start;
        let slot_at = |i: u32| start + u64::from(i) * 32;
        let null = std::ptr::null();

        let mut uav_textures = Vec::with_capacity(UAVS as usize);
        for i in 0..UAVS {
            let tex = make_texture(
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            );
            crate::descriptor::create_uav(null, tex.0, null, null.cast(), slot_at(i));
            uav_textures.push(tex);
        }
        // Populate > 64 live SRVs so `live_srvs.len() > 64` exercises the
        // `HashSet` lookup branch, including an overlapping UAV+SRV texture.
        crate::descriptor::create_srv(null, uav_textures[0].0, null.cast(), slot_at(UAVS));
        let mut srv_textures = Vec::with_capacity(SRVS as usize);
        for i in 0..SRVS {
            let tex = make_texture(
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            );
            crate::descriptor::create_srv(null, tex.0, null.cast(), slot_at(UAVS + 1 + i));
            srv_textures.push(tex);
        }

        let list = host.new_list().expect("host list");
        let mut state = State::default();
        state.set_heaps(&[heap.0]);
        assert!(state.bind(&list, &gfx_srv_uav, false));

        const UNORDERED_ACCESS: u32 = 0x8;
        for tex in &uav_textures {
            assert_eq!(list.tracked_state(tex.0), Some(UNORDERED_ACCESS));
        }
        for tex in &srv_textures {
            assert_eq!(list.tracked_state(tex.0), Some(SHADER_RESOURCE));
        }
        let _ = host.log_messages();
        let errors_before = host.d3d12_errors();
        list.close();
        host.execute(&[&list]);
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(host.d3d12_errors(), errors_before);
    }

    #[test]
    fn bindless_dual_view_texture_preserves_srv_state_when_already_readable() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let _ = host.log_messages();
        let (gfx_srv_uav, gfx_srv_only, _) = make_test_pipelines(&host.device);
        let heap = make_view_heap(16);
        let start = crate::descriptor::heap(heap.0).start;
        let slot_at = |i: u32| start + u64::from(i) * 32;
        let null = std::ptr::null();

        let tex_a = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        );
        let tex_b = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        );
        // Both textures have SRV + UAV views resident in the bindless heap.
        crate::descriptor::create_srv(null, tex_a.0, null.cast(), slot_at(0));
        crate::descriptor::create_uav(null, tex_a.0, null, null.cast(), slot_at(1));
        crate::descriptor::create_srv(null, tex_b.0, null.cast(), slot_at(2));
        crate::descriptor::create_uav(null, tex_b.0, null, null.cast(), slot_at(3));

        const UNORDERED_ACCESS: u32 = 0x8;
        let list = host.new_list().expect("host list");
        // Title transitions A for SRV sampling and B for UAV writing.
        crate::list::ensure_state(&list, tex_a.0, SHADER_RESOURCE);
        crate::list::ensure_state(&list, tex_b.0, UNORDERED_ACCESS);

        let mut state = State::default();
        state.set_heaps(&[heap.0]);
        // An intervening SRV-only bindless draw temporarily sweeps B to
        // SHADER_RESOURCE without overwriting its explicit UAV access intent.
        assert!(state.bind(&list, &gfx_srv_only, false));
        assert_eq!(list.tracked_state(tex_b.0), Some(SHADER_RESOURCE));

        // A subsequent dual SRV+UAV draw must keep A in SHADER_RESOURCE
        // (explicit SRV intent) and restore B to UNORDERED_ACCESS (explicit UAV
        // intent, not masked by the intervening bindless sweep).
        assert!(state.bind(&list, &gfx_srv_uav, false));
        assert_eq!(list.tracked_state(tex_a.0), Some(SHADER_RESOURCE));
        assert_eq!(list.tracked_state(tex_b.0), Some(UNORDERED_ACCESS));

        let _ = host.log_messages();
        let errors_before = host.d3d12_errors();
        list.close();
        host.execute(&[&list]);
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(host.d3d12_errors(), errors_before);
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod diagnostics_tests {
    use super::*;

    #[test]
    fn typed_null_rtv_keeps_the_next_color_slot() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use windows::core::s;

        struct Texture(This);

        impl Drop for Texture {
            fn drop(&mut self) {
                let release = crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10);
                release(self.0);
            }
        }

        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let device = &host.device;
        let list = host.new_list().expect("host command list");
        let l = &list.list;
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let source = r#"
            [RootSignature("RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT)")]
            float4 vs(uint id : SV_VertexID) : SV_Position {
                return float4(id == 2 ? 3 : -1, id == 1 ? -3 : 1, 0, 1);
            }
            struct Colors { float4 red : SV_Target0; float4 green : SV_Target1; };
            Colors ps() {
                Colors c;
                c.red = float4(1, 0, 0, 1);
                c.green = float4(0, 1, 0, 1);
                return c;
            }
        "#;
        let compile = |entry, target| {
            let mut code = None;
            // SAFETY: source and shader names are live, and code is our output pointer.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    s!("rtv-slot-test"),
                    None,
                    None,
                    entry,
                    target,
                    0,
                    0,
                    &mut code,
                    None,
                )
                .expect("compile fixture shader");
            }
            code.expect("fixture bytecode")
        };
        let vs = compile(s!("vs"), s!("vs_5_1"));
        let ps = compile(s!("ps"), s!("ps_5_1"));
        // SAFETY: compiled fixture shaders and their embedded root signature are live.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let bytecode = |blob: &ID3DBlob| D3D12_SHADER_BYTECODE {
                pShaderBytecode: blob.GetBufferPointer(),
                BytecodeLength: blob.GetBufferSize(),
            };
            let code = bytecode(&vs);
            let root = device
                .CreateRootSignature(
                    0,
                    std::slice::from_raw_parts(code.pShaderBytecode.cast(), code.BytecodeLength),
                )
                .expect("graphics root signature");
            let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code,
                PS: bytecode(&ps),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 2,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            // D3D12 requires UNKNOWN in the PSO slot that binds a null RTV.
            desc.RTVFormats[1] = format;
            desc.BlendState.RenderTarget[0].RenderTargetWriteMask =
                D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
            let pso = device
                .CreateGraphicsPipelineState(&desc)
                .expect("graphics pipeline");
            (root, pso)
        };

        const SIDE: u32 = 4;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(SIDE),
            Height: SIDE,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut target = std::ptr::null();
        let null = std::ptr::null();
        let iid = ID3D12Resource::IID;
        assert_eq!(
            crate::resource::create_committed(
                null,
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
                null.cast(),
                (&raw const iid).cast(),
                &mut target,
            ),
            crate::object::S_OK
        );
        let _target = Texture(target);
        let color = crate::resource::host_resource(target).expect("color target");
        let mut slots = [[0u64; 16]; 2];
        let handles = slots.each_mut().map(|slot| slot.as_mut_ptr() as u64);
        let null_desc = [
            format.0 as u32,
            D3D12_RTV_DIMENSION_TEXTURE2D.0 as u32,
            0,
            0,
            0,
            0,
        ];
        crate::descriptor::create_rtv(null, null, &null_desc, handles[0]);
        crate::descriptor::create_rtv(null, target, null.cast(), handles[1]);
        crate::list::ensure_state(&list, target, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32);
        // SAFETY: open list, live target and graphics pipeline.
        unsafe {
            l.ClearRenderTargetView(host.rtv(Some(color), None), &[0.0, 0.0, 1.0, 1.0], None);
            l.SetGraphicsRootSignature(&root);
            l.SetPipelineState(&pso);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        }
        viewports(
            &list,
            &[D3D12_VIEWPORT {
                Width: SIDE as f32,
                Height: SIDE as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }],
        );
        scissors(
            &list,
            &[RECT {
                right: SIDE as i32,
                bottom: SIDE as i32,
                ..Default::default()
            }],
        );
        State::default().set_targets(&list, &handles, 0);
        // SAFETY: complete graphics state on the open list.
        unsafe { l.DrawInstanced(3, 1, 0, 0) };

        crate::list::ensure_state(&list, target, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
        let (readback, footprint) =
            crate::capture::read_back(device, l, color).expect("color readback");
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        let mut pixels = Vec::new();
        // SAFETY: execution has completed and all pixels lie in the mapped footprint.
        unsafe {
            readback
                .Map(0, None, Some(&mut mapped))
                .expect("map readback");
            for y in 0..SIDE {
                for x in 0..SIDE {
                    let at = footprint.Offset as usize
                        + (y * footprint.Footprint.RowPitch + x * 4) as usize;
                    pixels.push(mapped.cast::<u8>().add(at).cast::<[u8; 4]>().read());
                }
            }
            readback.Unmap(0, None);
        }
        assert_eq!(pixels, [[0, 255, 0, 255]; (SIDE * SIDE) as usize]);
    }
}

#[cfg(test)]
mod structured_alias_tests {
    use super::*;

    struct TitleObject(This);

    impl Drop for TitleObject {
        fn drop(&mut self) {
            let release = crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10);
            release(self.0);
        }
    }

    struct ShadowGuard {
        resource: usize,
        address: u64,
    }

    impl Drop for ShadowGuard {
        fn drop(&mut self) {
            crate::shadow::unregister(self.resource, self.address);
        }
    }

    #[test]
    fn root_constant_selects_the_buffer_stride_and_bounded_range_at_dispatch() {
        use victory_shader::native::{ComputeIo, RootParam, StageIo};
        use victory_shader::{HwStage, NativeProgram};
        let Some(host) = crate::host::host() else {
            eprintln!("skipped indexed buffer readback: no D3D12 device");
            return;
        };
        // Select an SRV via s1, read the record in s2 at byte 4 and write it to UAV slot 0.
        let words = [
            0xbe84_0300u32,
            0xbe85_0380, // heap base in s[4:5]
            0x8f06_8501, // s6 = s1 << 5
            0xf408_0202,
            0x0c00_0000, // s[8:11] = selected descriptor
            0xf408_0302,
            0xfa00_0000, // s[12:15] = output descriptor
            0x7e00_0202, // v0 = s2
            0xe034_2004,
            0x8002_0100, // buffer_load_dwordx2 v[1:2], v0, s[8:11], idxen offset:4
            0xe074_0000,
            0x8003_0100, // buffer_store_dwordx2 v[1:2], off, s[12:15]
            0xbf81_0000,
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
            vgprs: 4,
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
                NumDescriptors: 6,
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
        let out = host.default_buffer(256).unwrap();
        heap.host().unwrap().raw_uav_at(0, &out, 0, 256);
        let mut resource = [0u32; 14];
        resource[0] = 1;
        resource[4] = 256;
        // The first view starts at byte 12 and has stride 12. The second starts
        // at byte 32 and has stride 32. The last two test reads starting just
        // past a view and reads straddling its final dword.
        for (slot, stride, count) in [(1, 12, 2), (2, 32, 2), (3, 16, 1), (4, 20, 1), (5, 8, 2)] {
            let mut words = [0u32; 16];
            words[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
            words[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
            words[4] = 1; // FirstElement
            words[6] = count;
            words[7] = stride;
            words[10] = base as u32;
            words[11] = (base >> 32) as u32;
            crate::descriptor::create_placed_srv(
                std::ptr::null(),
                &resource,
                &words,
                heap.start + slot * 32,
            );
        }
        let (buffer, mapped) = host.readback_buffer(64).unwrap();
        let mut state = State::default();
        state.set_heaps(&[views]);
        let list = host.new_list().unwrap();
        // The last two addresses wrap during the multiplication or the offset addition.
        let cases = [
            (1, 1),
            (2, 1),
            (3, 1),
            (4, 1),
            (5, 1),
            (99, 1),
            (2, 0x0800_0001),
            (1, 0x1555_5555),
        ];
        for (i, (selected, record)) in cases.into_iter().enumerate() {
            state.set_constants(true, 2, &[record], 0);
            state.set_constants(true, 1, &[selected], 0);
            assert!(state.bind(&list, &pipeline, true));
            // SAFETY: the open list has live descriptors and the readback has two dwords per dispatch.
            unsafe {
                list.list.Dispatch(1, 1, 1);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &out,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list
                    .CopyBufferRegion(&buffer, 8 * i as u64, &out, 0, 8);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &out,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: the submission completed, and the mapped buffer holds eight pairs of dwords.
        let got = unsafe { mapped.cast::<[[u32; 2]; 8]>().read_unaligned() };
        assert_eq!(
            got,
            [
                [backing[7], backing[8]],
                [backing[17], backing[18]],
                [0; 2],
                [0; 2],
                [backing[5], 0],
                [0; 2],
                [0; 2],
                [0; 2]
            ]
        );
        eprintln!("indexed buffer readback: {got:x?}");
    }

    #[test]
    fn fullscreen_pairs_match_only_verified_vs_ps_combinations() {
        let expected = Some(StructuredBinding {
            slot: 0,
            entry: 0,
            stride: 64,
        });
        // Bink video
        assert_eq!(
            fullscreen_structured_alias("f7f331c8c5b3a0d8", "3bfe730f7e24ee1a"),
            expected
        );
        // Final sRGB write
        assert_eq!(
            fullscreen_structured_alias("80cb958cde505e15", "f9bf13c6c099ccec"),
            expected
        );
        // Swapped stage hashes or crossed pairs must not alias.
        assert_eq!(
            fullscreen_structured_alias("3bfe730f7e24ee1a", "f7f331c8c5b3a0d8"),
            None
        );
        assert_eq!(
            fullscreen_structured_alias("80cb958cde505e15", "3bfe730f7e24ee1a"),
            None
        );
        assert_eq!(
            fullscreen_structured_alias("f7f331c8c5b3a0d8", "f9bf13c6c099ccec"),
            None
        );
        // Other unrelated or non-scoped shaders remain untouched.
        assert_eq!(
            fullscreen_structured_alias("0602c2384acbb3b2", "a78d088fa55aa495"),
            None
        );
        assert_eq!(
            fullscreen_structured_alias("a7cfe758751c20aa", "88dc833624e2c5ec"),
            None
        );
    }

    #[test]
    fn state_tables_allocates_distinct_fresh_structured_table_and_refreshes_on_hit() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_TYPELESS, DXGI_SAMPLE_DESC};
        use windows::core::s;

        let Some(host) = crate::host::host() else {
            return; // no host GPU
        };

        let mut backing = vec![0u8; 512];
        let base = backing.as_ptr() as u64;
        let _shadow = ShadowGuard {
            resource: base as usize,
            address: base,
        };
        crate::shadow::register(base as usize, base, backing.len() as u64);
        for i in 0..32u32 {
            backing[i as usize * 4..i as usize * 4 + 4]
                .copy_from_slice(&(0x3300_0000 | i).to_le_bytes());
        }

        #[repr(C)]
        struct ViewHeapDesc {
            desc: D3D12_DESCRIPTOR_HEAP_DESC,
            placement: [u64; 2],
        }
        let heap_desc = ViewHeapDesc {
            desc: D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 4,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
            placement: [0; 2],
        };
        let heap_iid = ID3D12DescriptorHeap::IID;
        let mut views = std::ptr::null();
        assert_eq!(
            crate::descriptor::create_heap(
                std::ptr::null(),
                (&raw const heap_desc).cast(),
                (&raw const heap_iid).cast(),
                &mut views,
            ),
            crate::object::S_OK
        );
        let _views = TitleObject(views);

        let table_handle = crate::descriptor::heap(views).start;
        let mut res_words = [0u32; 14];
        res_words[0] = 1; // BUFFER
        res_words[4] = 128;
        let raw_srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: 32, // 32 raw dwords = 128 bytes = 2x 64-byte records
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: `D3D12_SHADER_RESOURCE_VIEW_DESC` is 40 bytes (`[u32; 10]`).
        let raw_words: [u32; 10] = unsafe {
            std::ptr::from_ref(&raw_srv)
                .cast::<[u32; 10]>()
                .read_unaligned()
        };
        let mut srv_words = [0u32; 16];
        srv_words[..10].copy_from_slice(&raw_words);
        srv_words[10] = base as u32;
        srv_words[11] = (base >> 32) as u32;
        crate::descriptor::create_placed_srv(
            std::ptr::null(),
            &res_words,
            &srv_words,
            table_handle,
        );

        let host_heap = crate::descriptor::heap(views).host().expect("host heap");
        let _ = host_heap.mirror(0, table_handle, true);

        let used_srv = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        let alias = fullscreen_structured_alias("80cb958cde505e15", "f9bf13c6c099ccec")
            .expect("sRGB fullscreen pair");

        let mut state = State::default();
        state.set_heaps(&[views]);
        state.set_table(false, 0, table_handle);

        let list1 = host.new_list().expect("host list");
        let made_unaliased = state.tables(&list1, &[used_srv], false, &[]);
        let made_aliased = state.tables(&list1, &[used_srv], false, &[alias]);
        let made_aliased_hit = state.tables(&list1, &[], false, &[alias]);
        assert_eq!(made_unaliased.len(), 1);
        assert_eq!(made_aliased.len(), 1);
        assert_ne!(
            made_unaliased[0].1, made_aliased[0].1,
            "aliased and unaliased draws on the same title table must get distinct dynamic tables"
        );
        assert!(
            made_aliased[0].1 >= host_heap.count() * 32,
            "aliased table must live in dynamic slots, leaving canonical slot 0 untouched"
        );
        assert_eq!(made_aliased_hit, made_aliased);

        // Compile a shader verifying the fresh dynamic slot serves StructuredBuffer(64)
        // while canonical slot 0 still serves raw ByteAddressBuffer.
        const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=1)), DescriptorTable(SRV(t1, numDescriptors=1)), UAV(u0)"
struct Record { uint w[16]; };
StructuredBuffer<Record> aliased : register(t0);
ByteAddressBuffer canonical_raw : register(t1);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint count = 0;
    uint stride = 0;
    aliased.GetDimensions(count, stride);
    dst.Store(0, count);
    dst.Store(4, stride);
    dst.Store(8, aliased[1].w[3]);
    dst.Store(12, canonical_raw.Load(16 * 4 + 3 * 4));
}
"#;
        let mut blob: Option<ID3DBlob> = None;
        // SAFETY: shader source and entry point strings are static.
        unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("draw_fresh_table_alias"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut blob,
                None,
            )
            .unwrap();
        }
        let blob = blob.unwrap();
        // SAFETY: `blob` holds valid DXBC bytes.
        let code = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        };
        // SAFETY: `code` includes the root signature and compute bytecode.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root = host.device.CreateRootSignature(0, code).unwrap();
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            let pso = host.device.CreateComputePipelineState(&desc).unwrap();
            (root, pso)
        };

        let make_buffer = |kind: D3D12_HEAP_TYPE, flags, state| -> ID3D12Resource {
            let mut b: Option<ID3D12Resource> = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
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
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            // SAFETY: valid buffer creation parameters.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut b,
                    )
                    .unwrap();
            }
            b.unwrap()
        };
        let out = make_buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let readback = make_buffer(
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // Mutate record 1, dword 3 and issue a cache-hit table lookup on `list2`
        // to confirm cache hits still schedule a fresh backing refresh.
        let updated = 0xDEAD_BEEFu32;
        backing[(16 + 3) * 4..(16 + 3) * 4 + 4].copy_from_slice(&updated.to_le_bytes());

        list1.close();
        host.execute(&[&list1]);

        let list2 = host.new_list().expect("host list");
        let cached = state.tables(&list2, &[], false, &[alias]);
        assert_eq!(cached, made_aliased);
        let aliased_gpu = host_heap.gpu(cached[0].1 / 32);
        let canonical_gpu = host_heap.gpu(0);
        // SAFETY: `list2` is open with live pipeline, heap, and output buffer.
        unsafe {
            let l = &list2.list;
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            l.SetDescriptorHeaps(&[Some(host_heap.heap.clone())]);
            l.SetComputeRootDescriptorTable(0, aliased_gpu);
            l.SetComputeRootDescriptorTable(1, canonical_gpu);
            l.SetComputeRootUnorderedAccessView(2, out.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[crate::host::transition(
                &out,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            l.CopyBufferRegion(&readback, 0, &out, 0, 16);
        }
        list2.close();
        host.execute(&[&list2]);

        let mut mapped = std::ptr::null_mut();
        let mut got = [0u32; 4];
        // SAFETY: `host.execute` completed and readback holds 16 bytes.
        unsafe {
            let range = D3D12_RANGE { Begin: 0, End: 16 };
            readback.Map(0, Some(&range), Some(&mut mapped)).unwrap();
            std::ptr::copy_nonoverlapping(mapped.cast::<u32>(), got.as_mut_ptr(), 4);
            readback.Unmap(0, None);
        }
        assert_eq!(got, [2, 64, updated, updated]);
    }

    /// Direct byte copies into a shader-visible descriptor heap (bypassing
    /// `Create*View` / `CopyDescriptors*`) are mirrored on the host so a GPU
    /// dispatch reading that canonical slot sees the copied view rather than
    /// a null or stale descriptor.
    #[test]
    fn draw_sees_descriptor_copied_directly_in_heap_memory() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_TYPELESS, DXGI_SAMPLE_DESC};
        use windows::core::s;

        struct DropHeap(This);
        impl Drop for DropHeap {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
                }
            }
        }

        let Some(host) = crate::host::host() else {
            return; // no host GPU
        };

        // Two distinct title buffers holding known 4-dword patterns.
        let mut buf_a = vec![0u8; 256];
        let mut buf_b = vec![0u8; 256];
        let pat_a = [0x1122_3344u32, 0x5566_7788, 0xAABB_CCDD, 0xCAFE_BABE];
        let pat_b = [0xDEAD_BEEFu32, 0x1337_C0DE, 0x0102_0304, 0xFEDC_BA98];
        for (i, w) in pat_a.iter().enumerate() {
            buf_a[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        for (i, w) in pat_b.iter().enumerate() {
            buf_b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }

        let make_placed_raw_srv = |buf: &[u8], slot_addr: u64| {
            let base = buf.as_ptr() as u64;
            let mut res_words = [0u32; 14];
            res_words[0] = 1; // D3D12_RESOURCE_DIMENSION_BUFFER
            res_words[4] = buf.len() as u32;
            res_words[6] = 1; // Height
            res_words[7] = 1 | (1 << 16); // DepthOrArraySize=1, MipLevels=1
            res_words[9] = 1; // SampleDesc.Count=1
            res_words[11] = 1; // D3D12_TEXTURE_LAYOUT_ROW_MAJOR
            let raw_srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: 4,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            // SAFETY: `D3D12_SHADER_RESOURCE_VIEW_DESC` is 40 bytes (`[u32; 10]`).
            let raw_words: [u32; 10] = unsafe {
                std::ptr::from_ref(&raw_srv)
                    .cast::<[u32; 10]>()
                    .read_unaligned()
            };
            let mut srv_words = [0u32; 16];
            srv_words[..10].copy_from_slice(&raw_words);
            srv_words[10] = base as u32;
            srv_words[11] = (base >> 32) as u32;
            crate::descriptor::create_placed_srv(
                std::ptr::null(),
                &res_words,
                &srv_words,
                slot_addr,
            );
        };

        // Write both views into a CPU-only staging buffer (not a shader-visible heap).
        let mut staging = [0u64; 2 * 4];
        let stage_a = staging.as_mut_ptr() as u64;
        let stage_b = stage_a + 32;
        make_placed_raw_srv(&buf_a, stage_a);
        make_placed_raw_srv(&buf_b, stage_b);

        // Create a shader-visible view heap and initialize its host mirror.
        #[repr(C)]
        struct ExtendedHeap {
            desc: [u32; 4],
            placement: [u64; 2],
        }
        let heap_desc = ExtendedHeap {
            desc: [0, 16, 1, 0],
            placement: [0; 2],
        };
        let mut raw_heap: This = std::ptr::null();
        assert_eq!(
            crate::descriptor::create_heap(
                std::ptr::null(),
                std::ptr::from_ref(&heap_desc).cast(),
                &crate::object::guid(
                    0x8efb471d,
                    0x616c,
                    0x4f49,
                    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
                ),
                &mut raw_heap,
            ),
            crate::object::S_OK
        );
        let _guard = DropHeap(raw_heap);
        let title_heap = crate::descriptor::heap(raw_heap);
        let host_heap = title_heap.host().expect("host view heap");
        let target_slot = title_heap.start + 3 * 32;

        // Copy `stage_a` bytes directly into slot 3 without calling any descriptor API.
        // SAFETY: both pointers refer to live 32-byte descriptor slots.
        unsafe { std::ptr::copy_nonoverlapping(stage_a as *const u8, target_slot as *mut u8, 32) };

        // Mirror via the submission-time sync hook and refresh backing memory.
        crate::descriptor::sync_all_heaps();

        const SOURCE: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=1)), UAV(u0)"
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    dst.Store(0, src.Load(0));
    dst.Store(4, src.Load(4));
    dst.Store(8, src.Load(8));
    dst.Store(12, src.Load(12));
}
"#;
        let mut blob: Option<ID3DBlob> = None;
        // SAFETY: static shader source and entry point strings.
        unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("draw_direct_heap_copy"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut blob,
                None,
            )
            .unwrap();
        }
        let blob = blob.unwrap();
        // SAFETY: `blob` holds valid DXBC bytes.
        let code = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        };
        // SAFETY: `code` includes the root signature and compute bytecode.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            let root = host.device.CreateRootSignature(0, code).unwrap();
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            let pso = host.device.CreateComputePipelineState(&desc).unwrap();
            (root, pso)
        };

        let make_buf = |kind: D3D12_HEAP_TYPE, flags, state| -> ID3D12Resource {
            let mut b: Option<ID3D12Resource> = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
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
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            // SAFETY: valid committed buffer description.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut b,
                    )
                    .unwrap();
            }
            b.unwrap()
        };
        let out = make_buf(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let readback = make_buf(
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        let run_and_read = |backing: &[u8]| -> [u32; 4] {
            let list = host.new_list().expect("host list");
            if let Some(shadow) = crate::shadow::find(backing.as_ptr() as u64, backing.len() as u64)
            {
                shadow.refresh(&list, backing.as_ptr() as u64, backing.len() as u64);
            }
            // SAFETY: open list with live pipeline, host view heap, and buffers.
            unsafe {
                let l = &list.list;
                l.SetComputeRootSignature(&root);
                l.SetPipelineState(&pso);
                l.SetDescriptorHeaps(&[Some(host_heap.heap.clone())]);
                l.SetComputeRootDescriptorTable(0, host_heap.gpu(3));
                l.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
                l.Dispatch(1, 1, 1);
                l.ResourceBarrier(&[crate::host::transition(
                    &out,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                l.CopyBufferRegion(&readback, 0, &out, 0, 16);
                l.ResourceBarrier(&[crate::host::transition(
                    &out,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
            list.close();
            host.execute(&[&list]);
            let mut mapped = std::ptr::null_mut();
            let mut got = [0u32; 4];
            // SAFETY: execution completed and `readback` holds 16 bytes.
            unsafe {
                let range = D3D12_RANGE { Begin: 0, End: 16 };
                readback.Map(0, Some(&range), Some(&mut mapped)).unwrap();
                std::ptr::copy_nonoverlapping(mapped.cast::<u32>(), got.as_mut_ptr(), 4);
                readback.Unmap(0, None);
            }
            got
        };

        assert_eq!(run_and_read(&buf_a), pat_a);

        // Overwrite slot 3 directly with `stage_b` bytes and sync again.
        // SAFETY: both pointers refer to live 32-byte descriptor slots.
        unsafe { std::ptr::copy_nonoverlapping(stage_b as *const u8, target_slot as *mut u8, 32) };
        assert_eq!(title_heap.sync_direct_copies(true), 1);
        assert_eq!(run_and_read(&buf_b), pat_b);
    }
}
