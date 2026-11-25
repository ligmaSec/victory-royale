//! Command allocators and command lists.
//!
//! The title's inlined code writes packets into a list only while its put
//! pointer is below a limit, and calls the list's method otherwise. Our
//! lists keep the pointer and every limit at zero, so every command reaches
//! the table as a call.
//!
//! Each list records onto a host command list as its methods are called
//! (decision 0007); draw state lives in `crate::draw`. Methods not
//! written yet log once with their caller, so a run lists what to do next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::mem::offset_of;

use crate::object::{
    self, ALLOCATOR, E_INVALIDARG, HRESULT, Iid, LIST, Object, S_OK, Table, This, guid,
};
use crate::queue::Written;
use crate::resource::PAGEABLE_IIDS;

const ALLOCATOR_IIDS: [Iid; 4] = [
    guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// The list interfaces up to `ID3D12GraphicsCommandList6` (the table has
/// all their slots): those whose ids are the same as on PC, the console's
/// own List5 and List6 (the title asks for List6), then the
/// device child's.
const LIST_IIDS: [Iid; 10] = [
    guid(
        0x7116d91c,
        0xe7e4,
        0x47ce,
        [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5],
    ),
    guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    ),
    guid(
        0x553103fb,
        0x1fe7,
        0x4557,
        [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7],
    ),
    guid(
        0x38c3e585,
        0xff17,
        0x412c,
        [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28],
    ),
    guid(
        0x6fda83a7,
        0xb84c,
        0x4e38,
        [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d],
    ),
    guid(
        0x8754318e,
        0xd3a9,
        0x4541,
        [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74],
    ),
    guid(
        0x55050859,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    guid(
        0xc3827890,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Allocator {
    object: Object,
    kind: u32,
}

/// The layout the title's inlined code reads.
#[repr(C)]
struct List {
    object: Object,
    kind: u32,
    /// The put pointer, then the plain, draw, API and unbiased API limits:
    /// all zero, so every inline check fails.
    put: [u64; 5],
    reserved: [u32; 64],
    /// State the inline code keeps for building packets; unused by us.
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    pipeline_shadow: [u32; 8],
    /// Newer titles' lists are larger: the title's state flush
    /// (title+0x32786) copies a pipeline's 32 bytes to +0x544..+0x564 and
    /// tests +0x548, so its layout runs past 0x560. Its size is unknown;
    /// this keeps our fields clear of whatever the title writes there.
    newer: [u32; 256],
    /// Ours, after the console layout: memory handed out by
    /// [`allocate_upload`], kept until the list is reset; the host list the
    /// calls record into (decision 0007), and the draw state the title set.
    uploads: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Shared with submissions still to run (queues held back by a wait).
    #[cfg(windows)]
    host: std::sync::Mutex<Option<std::sync::Arc<crate::host::HostList>>>,
    /// Closed host list segments and mid-list memory waits recorded before the
    /// active `host` list. Lock order: `host` -> `segments` -> `state`.
    #[cfg(windows)]
    segments: std::sync::Mutex<Vec<Segment>>,
    /// Whether `host` has GPU or deferred work recorded since the last split or
    /// reset.
    #[cfg(windows)]
    has_work: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    state: std::sync::Mutex<crate::draw::State>,
}
const _: () = assert!(offset_of!(List, put) == 0x18);
const _: () = assert!(offset_of!(List, pipeline_shadow) + 32 == 0x560);

/// One segment of a recorded command list in submission order.
#[cfg(windows)]
#[derive(Clone)]
pub(crate) enum Segment {
    List(std::sync::Arc<crate::host::HostList>),
    Wait(crate::timeline::MemoryWait),
}

#[cfg(windows)]
impl Drop for List {
    fn drop(&mut self) {
        let uploads = std::mem::take(self.uploads.get_mut().unwrap_or_else(|e| e.into_inner()));
        let segments = std::mem::take(self.segments.get_mut().unwrap_or_else(|e| e.into_inner()));
        let current = self
            .host
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(pending) = segments
            .iter()
            .filter_map(|s| match s {
                Segment::List(l) => Some(l),
                Segment::Wait(_) => None,
            })
            .chain(current.as_ref())
            .rfind(|l| std::sync::Arc::strong_count(l) > 1)
        {
            pending.keep_uploads(uploads);
            let dynamic = self
                .state
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take_dynamic();
            pending.keep_dynamic(dynamic);
        }
    }
}

/// Runs `f` with the host list and draw state of a title list, if there
/// is a host GPU.
#[cfg(windows)]
fn with_state(this: This, f: impl FnOnce(&crate::host::HostList, &mut crate::draw::State)) {
    let Some(list) = host_list(this) else { return };
    // SAFETY: the title passes one of our lists.
    let state = unsafe { &(*this.cast::<List>()).state };
    f(&list, &mut state.lock().unwrap_or_else(|e| e.into_inner()));
}

/// Marks the active host list segment as having recorded GPU or deferred work.
#[cfg(windows)]
fn mark_work(this: This) {
    if !this.is_null() {
        // SAFETY: the title passes one of our lists.
        unsafe { &*this.cast::<List>() }
            .has_work
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Closed host list segments and mid-list memory waits recorded on `this`,
/// including the active host list when it has work or when no prior segments
/// exist.
#[cfg(windows)]
pub(crate) fn segments(this: This) -> Vec<Segment> {
    if this.is_null() {
        return Vec::new();
    }
    // SAFETY: the title passes one of our lists.
    let list = unsafe { &*this.cast::<List>() };
    let host = list.host.lock().unwrap_or_else(|e| e.into_inner());
    let segments = list.segments.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = segments.clone();
    if let Some(current) = host.as_ref()
        && (list.has_work.load(std::sync::atomic::Ordering::Acquire) || out.is_empty())
    {
        out.push(Segment::List(current.clone()));
    }
    out
}

/// The host list behind a title list, if there is a host GPU.
#[cfg(windows)]
fn host_list(this: This) -> Option<std::sync::Arc<crate::host::HostList>> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our lists.
    let host = unsafe { &(*this.cast::<List>()).host };
    host.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) extern "system" fn create_allocator(
    _: This,
    kind: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let allocator = Box::new(Allocator {
        object: Object::new(&ALLOCATOR_FUNCTIONS),
        kind,
    });
    object::hand_out(ALLOCATOR, allocator, iid, &ALLOCATOR_IIDS, out)
}

/// Lists start open, as in D3D12.
pub(crate) extern "system" fn create_list(
    _: This,
    _nodes: u32,
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    new_list(kind, allocator, pipeline, iid, out)
}

/// The console's list creation: type, flags, node mask and barrier batch
/// size in a description.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct ListDesc {
    kind: u32,
    flags: u32,
    nodes: u32,
    barrier_batch: u32,
}

pub(crate) extern "system" fn create_list_x(
    _: This,
    desc: *const ListDesc,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "console command list");
    new_list(desc.kind, allocator, pipeline, iid, out)
}

fn new_list(
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    tracing::debug!(kind, "command list");
    let list = Box::new(List {
        object: if crate::device::newer() {
            Object::new(&NEWER_LIST_FUNCTIONS)
        } else {
            Object::new(&LIST_FUNCTIONS)
        },
        kind,
        put: [0; 5],
        reserved: [0; 64],
        graphics_root_headers: [0; 128],
        compute_root_headers: [0; 128],
        pipeline_shadow: [0; 8],
        newer: [0; 256],
        uploads: Default::default(),
        #[cfg(windows)]
        host: std::sync::Mutex::new(crate::host::host().and_then(|h| match h.acquire_list() {
            Ok(list) => Some(std::sync::Arc::new(list)),
            Err(e) => {
                tracing::warn!(error = %e, "no host command list");
                None
            }
        })),
        #[cfg(windows)]
        segments: std::sync::Mutex::new(Vec::new()),
        #[cfg(windows)]
        has_work: std::sync::atomic::AtomicBool::new(false),
        #[cfg(windows)]
        state: {
            let mut state = crate::draw::State::default();
            state.set_pipeline(pipeline);
            std::sync::Mutex::new(state)
        },
    });
    #[cfg(not(windows))]
    let _ = pipeline;
    object::hand_out(LIST, list, iid, &LIST_IIDS, out)
}

extern "system" fn allocator_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ALLOCATOR_IIDS).then_some(this);
    object::answer(ALLOCATOR, iid, found, out)
}
extern "system" fn allocator_release(this: This) -> u32 {
    object::release_boxed::<Allocator>(this)
}
/// Nothing is recorded, so there is nothing to free.
extern "system" fn allocator_reset(_: This) -> HRESULT {
    S_OK
}

extern "system" fn list_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(LIST, iid, object::is(iid, &LIST_IIDS).then_some(this), out)
}
extern "system" fn list_release(this: This) -> u32 {
    object::release_boxed::<List>(this)
}
extern "system" fn list_type(this: This) -> u32 {
    // SAFETY: `this` is one of our lists.
    unsafe { (*this.cast::<List>()).kind }
}
extern "system" fn close(this: This) -> HRESULT {
    #[cfg(windows)]
    if let Some(host) = host_list(this) {
        host.close();
    }
    S_OK
}
extern "system" fn reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes one of our lists.
    let list = unsafe { &*this.cast::<List>() };
    let uploads = std::mem::take(&mut *list.uploads.lock().unwrap_or_else(|e| e.into_inner()));
    // A submission of the list still to run (its queue waits for a fence or
    // memory wait) keeps the last pending segment's host list, upload memory,
    // and dynamic descriptor slots; recording goes on in a pooled or new list,
    // open already.
    #[cfg(windows)]
    let (fresh, keep_pending) = {
        let mut host = list.host.lock().unwrap_or_else(|e| e.into_inner());
        let mut segments = list.segments.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = list.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = segments
            .iter()
            .filter_map(|s| match s {
                Segment::List(l) => Some(l),
                Segment::Wait(_) => None,
            })
            .chain(host.as_ref())
            .rfind(|l| std::sync::Arc::strong_count(l) > 1)
        {
            pending.keep_uploads(uploads);
            pending.keep_dynamic(state.take_dynamic());
        }
        segments.clear();
        list.has_work
            .store(false, std::sync::atomic::Ordering::Release);
        let current_pending = host
            .as_ref()
            .is_some_and(|l| std::sync::Arc::strong_count(l) > 1);
        if current_pending {
            match crate::host::host().map(|h| h.acquire_list()) {
                Some(Ok(new)) => {
                    *host = Some(std::sync::Arc::new(new));
                    (true, false)
                }
                _ => {
                    tracing::warn!("no new host list: a list to run is reset");
                    (false, true)
                }
            }
        } else {
            (false, false)
        }
    };
    #[cfg(windows)]
    with_state(this, |host, state| {
        if !fresh && !keep_pending {
            host.reset();
        }
        state.reset(pipeline);
    });
    #[cfg(not(windows))]
    let _ = (this, pipeline);
    S_OK
}

/// A resource barrier as titles pass it: type, flags, then a resource and
/// either a subresource and two states (transition) or a second resource
/// (aliasing). The same layout as the host's.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Barrier {
    kind: u32,
    flags: u32,
    first: This,
    second: u64,
    last: u64,
}

/// The barrier on host resources, or `None` if it names a resource the host
/// GPU does not have (buffers so far).
#[cfg(windows)]
fn host_barrier(list: &crate::host::HostList, b: &Barrier) -> Option<Barrier> {
    use windows::core::Interface;
    let host = |r: This| crate::resource::host_resource(r).map(|h| h.as_raw().cast_const().cast());
    match b.kind {
        0 => {
            // A split barrier's first half: its second half transitions.
            const BEGIN_ONLY: u32 = 1;
            if b.flags & BEGIN_ONLY != 0 {
                return None;
            }
            // shortcut: whole resources, from the state the list tracked
            // (`HostList::transition`); subresources are not told apart.
            let wanted = crate::host::texture_state(b.last as u32);
            let (texture, before) = list.transition(b.first, wanted)?;
            Some(Barrier {
                kind: 0,
                flags: 0,
                first: texture.as_raw().cast_const().cast(),
                second: 0xffff_ffff | (u64::from(before) << 32),
                last: u64::from(crate::host::legal_state(texture, wanted)),
            })
        }
        1 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                host(b.first)?
            },
            second: if b.second == 0 {
                0
            } else {
                host(b.second as This)? as u64
            },
            ..*b
        }),
        2 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                host(b.first)?
            },
            ..*b
        }),
        _ => None,
    }
}

/// An unordered access barrier on every resource: earlier shader writes
/// finish before later work reads or writes.
const UAV_BARRIER: Barrier = Barrier {
    kind: 2,
    flags: 0,
    first: std::ptr::null(),
    second: 0,
    last: 0,
};

/// Returns the console's barrier "actions" (none).
extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && !barriers.is_null()
    {
        // SAFETY: the title passes `count` barriers.
        let barriers = unsafe { std::slice::from_raw_parts(barriers, count as usize) };
        let mut host: Vec<Barrier> = barriers
            .iter()
            .filter_map(|b| host_barrier(&list, b))
            .collect();
        // Buffers have host copies, not resources the barrier can name:
        // a barrier on one waits for every shader write before it.
        if barriers.iter().any(|b| {
            b.kind != 1 && !b.first.is_null() && crate::resource::buffer_bytes(b.first).is_some()
        }) {
            host.push(UAV_BARRIER);
        }
        if !host.is_empty() {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
            const _: () = assert!(size_of::<Barrier>() == size_of::<D3D12_RESOURCE_BARRIER>());
            list.mark_work();
            // SAFETY: the same layout; borrowed resource pointers, never
            // released through this slice.
            let host = unsafe {
                std::slice::from_raw_parts(
                    host.as_ptr().cast::<D3D12_RESOURCE_BARRIER>(),
                    host.len(),
                )
            };
            // SAFETY: an open list and live resources.
            unsafe { list.list.ResourceBarrier(host) };
            mark_work(this);
        }
    }
    0
}

/// Moves a title texture's host copy to `state` in the list, if needed.
#[cfg(windows)]
pub(crate) fn ensure_state(list: &crate::host::HostList, resource: This, state: u32) {
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
    if let Some((texture, before)) = list.transition(resource, state) {
        let state = crate::host::legal_state(texture, state);
        let barrier = crate::host::transition(texture, State(before as i32), State(state as i32));
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
}

/// Same as [`ensure_state`], for bindless heap sweeps: records a host barrier
/// without overwriting the title's explicit access intent.
#[cfg(windows)]
pub(crate) fn ensure_bindless_state(list: &crate::host::HostList, resource: This, state: u32) {
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
    if let Some((texture, before)) = list.transition_bindless(resource, state) {
        let state = crate::host::legal_state(texture, state);
        let barrier = crate::host::transition(texture, State(before as i32), State(state as i32));
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
}

#[cfg(windows)]
const COPY_DEST: u32 = 0x400;
#[cfg(windows)]
const COPY_SOURCE: u32 = 0x800;

/// `len` bytes at `offset` of a title buffer, if they are inside it.
#[cfg(windows)]
fn buffer_range(buffer: This, offset: u64, len: u64) -> Option<*mut u8> {
    let (data, size) = crate::resource::buffer_bytes(buffer)?;
    let end = offset.checked_add(len)?;
    if end > size {
        tracing::warn!(offset, len, size, "copy outside its buffer");
        return None;
    }
    // SAFETY: inside the buffer, checked above.
    Some(unsafe { data.add(offset as usize) })
}

/// Buffer to buffer: both are host memory, copied at submission.
extern "system" fn copy_buffer_region(
    this: This,
    dst: This,
    dst_offset: u64,
    src: This,
    src_offset: u64,
    len: u64,
) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && let Some(to) = buffer_range(dst, dst_offset, len)
        && let Some(from) = buffer_range(src, src_offset, len)
    {
        list.defer_copy(to, from, len as usize);
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_offset, src, src_offset, len);
}

extern "system" fn copy_resource(this: This, dst: This, src: This) {
    #[cfg(windows)]
    {
        let Some(list) = host_list(this) else {
            return;
        };
        if let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        ) {
            if crate::texture::title_written(from) {
                crate::texture::upload(&list, src, from);
            }
            ensure_state(&list, dst, COPY_DEST);
            ensure_state(&list, src, COPY_SOURCE);
            crate::texture::host_written(dst);
            list.mark_work();
            #[cfg(feature = "diagnostics")]
            list.note_op(|| format!("copy resource {src:?} to {dst:?}"));
            // SAFETY: an open list and live textures.
            unsafe { list.list.CopyResource(to, from) };
            mark_work(this);
        } else if let (Some((to, a)), Some((from, b))) = (
            crate::resource::buffer_bytes(dst),
            crate::resource::buffer_bytes(src),
        ) {
            list.defer_copy(to, from, a.min(b) as usize);
            mark_work(this);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src);
}

extern "system" fn copy_resource_x(this: This, dst: This, src: This, _flags: u32) {
    copy_resource(this, dst, src);
}

/// A copy location as titles pass it, laid out as the host's: a resource,
/// then a subresource index (type 0) or a placed footprint (type 1).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CopyLocation {
    resource: This,
    kind: u32,
    footprint: crate::resource::PlacedFootprint,
}

/// Texture to texture on the GPU, buffer to texture through an upload
/// buffer filled at submission, and texture to buffer through a readback
/// buffer copied into title memory once the submission completes.
extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_BOX, D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_TEXTURE_COPY_LOCATION,
        };
        use windows::core::Interface;
        const FOOTPRINT: u32 = 1;
        // SAFETY: the title passes both locations (null is tolerated).
        let (Some(list), Some(&dst), Some(&src)) =
            (host_list(this), unsafe { dst.as_ref() }, unsafe {
                src.as_ref()
            })
        else {
            return;
        };
        const _: () =
            assert!(size_of::<CopyLocation>() == size_of::<D3D12_TEXTURE_COPY_LOCATION>());
        let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
        if dst.kind == FOOTPRINT {
            if src.kind == FOOTPRINT {
                return;
            }
            let (Some(host), Some(from_tex)) = (
                crate::host::host(),
                crate::resource::host_resource(src.resource),
            ) else {
                return;
            };
            let Some((lines, row_bytes, total)) = dst.footprint.layout() else {
                return;
            };
            let host_format = crate::resource::pc_format(dst.footprint.format);
            if crate::resource::element(host_format)
                != crate::resource::element(dst.footprint.format)
            {
                // Console formats such as 118..=120 (3-byte D16S8) map to
                // 4-byte PC substitutes (45..=47): raw readback would mis-size
                // the staging buffer and write the wrong texel layout.
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(
                        format = dst.footprint.format,
                        host_format,
                        "texture-to-buffer readback skipped: host format has a different texel size",
                    );
                });
                return;
            }
            if total == 0 || lines == 0 || row_bytes == 0 {
                return;
            }
            let extent = if let Some(box_ptr) = host_box {
                // SAFETY: the title passed a complete source box.
                let b = unsafe { *box_ptr };
                let [Some(w), Some(h), Some(d)] = [
                    b.right.checked_sub(b.left),
                    b.bottom.checked_sub(b.top),
                    b.back.checked_sub(b.front),
                ] else {
                    return;
                };
                [w, h, d]
            } else {
                // A missing box copies the entire source subresource. Its mip
                // or plane can be smaller than the destination footprint.
                let mut layout = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                // SAFETY: a live texture and device, with room for one footprint.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &from_tex.GetDesc(),
                        src.footprint.offset as u32,
                        1,
                        0,
                        Some(&mut layout),
                        None,
                        None,
                        None,
                    );
                }
                [
                    layout.Footprint.Width,
                    layout.Footprint.Height,
                    layout.Footprint.Depth,
                ]
            };
            let Some(region) = dst.footprint.copy_rows([x, y, z], extent) else {
                return;
            };
            let dst_pitch = u64::from(dst.footprint.row_pitch);
            if lines > 1 && dst_pitch < row_bytes {
                return;
            }
            let Some(target) = buffer_range(dst.resource, dst.footprint.offset, total) else {
                return;
            };
            // Only the copied rectangle needs a host staging footprint.
            let Ok(row_bytes_u32) = u32::try_from(region.row_bytes) else {
                return;
            };
            let Some(host_pitch) = row_bytes_u32.checked_next_multiple_of(256) else {
                return;
            };
            let staging_lines = u64::from(extent[2]) * region.lines;
            let Some(staging_bytes) = u64::from(host_pitch)
                .checked_mul(staging_lines - 1)
                .and_then(|n| n.checked_add(region.row_bytes))
            else {
                return;
            };
            let Some((buffer, staging)) = list.readback(host, staging_bytes) else {
                return;
            };
            crate::texture::upload(&list, src.resource, from_tex);
            ensure_state(&list, src.resource, COPY_SOURCE);
            list.mark_work();
            let mut to_location = dst;
            to_location.resource = buffer.as_raw().cast_const().cast();
            to_location.footprint.offset = 0;
            to_location.footprint.format = host_format;
            [
                to_location.footprint.width,
                to_location.footprint.height,
                to_location.footprint.depth,
            ] = extent;
            to_location.footprint.row_pitch = host_pitch;
            let mut from_location = src;
            from_location.resource = from_tex.as_raw().cast_const().cast();
            #[cfg(feature = "diagnostics")]
            list.note_op(|| {
                // SAFETY: a live texture; the box is the title's or null.
                let (d, b) = unsafe { (from_tex.GetDesc(), host_box.map(|b| *b)) };
                format!(
                    "readback to {:?} footprint {:?} at {x},{y},{z} from {:?} {}x{} f{} sub {} box {b:?}",
                    dst.resource,
                    dst.footprint,
                    src.resource,
                    d.Width,
                    d.Height,
                    d.Format.0,
                    src.kind,
                )
            });
            // SAFETY: an open list; both locations in the host's layout, naming
            // live resources; the box is the title's (same layout).
            unsafe {
                list.list.CopyTextureRegion(
                    (&raw const to_location).cast(),
                    0,
                    0,
                    0,
                    (&raw const from_location).cast(),
                    host_box,
                )
            };
            for slice in 0..extent[2] {
                let line = region.first_line + u64::from(slice) * region.slice_lines;
                // copy_rows checked the rectangle against the footprint, and
                // both mapped buffers cover their full pitched footprints.
                let to = line * dst_pitch + region.byte_in_row;
                let from = u64::from(slice) * region.lines * u64::from(host_pitch);
                list.defer_readback(
                    target.wrapping_add(to as usize),
                    staging.wrapping_add(from as usize),
                    dst_pitch as usize,
                    host_pitch as usize,
                    region.lines as usize,
                    region.row_bytes as usize,
                );
            }
            mark_work(this);
            return;
        }
        let Some(to) = crate::resource::host_resource(dst.resource) else {
            return;
        };
        let mut from = src;
        let _upload; // keeps the source alive while this call records it
        #[cfg(feature = "diagnostics")]
        let mut read = None;
        if src.kind == FOOTPRINT {
            let (Some(host), Some(bytes)) = (crate::host::host(), src.footprint.bytes()) else {
                return;
            };
            let Some(data) = buffer_range(src.resource, src.footprint.offset, bytes) else {
                return;
            };
            let Some((buffer, staging)) = list.upload(host, bytes) else {
                return;
            };
            // Host-owned mapped upload (not title memory): `defer_fill` so
            // `copy_mapped` skips the title-destination VirtualQuery guard.
            // Wine+vkd3d can report Map memory as MEM_FREE; `defer_copy` would
            // then skip the upload (#518). Buffer→buffer title destinations
            // above keep `defer_copy`.
            list.defer_fill(staging, data, bytes as usize);
            from.resource = buffer.as_raw().cast_const().cast();
            from.footprint.offset = 0;
            #[cfg(feature = "diagnostics")]
            {
                // SAFETY: a live buffer.
                read = Some((unsafe { buffer.GetGPUVirtualAddress() }, bytes));
            }
            _upload = buffer;
        } else {
            let Some(texture) = crate::resource::host_resource(src.resource) else {
                return;
            };
            if crate::texture::title_written(texture) {
                crate::texture::upload(&list, src.resource, texture);
            }
            ensure_state(&list, src.resource, COPY_SOURCE);
            from.resource = texture.as_raw().cast_const().cast();
        }
        ensure_state(&list, dst.resource, COPY_DEST);
        crate::texture::host_written(dst.resource);
        list.mark_work();
        let mut to_location = dst;
        to_location.resource = to.as_raw().cast_const().cast();
        #[cfg(feature = "diagnostics")]
        {
            let text = || {
                // SAFETY: a live texture; the box is the title's or null.
                let (d, b) = unsafe { (to.GetDesc(), host_box.map(|b| *b)) };
                format!(
                    "copy to {:?} {}x{} f{} sub/kind {} at {x},{y},{z} from {:?} kind {} footprint {:?} box {b:?}",
                    dst.resource,
                    d.Width,
                    d.Height,
                    d.Format.0,
                    dst.kind,
                    src.resource,
                    src.kind,
                    (src.kind == FOOTPRINT).then_some(src.footprint)
                )
            };
            match read {
                Some(range) => list.note_read(text, range),
                None => list.note_op(text),
            }
        }
        // SAFETY: an open list; both locations in the host's layout, naming
        // live resources; the box is the title's (same layout).
        unsafe {
            list.list.CopyTextureRegion(
                (&raw const to_location).cast(),
                x,
                y,
                z,
                (&raw const from).cast(),
                host_box,
            )
        };
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, x, y, z, src, source_box);
}

#[allow(clippy::too_many_arguments)] // the method's own arguments
extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

extern "system" fn resolve(
    this: This,
    dst: This,
    dst_sub: u32,
    src: This,
    src_sub: u32,
    format: u32,
) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        )
    {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_RESOLVE_MODE_AVERAGE, D3D12_RESOURCE_DESC, ID3D12GraphicsCommandList1,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
        use windows::core::Interface;
        const RESOLVE_DEST: u32 = 0x1000;
        const RESOLVE_SOURCE: u32 = 0x2000;
        ensure_state(&list, dst, RESOLVE_DEST);
        ensure_state(&list, src, RESOLVE_SOURCE);
        crate::texture::host_written(dst);
        list.mark_work();
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("resolve {src:?} to {dst:?} format {format}"));
        let extent = |d: D3D12_RESOURCE_DESC, subresource: u32| {
            // Mips are the low-order component of an array or plane subresource index.
            let mip = subresource % u32::from(d.MipLevels);
            ((d.Width >> mip).max(1), (d.Height >> mip).max(1))
        };
        // SAFETY: an open list and live textures; the region is inside both.
        unsafe {
            let (dst_width, dst_height) = extent(to.GetDesc(), dst_sub);
            let (src_width, src_height) = extent(from.GetDesc(), src_sub);
            let format = DXGI_FORMAT(format as i32);
            if (dst_width, dst_height) == (src_width, src_height) {
                list.list
                    .ResolveSubresource(to, dst_sub, from, src_sub, format);
            } else if let Ok(list1) = list.list.cast::<ID3D12GraphicsCommandList1>() {
                // The console clips a larger source to the destination.
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: dst_width.min(src_width) as i32,
                    bottom: dst_height.min(src_height) as i32,
                };
                list1.ResolveSubresourceRegion(
                    to,
                    dst_sub,
                    0,
                    0,
                    from,
                    src_sub,
                    Some(&rect),
                    format,
                    D3D12_RESOLVE_MODE_AVERAGE,
                );
            }
        }
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_sub, src, src_sub, format);
}

#[repr(C)]
pub(crate) struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DiscardRegion {
    num_rects: u32,
    rects: *const Rect,
    first_subresource: u32,
    num_subresources: u32,
}

#[cfg(windows)]
const _: () = {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D12::D3D12_DISCARD_REGION;
    assert!(size_of::<DiscardRegion>() == size_of::<D3D12_DISCARD_REGION>());
    assert!(align_of::<DiscardRegion>() == align_of::<D3D12_DISCARD_REGION>());
    assert!(size_of::<Rect>() == size_of::<RECT>());
    assert!(align_of::<Rect>() == align_of::<RECT>());
};

/// A non-null, aligned rectangle pointer must cover `num_rects` rectangles.
#[cfg(any(windows, test))]
unsafe fn valid_discard_region(
    region: Option<&DiscardRegion>,
    dimension: u32,
    mip_levels: u16,
    depth_or_array_size: u16,
    planes: u8,
    extent: [u64; 2],
) -> bool {
    const TEXTURE_2D: u32 = 3;
    const TEXTURE_3D: u32 = 4;
    let Some(region) = region else { return true };
    let mips = u64::from(mip_levels.max(1));
    let slices = if dimension == TEXTURE_3D {
        1
    } else {
        u64::from(depth_or_array_size.max(1))
    };
    let first = u64::from(region.first_subresource);
    let count = u64::from(region.num_subresources);
    if count == 0 || first + count > mips * slices * u64::from(planes) {
        return false;
    }
    if region.num_rects == 0 {
        return true;
    }
    let rects = region.num_rects as usize;
    // Rectangles apply to 2D subresources of the same dimensions. In a
    // contiguous range with multiple mips, adjacent subresources differ.
    if dimension != TEXTURE_2D
        || (mips > 1 && count > 1)
        || region.rects.is_null()
        || !region.rects.is_aligned()
        || rects > isize::MAX as usize / size_of::<Rect>()
    {
        return false;
    }
    let [width, height] =
        extent.map(|size| size.checked_shr((first % mips) as u32).unwrap_or(0).max(1));
    // SAFETY: the caller supplies the array, and its pointer and size were checked.
    unsafe { std::slice::from_raw_parts(region.rects, rects) }
        .iter()
        .all(|r| {
            r.left >= 0
                && r.top >= 0
                && r.left < r.right
                && r.top < r.bottom
                && r.right as u64 <= width
                && r.bottom as u64 <= height
        })
}

#[cfg(any(windows, test))]
fn discard_state_allowed(state: u32, flags: u32) -> bool {
    const RENDER_TARGET: u32 = 0x1;
    const DEPTH_STENCIL: u32 = 0x2;
    const UNORDERED: u32 = 0x4;
    match state {
        0x4 => flags & RENDER_TARGET != 0,  // RENDER_TARGET
        0x10 => flags & DEPTH_STENCIL != 0, // DEPTH_WRITE
        // A texture that can be a render target or depth stencil must be
        // discarded in that state, even if it also allows unordered access.
        0x8 => flags & UNORDERED != 0 && flags & (RENDER_TARGET | DEPTH_STENCIL) == 0,
        _ => false,
    }
}

extern "system" fn discard_resource(this: This, resource: This, region: *const DiscardRegion) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::*;
        let (Some(list), Some(copy)) = (host_list(this), crate::resource::host_copy(resource))
        else {
            return;
        };
        let Some(state) = list.tracked_texture_state(&copy) else {
            return;
        };
        // SAFETY: a live host texture.
        let desc = unsafe { copy.texture.GetDesc() };
        if !discard_state_allowed(state, desc.Flags.0 as u32) || !region.is_aligned() {
            return;
        }
        // SAFETY: the title passes a discard region or null.
        let region = unsafe { region.as_ref() };
        if let Some(region) = region {
            let Some(host) = crate::host::host() else {
                return;
            };
            let mut format = D3D12_FEATURE_DATA_FORMAT_INFO {
                Format: desc.Format,
                PlaneCount: 0,
            };
            // SAFETY: the feature data matches the requested query.
            if unsafe {
                host.device.CheckFeatureSupport(
                    D3D12_FEATURE_FORMAT_INFO,
                    (&raw mut format).cast(),
                    size_of::<D3D12_FEATURE_DATA_FORMAT_INFO>() as u32,
                )
            }
            .is_err()
            {
                return;
            }
            // SAFETY: the title supplies the rectangles described by the region.
            if !unsafe {
                valid_discard_region(
                    Some(region),
                    desc.Dimension.0 as u32,
                    desc.MipLevels,
                    desc.DepthOrArraySize,
                    format.PlaneCount,
                    [desc.Width, u64::from(desc.Height)],
                )
            } {
                return;
            }
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("discard resource {resource:?}"));
        // SAFETY: the list holds the texture in a compatible state. The
        // checked region and its rectangles have the host layout.
        unsafe {
            list.list
                .DiscardResource(&copy.texture, region.map(|r| std::ptr::from_ref(r).cast()))
        };
    }
    #[cfg(not(windows))]
    let _ = (this, resource, region);
}

extern "system" fn clear_rtv(
    this: This,
    handle: u64,
    color: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::rtv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        // A texture the host cannot render to has no view to clear, nor a
        // render-target state to move to (see `host::renders_to`).
        if !crate::host::renders_to(texture) {
            return;
        }
        // SAFETY: the title passes four floats (null is tolerated).
        let Some(color) = (unsafe { color.as_ref() }) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const RENDER_TARGET: u32 = 0x4;
        list.mark_work();
        let rtv = host.rtv(Some(texture), record.desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            if let Some((_, before)) = list.transition(record.resource, RENDER_TARGET) {
                use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    State(before as i32),
                    State(RENDER_TARGET as i32),
                )]);
            }
            list.list.ClearRenderTargetView(rtv, color, rects);
        }
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, handle, color, count, rects);
}

/// `ClearDepthStencilView(handle, flags, depth, stencil, count, rects)`;
/// flags: 1 depth, 2 stencil.
extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_FLAGS;
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::dsv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const DEPTH_WRITE: u32 = 0x10;
        list.mark_work();
        ensure_state(&list, record.resource, DEPTH_WRITE);
        let dsv = host.dsv(texture, record.depth_desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            list.list.ClearDepthStencilView(
                dsv,
                D3D12_CLEAR_FLAGS(flags as i32 & 3),
                depth,
                stencil,
                rects,
            )
        };
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, handle, flags, depth, stencil, count, rects);
}

/// `CopyMemoryToMemoryX(destination, source, size, flags)`: GPU addresses
/// are title memory, copied at submission like buffer copies (if mapped).
extern "system" fn copy_memory(this: This, dst: u64, src: u64, len: u64, _flags: u32) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && dst != 0
        && src != 0
    {
        let len = len as usize;
        list.defer(move || {
            if crate::texture::committed(src as *const u8, len)
                && crate::texture::committed(dst as *const u8, len)
            {
                // SAFETY: both ranges are committed title memory.
                unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
            }
        });
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src, len);
}

/// `DriverHintX(feature, value)`: no hints for the host.
extern "system" fn driver_hint(_: This, _feature: u64, _value: u64) -> u64 {
    0
}

/// `DispatchX(x, y, z, flags)`: a dispatch with console flags, which the
/// host has no use for (the ray-tracing builder dispatches this way).
extern "system" fn dispatch_x(this: This, x: u32, y: u32, z: u32, _flags: u32) {
    #[cfg(windows)]
    state::dispatch(this, x, y, z);
    #[cfg(not(windows))]
    {
        let _ = (this, x, y, z);
        count_dispatch();
    }
}

/// `BeginQuery`/`EndQuery(heap, type, index)`: queries are answered when
/// resolved.
extern "system" fn query_mark(_: This, _heap: This, _kind: u32, _index: u32) {}

/// `ResolveQueryData(heap, type, first, count, buffer, offset)`: writes
/// each query's result into the buffer once the list has run. Timestamps
/// (type 2) are the GPU clock then; occlusion counts (0) say a million
/// samples passed and binary occlusion (1) visible, so nothing is culled
/// by a query.
/// shortcut: no query runs on the host GPU; results are plausible values,
/// not measured ones (time spans come out as the submission's length).
extern "system" fn resolve_queries(
    this: This,
    _heap: This,
    kind: u32,
    _first: u32,
    count: u32,
    buffer: This,
    offset: u64,
) {
    const OCCLUSION: u32 = 0;
    const BINARY: u32 = 1;
    const TIMESTAMP: u32 = 2;
    const PIPELINE_STATISTICS: u32 = 3;
    let Some((data, size)) = crate::resource::buffer_bytes(buffer) else {
        return;
    };
    let (what, bytes) = match kind {
        OCCLUSION => (Written::Value(1 << 20), 8),
        BINARY => (Written::Value(1), 8),
        TIMESTAMP => (Written::GpuTimestamp, 8),
        PIPELINE_STATISTICS => (Written::Value(0), 88),
        _ => (Written::Value(0), 16),
    };
    if offset + u64::from(count) * bytes > size {
        return;
    }
    for i in 0..u64::from(count) {
        // Wider results are written as 8-byte pieces.
        for piece in 0..bytes / 8 {
            // SAFETY: inside the buffer, checked above.
            let at = unsafe { data.add((offset + i * bytes + piece * 8) as usize) };
            let what = if piece == 0 { what } else { Written::Value(0) };
            defer_write(this, at as u64, what, 8);
        }
    }
}

/// IUnknown 3, object 4, device child 1, allocator 2.
static ALLOCATOR_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ALLOCATOR, 24>());
    f[0] = allocator_query as *const ();
    f[2] = allocator_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = allocator_reset as *const (); // 0x40
    f[9] = object::ignore as *const (); // 0x48 allocation callback
    Table(f)
};

extern "system" fn set_pipeline(this: This, pipeline: This) {
    crate::pipeline::bound(pipeline);
    #[cfg(windows)]
    with_state(this, |_, state| state.set_pipeline(pipeline));
    #[cfg(not(windows))]
    let _ = this;
}

/// Pipeline syncs: waits for earlier work and cache flushes, at the top or
/// bottom of the pipe `(flags, control, address, size)`. Host lists order
/// their own work and every submission is waited for, so there is nothing
/// to do.
extern "system" fn sync(_: This, _flags: u32, _control: u32, _address: u64, _size: u64) {}

/// Reset with console flags `(allocator, pipeline, flags)`; the flags only
/// tune the console's command memory.
extern "system" fn reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    reset(this, allocator, pipeline)
}

/// Newer lists' 0x4a0 `(size, alignment) -> pointer`: memory the list
/// owns until it is reset, which the CPU writes and the GPU reads at the
/// same address; null on failure. The title's BVH builder puts
/// its instance descriptions there (and wrote through our old
/// `E_NOTIMPL` as a pointer).
extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size)
        .ok()
        .and_then(|s| s.checked_add(align))
    else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let mut block = vec![0u8; len];
    let start = block.as_mut_ptr();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let at = unsafe { start.add(start.align_offset(align)) };
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    // Moving the vector keeps its heap block where it is.
    uploads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(block);
    at
}

/// Draw state the host has no use for (root signatures: ours come with
/// the pipeline; depth bounds, depth order and shading rate hints): any
/// arguments, nothing returned.
extern "system" fn ignored_state(_: This) {}

/// The title's draw state (decision 0007, step 3), kept for its draws or
/// set on the host list at once. Without a host GPU these do nothing.
#[cfg(windows)]
mod state {
    use super::{This, with_state};
    use crate::draw::{IndexView, VertexView};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

    /// `count` items at `items`, or none.
    fn items<'a, T>(items: *const T, count: u32) -> &'a [T] {
        if items.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: the title passes `count` items.
        unsafe { std::slice::from_raw_parts(items, count as usize) }
    }

    pub(super) extern "system" fn topology(this: This, topology: u32) {
        with_state(this, |list, s| s.set_topology(list, topology));
    }
    pub(super) extern "system" fn viewports(
        this: This,
        count: u32,
        viewports: *const D3D12_VIEWPORT,
    ) {
        with_state(this, |list, s| {
            s.set_viewports(list, items(viewports, count))
        });
    }
    pub(super) extern "system" fn scissors(this: This, count: u32, rects: *const RECT) {
        with_state(this, |list, s| s.set_scissors(list, items(rects, count)));
    }
    pub(super) extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
        // SAFETY: the title passes four floats or null.
        let factor = unsafe { factor.as_ref() };
        with_state(this, |list, s| s.set_blend_factor(list, factor));
    }
    pub(super) extern "system" fn stencil_ref(this: This, reference: u32) {
        with_state(this, |list, s| s.set_stencil_ref(list, reference));
    }
    pub(super) extern "system" fn heaps(this: This, count: u32, heaps: *const This) {
        with_state(this, |_, s| s.set_heaps(items(heaps, count)));
    }
    pub(super) extern "system" fn compute_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(true, slot, handle));
    }
    pub(super) extern "system" fn graphics_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(false, slot, handle));
    }
    pub(super) extern "system" fn compute_constant(this: This, slot: u32, value: u32, offset: u32) {
        with_state(this, |_, s| s.set_constants(true, slot, &[value], offset));
    }
    pub(super) extern "system" fn graphics_constant(
        this: This,
        slot: u32,
        value: u32,
        offset: u32,
    ) {
        with_state(this, |_, s| s.set_constants(false, slot, &[value], offset));
    }
    pub(super) extern "system" fn compute_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(true, slot, items(values, count), offset)
        });
    }
    pub(super) extern "system" fn graphics_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(false, slot, items(values, count), offset)
        });
    }
    /// Root constant buffer, shader resource and unordered access views.
    pub(super) extern "system" fn compute_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(true, slot, address));
    }
    pub(super) extern "system" fn graphics_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(false, slot, address));
    }
    pub(super) extern "system" fn index_buffer(this: This, view: *const IndexView) {
        // SAFETY: the title passes a view or null.
        with_state(this, |_, s| s.set_index_buffer(unsafe { view.as_ref() }));
    }
    pub(super) extern "system" fn vertex_buffers(
        this: This,
        start: u32,
        count: u32,
        views: *const VertexView,
    ) {
        with_state(this, |_, s| {
            if views.is_null() {
                s.clear_vertex_buffers(start, count);
            } else {
                s.set_vertex_buffers(start, items(views, count));
            }
        });
    }
    /// `(count, handles, one range, depth)`: with one range, the handles
    /// follow the first at the render target view size (128 bytes).
    pub(super) extern "system" fn targets(
        this: This,
        count: u32,
        handles: *const u64,
        one_range: i32,
        depth: *const u64,
    ) {
        let handles: Vec<u64> = if one_range != 0 && !handles.is_null() {
            // SAFETY: the title passes the first handle.
            let first = unsafe { *handles };
            (0..u64::from(count)).map(|i| first + 128 * i).collect()
        } else {
            items(handles, count).to_vec()
        };
        // SAFETY: the title passes a handle or null.
        let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
        with_state(this, |list, s| s.set_targets(list, &handles, depth));
    }
    pub(super) extern "system" fn draw(
        this: This,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        super::count_draw();
        super::mark_work(this);
        with_state(this, |list, s| {
            s.draw(list, vertices, instances, first, first_instance)
        });
    }
    pub(super) extern "system" fn dispatch(this: This, x: u32, y: u32, z: u32) {
        super::count_dispatch();
        super::mark_work(this);
        with_state(this, |list, s| s.dispatch(list, x, y, z));
    }
    /// `DispatchMesh(x, y, z)` (the PC method of List6); the title's mesh
    /// programs take `x` groups only.
    pub(super) extern "system" fn dispatch_mesh(this: This, x: u32, y: u32, z: u32) {
        super::count_draw();
        super::mark_work(this);
        if y * z != 1 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(x, y, z, "a mesh dispatch with y or z groups"));
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    /// `DispatchMeshX(x, flags, indirect arguments, payload)`: dispatches `x`
    /// mesh groups directly, or one indirect mesh dispatch from `indirect`
    /// when non-zero.
    pub(super) extern "system" fn dispatch_mesh_x(
        this: This,
        x: u32,
        _flags: u64,
        indirect: u64,
        _payload: u64,
    ) {
        super::count_draw();
        super::mark_work(this);
        if indirect != 0 {
            const MESH: u32 = 10;
            let mut ran = false;
            let mut dropped = "";
            with_state(this, |list, s| {
                ran = s.execute_indirect(list, (MESH, 12, 1), indirect, None, &mut dropped);
            });
            if !ran {
                super::INDIRECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    pub(super) extern "system" fn draw_indexed(
        this: This,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        super::count_draw();
        super::mark_work(this);
        with_state(this, |list, s| {
            s.draw_indexed(list, indices, instances, first, base, first_instance)
        });
    }
}

/// Without a host GPU every draw state call is ignored.
#[cfg(not(windows))]
mod state {
    pub(super) use super::dispatch;
    pub(super) use super::draw;
    pub(super) use super::draw as draw_indexed;
    pub(super) use super::draw as dispatch_mesh;
    pub(super) use super::draw as dispatch_mesh_x;
    pub(super) use super::ignored_state as blend_factor;
    pub(super) use super::ignored_state as compute_constant;
    pub(super) use super::ignored_state as compute_constants;
    pub(super) use super::ignored_state as compute_table;
    pub(super) use super::ignored_state as compute_view;
    pub(super) use super::ignored_state as graphics_constant;
    pub(super) use super::ignored_state as graphics_constants;
    pub(super) use super::ignored_state as graphics_table;
    pub(super) use super::ignored_state as graphics_view;
    pub(super) use super::ignored_state as heaps;
    pub(super) use super::ignored_state as index_buffer;
    pub(super) use super::ignored_state as scissors;
    pub(super) use super::ignored_state as stencil_ref;
    pub(super) use super::ignored_state as targets;
    pub(super) use super::ignored_state as topology;
    pub(super) use super::ignored_state as vertex_buffers;
    pub(super) use super::ignored_state as viewports;
}

fn count_draw() {
    DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Draws of the kinds the host does not run yet (indirect, mesh), and
/// dispatches, counted per frame for the log.
/// shortcut: dropped until their shaders translate (decision 0007).
#[cfg(not(windows))]
extern "system" fn draw(_: This) {
    count_draw();
}

/// `ExecuteIndirect(signature, max count, arguments, offset, count buffer,
/// offset)`. The title's signatures (title+0x2479466, 2026-11-24)
/// hold one argument each: draw (stride 16), indexed draw (20), dispatch
/// (12), mesh dispatch (12), and the console's single-pass dispatch, type
/// 105 (stride 12: the three group counts; its flags live in the
/// signature, the title passes none), run as a plain dispatch. They run
/// through the host's indirect execution (`State::execute_indirect`).
/// shortcut: signatures that change root arguments are dropped (logged
/// once per signature).
extern "system" fn execute_indirect(
    this: This,
    signature: This,
    max: u32,
    arguments: This,
    offset: u64,
    count: This,
    count_offset: u64,
) {
    count_draw();
    #[cfg(windows)]
    mark_work(this);
    let Some((stride, args)) = crate::signature::layout(signature) else {
        return;
    };
    let address = crate::resource::buffer_bytes(arguments).map(|(p, _)| p as u64 + offset);
    let counter = crate::resource::buffer_bytes(count).map(|(p, _)| p as u64 + count_offset);
    let mut ran = false;
    let mut dropped = "";
    #[cfg(windows)]
    if let ([[kind, ..]], Some(address)) = (args, address)
        && (count.is_null() || counter.is_some())
    {
        const DISPATCH: u32 = 2;
        const DISPATCH_SINGLE: u32 = 105;
        let kind = if *kind == DISPATCH_SINGLE {
            DISPATCH
        } else {
            *kind
        };
        with_state(this, |list, s| {
            ran = s.execute_indirect(list, (kind, stride, max), address, counter, &mut dropped);
        });
    } else {
        dropped = if args.len() != 1 {
            "a signature of more than one argument"
        } else if address.is_none() {
            "arguments outside title buffers"
        } else {
            "count outside title buffers"
        };
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::pipeline::note_why(format!(
            "indirect signature {args:?}: arguments in a buffer {}, count in a buffer {}",
            address.is_some(),
            count.is_null() || counter.is_some()
        ));
    }
    if ran {
        return;
    }
    INDIRECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Once per signature and reason: what the records hold is only worth
    // saying when the reason is new too.
    static SEEN: std::sync::Mutex<Vec<(usize, &'static str)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&(signature as usize, dropped)) {
        seen.push((signature as usize, dropped));
        // The records from a host copy that already exists, never one made
        // here: its bytes are as of that copy's last refresh, before this
        // submission and any producer earlier in this list. A buffer with no
        // copy reads as absent rather than as zeroes.
        #[cfg(windows)]
        let records = address.and_then(|at| {
            let shadow = crate::shadow::peek(at, u64::from(stride))?;
            let bytes = shadow.bytes(at, u64::from(stride))?;
            // A stride that is not a multiple of four leaves a tail out
            // rather than panicking in this callback.
            let (words, _tail) = bytes.as_chunks::<4>();
            Some(
                words
                    .iter()
                    .map(|w| u32::from_le_bytes(*w))
                    .collect::<Vec<u32>>(),
            )
        });
        #[cfg(not(windows))]
        let records: Option<Vec<u32>> = None;
        tracing::info!(
            stride,
            // The command signature's descriptor, not the records: its
            // fields past the type are whatever the title left in the union.
            signature = ?args,
            ?records,
            max,
            address = ?address.map(|a| format!("{a:#x}")),
            counted = !count.is_null(),
            reason = dropped,
            "indirect execution dropped (first time for this signature)"
        );
    }
}

/// Indirect executions dropped since the last call.
pub(crate) fn take_indirect() -> u64 {
    INDIRECT.swap(0, std::sync::atomic::Ordering::Relaxed)
}
static INDIRECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn count_dispatch() {
    DISPATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(windows))]
extern "system" fn dispatch(_: This) {
    count_dispatch();
}
static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DISPATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Draws (any kind, mesh and indirect included) and compute dispatches
/// recorded since the last call.
pub(crate) fn take_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (DRAWS.swap(0, Relaxed), DISPATCHES.swap(0, Relaxed))
}

/// A GPU wait for a 32-bit `(address, comparison, reference, mask)` or
/// 64-bit `(address, comparison, reference, flags)` memory value: splits
/// a recording with earlier work into a closed host list before the wait and
/// a fresh open host list after it.
extern "system" fn wait32(this: This, address: u64, comparison: u32, reference: u32, mask: u32) {
    record_wait(this, address, comparison, u64::from(reference), mask, 4);
}

extern "system" fn wait64(this: This, address: u64, comparison: u32, reference: u64, _flags: u32) {
    record_wait(this, address, comparison, reference, u32::MAX, 8);
}

/// Maximum number of mid-list host command-list splits per title list.
/// Real titles issue only a handful of mid-list memory waits per list; capping
/// splits bounds host list allocation and `segments()` cloning on pathological
/// wait chains.
#[cfg(windows)]
const MAX_WAIT_SPLITS: usize = 64;

fn record_wait(this: This, address: u64, comparison: u32, reference: u64, mask: u32, width: u8) {
    const ALWAYS: u32 = 8;
    if address == 0 || comparison == ALWAYS {
        return;
    }
    if !(2..=7).contains(&comparison) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(comparison, "unsupported GPU memory wait comparison skipped");
        });
        return;
    }
    #[cfg(windows)]
    if !this.is_null() {
        // SAFETY: the title passes one of our lists.
        let list = unsafe { &*this.cast::<List>() };
        let mut host = list.host.lock().unwrap_or_else(|e| e.into_inner());
        let mut segments = list.segments.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = list.state.lock().unwrap_or_else(|e| e.into_inner());
        let splits = segments
            .iter()
            .filter(|s| matches!(s, Segment::List(_)))
            .count();
        if list.has_work.load(std::sync::atomic::Ordering::Acquire) {
            if splits >= MAX_WAIT_SPLITS {
                static CAPPED: std::sync::Once = std::sync::Once::new();
                CAPPED.call_once(|| {
                    tracing::warn!(
                        cap = MAX_WAIT_SPLITS,
                        "mid-list GPU memory wait split cap reached; skipping further splits"
                    );
                });
                return;
            }
            if list
                .has_work
                .swap(false, std::sync::atomic::Ordering::AcqRel)
                && let Some(prev) = host.take()
            {
                prev.close();
                segments.push(Segment::List(prev.clone()));
                match crate::host::host().and_then(|h| h.acquire_list().ok()) {
                    Some(new) => {
                        let new = std::sync::Arc::new(new);
                        state.reapply_host(&new, &prev);
                        *host = Some(new);
                    }
                    None => {
                        tracing::warn!("no new host list after mid-list wait split");
                    }
                }
            }
        }
        segments.push(Segment::Wait(crate::timeline::MemoryWait {
            address,
            reference,
            mask,
            width,
            comparison,
        }));
    }
    #[cfg(not(windows))]
    let _ = (this, reference, mask, width);
}

/// The enhanced barriers `(group count, groups)`: buffer, texture and
/// global barriers with layouts instead of states. Every host use of a
/// title texture (clears, copies) moves its host copy to the state it needs
/// first, so only their ordering matters: one barrier on every shader
/// write (the title's buffer barriers order compute passes).
extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    #[cfg(windows)]
    if count > 0
        && let Some(list) = host_list(this)
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
        list.mark_work();
        // SAFETY: the same layout; no resource named.
        let barrier =
            unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
        // SAFETY: an open list.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        mark_work(this);
    }
    #[cfg(not(windows))]
    let _ = (this, count);
}

/// The bottom-of-pipe value writes `(address, value, flush, flags)`, 32 and
/// 64 bits: done when the list has run, as the GPU would. Flag 1 writes the
/// GPU timestamp instead (64-bit form), flag 2 the system timestamp.
extern "system" fn write32(this: This, address: u64, value: u32, _flush: u32, _flags: u32) {
    defer_write(this, address, Written::Value(u64::from(value)), 4);
}
extern "system" fn write64(this: This, address: u64, value: u64, _flush: u32, flags: u32) {
    const GPU_TIMESTAMP: u32 = 1;
    const SYSTEM_TIMESTAMP: u32 = 2;
    let what = match flags {
        GPU_TIMESTAMP => Written::GpuTimestamp,
        SYSTEM_TIMESTAMP => Written::SystemTimestamp,
        _ => Written::Value(value),
    };
    defer_write(this, address, what, 8);
}

fn defer_write(this: This, address: u64, what: Written, bytes: usize) {
    if address == 0 {
        return;
    }
    #[cfg(windows)]
    if let Some(list) = host_list(this) {
        list.defer_write(address as *mut u8, what, bytes);
        mark_work(this);
        return;
    }
    let _ = this;
    // Without a host GPU nothing runs later: write now (no lock held).
    what.write(address as *mut u8, bytes).run();
}

/// The list methods by their slot in older titles' lists (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, list 1, graphics list 96
/// (59 as on PC, then console ones), then versions 1-6 (6 + 1 + 1 + 11 + 3
/// + 13): 140 slots.
const METHODS: [(usize, *const ()); 53] = [
    (0, list_query as *const ()),
    (2, list_release as *const ()),
    (7, object::parent_device as *const ()),      // 0x38
    (8, list_type as *const ()),                  // 0x40
    (9, close as *const ()),                      // 0x48
    (10, reset as *const ()),                     // 0x50
    (12, state::draw as *const ()),               // 0x60 instanced
    (13, state::draw_indexed as *const ()),       // 0x68 indexed
    (14, state::dispatch as *const ()),           // 0x70
    (15, copy_buffer_region as *const ()),        // 0x78
    (16, copy_texture_region as *const ()),       // 0x80
    (17, copy_resource as *const ()),             // 0x88
    (19, resolve as *const ()),                   // 0x98
    (20, state::topology as *const ()),           // 0xa0
    (21, state::viewports as *const ()),          // 0xa8
    (22, state::scissors as *const ()),           // 0xb0
    (23, state::blend_factor as *const ()),       // 0xb8
    (24, state::stencil_ref as *const ()),        // 0xc0
    (25, set_pipeline as *const ()),              // 0xc8
    (26, barrier as *const ()),                   // 0xd0
    (32, state::heaps as *const ()),              // 0x100
    (33, ignored_state as *const ()),             // 0x108 compute root signature
    (34, ignored_state as *const ()),             // 0x110 graphics root signature
    (35, state::compute_table as *const ()),      // 0x118
    (36, state::graphics_table as *const ()),     // 0x120
    (37, state::compute_constant as *const ()),   // 0x128
    (38, state::graphics_constant as *const ()),  // 0x130
    (39, state::compute_constants as *const ()),  // 0x138
    (40, state::graphics_constants as *const ()), // 0x140
    (41, state::compute_view as *const ()),       // 0x148 CBV
    (42, state::graphics_view as *const ()),      // 0x150 CBV
    (43, state::compute_view as *const ()),       // 0x158 SRV
    (44, state::graphics_view as *const ()),      // 0x160 SRV
    (45, state::compute_view as *const ()),       // 0x168 UAV
    (46, state::graphics_view as *const ()),      // 0x170 UAV
    (47, state::index_buffer as *const ()),       // 0x178
    (48, state::vertex_buffers as *const ()),     // 0x180
    (50, state::targets as *const ()),            // 0x190
    (51, clear_dsv as *const ()),                 // 0x198
    (52, clear_rtv as *const ()),                 // 0x1a0
    (55, discard_resource as *const ()),          // 0x1b8
    (59, execute_indirect as *const ()),          // 0x1d8 indirect
    (60, copy_memory as *const ()),               // 0x1e0
    (68, ignored_state as *const ()),             // 0x220 depth bounds
    (69, write32 as *const ()),                   // 0x228
    (70, write64 as *const ()),                   // 0x230
    (71, wait32 as *const ()),                    // 0x238, 32 bits
    (72, wait64 as *const ()),                    // 0x240, 64 bits
    (73, ignored_state as *const ()),             // 0x248 pixel shader depth order
    (83, copy_texture_region_x as *const ()),     // 0x298
    (84, copy_resource_x as *const ()),           // 0x2a0
    (124, ignored_state as *const ()),            // 0x3e0 shading rate
    (127, state::dispatch_mesh as *const ()),     // 0x3f8 mesh dispatch
];
/// The rest of [`METHODS`], kept apart only to stay readable.
const MORE_METHODS: [(usize, *const ()); 9] = [
    (28, query_mark as *const ()),             // 0xe0 BeginQuery
    (29, query_mark as *const ()),             // 0xe8 EndQuery
    (30, resolve_queries as *const ()),        // 0xf0
    (78, dispatch_x as *const ()),             // 0x270
    (98, state::dispatch_mesh_x as *const ()), // 0x310
    (99, driver_hint as *const ()),            // 0x318
    // Pipeline syncs at the top and bottom of the pipe, 0x408 and 0x410.
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, reset_x as *const ()), // 0x448
];

/// Newer titles' lists have two more methods in the console part of the
/// base list, after the clears (0x198/0x1a0 as before) and before
/// ExecuteIndirect, which moved from 0x1d8 to 0x1e8: every slot from 0x1d8
/// on is two later (taken here as starting exactly at 0x1d8). Inferred from
/// The title's call shapes: ExecuteIndirect 0x1e8, bottom-of-pipe
/// writes 0x238/0x240, waits 0x248/0x250, depth bounds 0x230, shading rate
/// 0x3f0/0x400, syncs 0x418/0x420, ResetX 0x458 (asserts on failure).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 59 { slot + 2 } else { slot }
}

/// Methods only newer titles' lists have. 0x490: the enhanced barrier
/// `(group count, groups)`, with 16-byte groups `(type, count, barriers)`
/// and 40-byte buffer barriers (title+0x95e0 builds them). 0x470 `(u64)`,
/// called from the title's state flush with its result unused.
/// shortcut: 0x470's purpose is unknown; ignored until a title shows it
/// matters.
const NEWER_METHODS: [(usize, *const ()); 3] = [
    (142, ignored_state as *const ()),
    (146, barrier_groups as *const ()),
    (148, allocate_upload as *const ()),
];

const fn list_table(newer: bool) -> [*const (); 160] {
    let mut f = object::d3d12_object(object::missing_table::<LIST, 160>());
    let mut i = 0;
    while i < METHODS.len() + MORE_METHODS.len() {
        let (slot, method) = if i < METHODS.len() {
            METHODS[i]
        } else {
            MORE_METHODS[i - METHODS.len()]
        };
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    let mut i = 0;
    while newer && i < NEWER_METHODS.len() {
        f[NEWER_METHODS[i].0] = NEWER_METHODS[i].1;
        i += 1;
    }
    f
}

/// Every slot up to the limit exists and logs, for the methods newer
/// titles add.
static LIST_FUNCTIONS: Table<160> = Table(list_table(false));
static NEWER_LIST_FUNCTIONS: Table<160> = Table(list_table(true));

#[cfg(all(test, windows))]
pub(crate) fn test_slot(list: This, slot_index: usize) -> usize {
    // SAFETY: one of our command lists; its first field is the vtable pointer.
    let table = unsafe { *list.cast::<*const *const ()>() };
    let resolved = if table == NEWER_LIST_FUNCTIONS.0.as_ptr() {
        newer_slot(slot_index)
    } else {
        slot_index
    };
    resolved * size_of::<*const ()>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn command_list_creation_keeps_the_initial_pipeline() {
        use windows::Win32::Graphics::Direct3D12::ID3D12PipelineState;
        use windows::core::Interface;

        let null = std::ptr::null();
        let iid = ID3D12PipelineState::IID;
        // SAFETY: ComputeHead contains only raw pointers and a byte count. No
        // shader is needed to verify which pipeline object the list remembers.
        let desc: crate::pipeline::ComputeHead = unsafe { std::mem::zeroed() };
        let mut pipeline = null;
        let mut allocator = null;
        assert_eq!(
            crate::pipeline::create_compute(null, &desc, (&raw const iid).cast(), &mut pipeline),
            S_OK
        );
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let desc = ListDesc {
            kind: 0,
            flags: 0,
            nodes: 0,
            barrier_batch: 0,
        };
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        let bound = |list: This| {
            // SAFETY: the test owns this live command list.
            unsafe { &*list.cast::<List>() }
                .state
                .lock()
                .unwrap()
                .current_pipeline()
        };
        let created_with = [false, true].map(|extended| {
            let mut list = null;
            let hr = if extended {
                create_list_x(null, &desc, allocator, pipeline, &LIST_IIDS[1], &mut list)
            } else {
                create_list(null, 0, 0, allocator, pipeline, &LIST_IIDS[1], &mut list)
            };
            assert_eq!(hr, S_OK);
            let initial = bound(list);
            if host_list(list).is_some() {
                assert_eq!(close(list), S_OK);
                assert_eq!(reset_x(list, allocator, null, 0), S_OK);
                assert_eq!(bound(list), null);
                assert_eq!(close(list), S_OK);
                assert_eq!(reset(list, allocator, pipeline), S_OK);
                assert_eq!(bound(list), pipeline);
            }
            assert_eq!(release(list), 0);
            initial
        });
        assert_eq!(release(allocator), 0);
        assert_eq!(release(pipeline), 0);
        assert_eq!(
            created_with,
            [pipeline, pipeline],
            "initial PSO for CreateCommandList and CreateCommandListX"
        );
    }

    #[test]
    fn discard_resource_slot_is_stable() {
        assert_eq!(LIST_FUNCTIONS.0[55], discard_resource as *const ());
        assert_eq!(NEWER_LIST_FUNCTIONS.0[55], discard_resource as *const ());
    }

    #[test]
    fn discard_region_validation() {
        let rect = Rect {
            left: 1,
            top: 1,
            right: 4,
            bottom: 4,
        };
        let valid = DiscardRegion {
            num_rects: 1,
            rects: &rect,
            first_subresource: 1,
            num_subresources: 1,
        };
        let whole_subresource = DiscardRegion {
            num_rects: 0,
            rects: std::ptr::null(),
            ..valid
        };
        let check = |region: Option<&DiscardRegion>, dimension, mips, slices, planes| {
            // SAFETY: each non-null rectangle pointer names one live rectangle.
            unsafe { valid_discard_region(region, dimension, mips, slices, planes, [8, 8]) }
        };
        assert!(check(None, 3, 2, 3, 1));
        assert!(check(Some(&valid), 3, 2, 3, 1));
        assert!(!check(Some(&valid), 2, 2, 3, 1)); // 1D subresources
        assert!(!check(Some(&valid), 4, 2, 3, 1)); // 3D subresources
        assert!(check(Some(&whole_subresource), 4, 2, 3, 1));
        assert!(check(Some(&whole_subresource), 3, 1, 1, 2)); // stencil plane
        assert!(!check(Some(&whole_subresource), 3, 1, 1, 1));

        for (first, count) in [(5, 2), (6, 1), (u32::MAX, 2), (1, u32::MAX), (1, 0)] {
            let bad_range = DiscardRegion {
                first_subresource: first,
                num_subresources: count,
                ..whole_subresource
            };
            assert!(!check(Some(&bad_range), 3, 2, 3, 1));
        }
        let last_slice = DiscardRegion {
            first_subresource: 5,
            ..whole_subresource
        };
        assert!(check(Some(&last_slice), 3, 2, 3, 1));
        assert!(!check(Some(&last_slice), 4, 2, 3, 1)); // depth is not an array size

        let null_rects = DiscardRegion {
            rects: std::ptr::null(),
            ..valid
        };
        assert!(!check(Some(&null_rects), 3, 2, 3, 1));
        let different_mips = DiscardRegion {
            num_subresources: 2,
            ..valid
        };
        assert!(!check(Some(&different_mips), 3, 2, 3, 1));
        assert!(check(Some(&different_mips), 3, 1, 3, 1));
        for right in [0, 1, 5] {
            let rect = Rect { right, ..rect };
            let bad_rect = DiscardRegion {
                rects: &rect,
                ..valid
            };
            assert!(!check(Some(&bad_rect), 3, 2, 3, 1));
        }
    }

    #[test]
    fn discard_requires_matching_state_and_flags() {
        for (state, flag) in [(0x4, 1), (0x10, 2), (0x8, 4)] {
            assert!(discard_state_allowed(state, flag));
            assert!(discard_state_allowed(state, flag | 0x20)); // simultaneous access
            assert!(!discard_state_allowed(state, 0x7 ^ flag));
            assert!(!discard_state_allowed(state | 0x80, flag));
        }
        for state in [0, 0x400, 0x80] {
            assert!(!discard_state_allowed(state, 0x7));
        }
        // A render target or depth stencil that also allows unordered access
        // is not discarded in the UAV state.
        assert!(!discard_state_allowed(0x8, 0x5));
        assert!(!discard_state_allowed(0x8, 0x6));
        assert!(discard_state_allowed(0x4, 0x5));
    }

    #[cfg(windows)]
    #[test]
    fn discard_resource_executes_on_guest_queue() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4,
        };

        let Some(host) = crate::host::host() else {
            eprintln!("skipped live discard: no host D3D12 device");
            return;
        };
        // SAFETY: plain DXGI queries for the device the guest queue uses.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(host.device.GetAdapterLuid())
                .unwrap();
            let desc = adapter.GetDesc1().unwrap();
            let name = String::from_utf16_lossy(&desc.Description);
            eprintln!("live discard adapter: {}", name.trim_end_matches('\0'));
        }

        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let resource_iid = guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let make_resource = |desc: D3D12_RESOURCE_DESC| {
            let mut resource = null;
            assert_eq!(
                crate::resource::create_committed(
                    null,
                    (&raw const heap).cast(),
                    0,
                    (&raw const desc).cast(),
                    0,
                    null.cast(),
                    &resource_iid,
                    &mut resource,
                ),
                S_OK
            );
            resource
        };
        let make_texture = |format, flags| {
            let resource = make_resource(D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 8,
                Height: 8,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            });
            assert!(
                crate::resource::host_resource(resource).is_some(),
                "missing host texture for {format:?}"
            );
            resource
        };
        let rt = make_texture(
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        );
        let ds = make_texture(
            DXGI_FORMAT_D24_UNORM_S8_UINT,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        );
        let uav = make_texture(
            DXGI_FORMAT_R32_UINT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        );
        let srv = make_texture(DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_NONE);
        let buffer = make_resource(D3D12_RESOURCE_DESC {
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
            ..Default::default()
        });
        assert!(crate::resource::buffer_bytes(buffer).is_some());

        let (mut queue, mut allocator, mut list) = (null, null, null);
        let queue_iid = guid(
            0x0ec870a6,
            0x5d7e,
            0x4c22,
            [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
        );
        let queue_desc = [0_u32; 4];
        assert_eq!(
            crate::queue::create(null, queue_desc.as_ptr().cast(), &queue_iid, &mut queue),
            S_OK
        );
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );
        let host_list = host_list(list).unwrap();
        let discard = slot::<extern "system" fn(This, This, *const DiscardRegion)>(list, 0x1b8);
        discard(list, null, null.cast());
        discard(list, buffer, null.cast());
        ensure_state(
            &host_list,
            srv,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0 as u32,
        );
        discard(list, srv, null.cast());
        discard(list, rt, null.cast()); // no state established by this list yet

        ensure_state(&host_list, rt, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32);
        discard(list, rt, null.cast());
        let rect = Rect {
            left: 1,
            top: 2,
            right: 5,
            bottom: 6,
        };
        discard(
            list,
            rt,
            &DiscardRegion {
                num_rects: 1,
                rects: &rect,
                first_subresource: 0,
                num_subresources: 1,
            },
        );
        let mut descriptor = [0_u64; 14];
        let handle = descriptor.as_mut_ptr() as u64;
        crate::descriptor::create_rtv(null, rt, std::ptr::null(), handle);
        let clear =
            slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const Rect)>(list, 0x1a0);
        clear(list, handle, &[0.25, 0.5, 0.75, 1.0], 0, std::ptr::null());

        ensure_state(&host_list, ds, D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32);
        for first_subresource in [0, 1] {
            discard(
                list,
                ds,
                &DiscardRegion {
                    num_rects: 0,
                    rects: std::ptr::null(),
                    first_subresource,
                    num_subresources: 1,
                },
            );
        }
        ensure_state(
            &host_list,
            uav,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32,
        );
        discard(list, uav, null.cast());

        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
            S_OK
        );
        assert_eq!(host.log_messages(), None);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);
        // SAFETY: the guest queue waits for submission and the device is live.
        unsafe { host.device.GetDeviceRemovedReason().unwrap() };

        for object in [list, allocator, queue, rt, ds, uav, srv, buffer] {
            assert_eq!(
                slot::<extern "system" fn(This) -> u32>(object, 0x10)(object),
                0
            );
        }
    }

    #[test]
    fn every_inline_command_falls_back_to_the_table() {
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        let hr = create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list);
        assert_eq!(hr, S_OK);
        // What the title's inline draw checks: put < draw limit.
        // SAFETY: the list is 0x560 bytes.
        let (put, draw_limit, api_limit) = unsafe {
            let at = |o: usize| list.cast::<u8>().add(o).cast::<u64>().read();
            (at(0x18), at(0x28), at(0x30))
        };
        assert!(put >= draw_limit && put >= api_limit);
        // The console's ID3D12GraphicsCommandList6, as the title asks.
        let mut same = null;
        assert_eq!(list_query(list, &LIST_IIDS[7], &mut same), S_OK);
        assert_eq!(same, list);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x10)(list), 1);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x40)(list), 0);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
            S_OK
        );
        // List-owned upload memory: aligned, writable, gone at reset.
        let p = allocate_upload(list, 40, 0x100);
        assert!(!p.is_null() && (p as usize).is_multiple_of(0x100));
        // SAFETY: 40 bytes were handed out.
        unsafe { p.write_bytes(0xab, 40) };
        assert!(
            allocate_upload(list, 8, 0x30).is_null(),
            "not a power of two"
        );
        assert_eq!(reset(list, allocator, null), S_OK);
        assert!(
            // SAFETY: the list is ours.
            unsafe { &(*list.cast::<List>()).uploads }
                .lock()
                .unwrap()
                .is_empty()
        );
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }

    /// Resetting or dropping a list whose submission is held behind a fence
    /// wait keeps its upload blocks alive until the submission runs, and
    /// recycles the detached host list back into the host's pool.
    #[cfg(windows)]
    #[test]
    fn pending_list_reset_and_drop_keep_uploads_and_recycle_host_list() {
        use windows::core::Interface;

        let Some(host) = crate::host::host() else {
            return;
        };
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);
        let null = std::ptr::null();
        let (waits, sig_slot) = crate::queue::test_queue();
        let wait = slot::<Sync>(waits, sig_slot + 8);
        let signal = slot::<Sync>(waits, sig_slot);
        let execute = slot::<Execute>(waits, 0x50);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);

        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        // Hold the queue, record a copy from list upload memory, submit, then
        // reset the list while its submission is still waiting.
        assert_eq!(wait(waits, ready, 1), S_OK);
        let src1 = allocate_upload(list, 64, 16);
        assert!(!src1.is_null());
        // SAFETY: 64 bytes handed out above.
        unsafe { src1.write_bytes(0x3c, 64) };
        let mut dst1 = [0u8; 64];
        copy_memory(list, dst1.as_mut_ptr() as u64, src1 as u64, 64, 0);
        assert_eq!(close(list), S_OK);
        let raw1 = host_list(list).unwrap().list.as_raw() as usize;
        execute(waits, 1, &list);
        assert_eq!(signal(waits, done, 1), S_OK);
        assert_eq!(reset(list, allocator, null), S_OK);
        let raw2 = host_list(list).unwrap().list.as_raw() as usize;
        assert_ne!(raw2, raw1, "reset must detach from the pending host list");
        assert_eq!(dst1, [0u8; 64]);

        let reused_before = host.reused_lists();
        assert_eq!(crate::fence::signal(ready, 1), S_OK);
        assert_eq!(completed(done), 1);
        assert_eq!(dst1, [0x3c; 64], "kept upload memory reached the copy");

        // Now hold behind ready == 2: reset `list` again (reusing a pooled
        // host list) and also drop a second list while its submission waits.
        assert_eq!(wait(waits, ready, 2), S_OK);
        assert_eq!(close(list), S_OK);
        execute(waits, 1, &list);
        assert_eq!(reset(list, allocator, null), S_OK);
        assert!(
            host.reused_lists() > reused_before,
            "detached host list should be recycled from the pool"
        );

        let mut list2 = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list2),
            S_OK
        );
        let src2 = allocate_upload(list2, 64, 16);
        assert!(!src2.is_null());
        // SAFETY: 64 bytes handed out above.
        unsafe { src2.write_bytes(0x5a, 64) };
        let mut dst2 = [0u8; 64];
        copy_memory(list2, dst2.as_mut_ptr() as u64, src2 as u64, 64, 0);
        assert_eq!(close(list2), S_OK);
        execute(waits, 1, &list2);
        assert_eq!(signal(waits, done, 2), S_OK);
        // Drop `list2` while its submission is still held back.
        assert_eq!(release(list2), 0);
        assert_eq!(dst2, [0u8; 64]);

        assert_eq!(crate::fence::signal(ready, 2), S_OK);
        assert_eq!(completed(done), 2);
        assert_eq!(
            dst2, [0x5a; 64],
            "dropping a pending list must keep its upload memory until run"
        );

        for object in [list, allocator, waits, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    fn list_slot(list: This, slot_index: usize) -> usize {
        test_slot(list, slot_index)
    }

    #[cfg(windows)]
    #[test]
    fn mid_list_write_wait_write_orders_across_memory_wait() {
        use std::sync::atomic::{AtomicU32, Ordering};

        if crate::host::host().is_none() {
            return;
        }
        type Execute = extern "system" fn(This, u32, *const This);
        type Write32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait32 = extern "system" fn(This, u64, u32, u32, u32);

        let null = std::ptr::null();
        let (queue, _sig_slot) = crate::queue::test_queue();
        let execute = slot::<Execute>(queue, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        let first = AtomicU32::new(0);
        let second = AtomicU32::new(0);
        let gate = std::sync::Arc::new(AtomicU32::new(0));

        let call_write32 = slot::<Write32>(list, list_slot(list, 69));
        let call_wait32 = slot::<Wait32>(list, list_slot(list, 71));

        call_write32(list, (&raw const first) as u64, 11, 0, 0);
        // EQUAL (3): wait until gate == 42 (masked with u32::MAX).
        call_wait32(list, (&raw const *gate) as u64, 3, 42, u32::MAX);
        call_write32(list, (&raw const second) as u64, 22, 0, 0);
        assert_eq!(close(list), S_OK);

        execute(queue, 1, &list);

        // First segment ran immediately before the wait; second segment stays blocked.
        assert_eq!(first.load(Ordering::Acquire), 11);
        assert_eq!(second.load(Ordering::Acquire), 0);

        let gate_thread = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            gate_thread.store(42, Ordering::Release);
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while second.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(second.load(Ordering::Acquire), 22);

        for object in [list, allocator, queue] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn indirect_dispatch_mesh_x_routes_through_execute_indirect() {
        if crate::host::host().is_none() {
            return;
        }
        type DispatchMeshX = extern "system" fn(This, u32, u64, u64, u64);

        let null = std::ptr::null();
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        let _ = take_indirect();
        let call_mesh_x = slot::<DispatchMeshX>(list, list_slot(list, 98));
        // Non-zero indirect address outside shadow buffers increments INDIRECT via execute_indirect.
        call_mesh_x(list, 0, 0, 0x1000, 0);
        assert_eq!(take_indirect(), 1);
        assert!(
            // SAFETY: the list is ours.
            unsafe { &(*list.cast::<List>()).has_work }.load(std::sync::atomic::Ordering::Acquire),
            "indirect DispatchMeshX must mark has_work"
        );

        let mut gate = 0u32;
        let call_wait32 =
            slot::<extern "system" fn(This, u64, u32, u32, u32)>(list, list_slot(list, 71));
        call_wait32(list, (&raw mut gate) as u64, 3, 1, u32::MAX);
        assert!(
            matches!(
                segments(list).as_slice(),
                [Segment::List(_), Segment::Wait(_)]
            ),
            "indirect DispatchMeshX followed by wait must emit [List, Wait]"
        );

        // Direct DispatchMeshX also marks has_work.
        call_mesh_x(list, 1, 0, 0, 0);
        assert!(
            // SAFETY: the list is ours.
            unsafe { &(*list.cast::<List>()).has_work }.load(std::sync::atomic::Ordering::Acquire),
            "direct DispatchMeshX must mark has_work"
        );

        assert_eq!(close(list), S_OK);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }

    #[cfg(windows)]
    #[test]
    fn leading_and_consecutive_waits_produce_no_empty_host_segments() {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

        if crate::host::host().is_none() {
            return;
        }
        type SetPipeline = extern "system" fn(This, This);
        type Topology = extern "system" fn(This, u32);
        type Viewports = extern "system" fn(This, u32, *const D3D12_VIEWPORT);
        type Scissors = extern "system" fn(This, u32, *const RECT);
        type BlendFactor = extern "system" fn(This, *const [f32; 4]);
        type StencilRef = extern "system" fn(This, u32);
        type Write32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait64 = extern "system" fn(This, u64, u32, u64, u32);

        let null = std::ptr::null();
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        // Pure state setters must not mark has_work.
        slot::<SetPipeline>(list, list_slot(list, 25))(list, null);
        slot::<Topology>(list, list_slot(list, 20))(list, 4);
        let vp = D3D12_VIEWPORT {
            Width: 8.0,
            Height: 8.0,
            MaxDepth: 1.0,
            ..Default::default()
        };
        slot::<Viewports>(list, list_slot(list, 21))(list, 1, &vp);
        let rc = RECT {
            right: 8,
            bottom: 8,
            ..Default::default()
        };
        slot::<Scissors>(list, list_slot(list, 22))(list, 1, &rc);
        let blend = [1.0, 0.5, 0.25, 1.0];
        slot::<BlendFactor>(list, list_slot(list, 23))(list, &blend);
        slot::<StencilRef>(list, list_slot(list, 24))(list, 3);

        let call_write32 = slot::<Write32>(list, list_slot(list, 69));
        let call_wait32 = slot::<Wait32>(list, list_slot(list, 71));
        let call_wait64 = slot::<Wait64>(list, list_slot(list, 72));

        let mut mem32 = 0u32;
        let mut mem64 = 0u64;
        let addr32 = (&raw mut mem32) as u64;
        let addr64 = (&raw mut mem64) as u64;

        // Skipped waits: null address, NEVER (1), ALWAYS (8), unknown (99).
        call_wait32(list, 0, 3, 1, u32::MAX);
        call_wait32(list, addr32, 1, 1, u32::MAX);
        call_wait32(list, addr32, 8, 1, u32::MAX);
        call_wait32(list, addr32, 99, 1, u32::MAX);
        assert!(
            matches!(segments(list).as_slice(), [Segment::List(_)]),
            "skipped waits leave a single initial empty List segment only when no segments exist"
        );

        // Leading wait + consecutive wait produce [Wait, Wait] with no empty List segments.
        call_wait32(list, addr32, 3, 1, u32::MAX);
        call_wait64(list, addr64, 7, 2, 0);
        let segs = segments(list);
        assert_eq!(segs.len(), 2);
        assert!(matches!(segs[0], Segment::Wait(_)));
        assert!(matches!(segs[1], Segment::Wait(_)));

        // Work -> Wait -> Wait -> Work -> Wait produces [Wait, Wait, List, Wait, Wait, List, Wait].
        let mut out = 0u32;
        call_write32(list, (&raw mut out) as u64, 1, 0, 0);
        call_wait32(list, addr32, 3, 3, u32::MAX);
        call_wait64(list, addr64, 3, 4, 0);
        call_write32(list, (&raw mut out) as u64, 2, 0, 0);
        call_wait32(list, addr32, 3, 5, u32::MAX);

        let segs = segments(list);
        assert_eq!(segs.len(), 7);
        assert!(matches!(segs[0], Segment::Wait(_)));
        assert!(matches!(segs[1], Segment::Wait(_)));
        assert!(matches!(segs[2], Segment::List(_)));
        assert!(matches!(segs[3], Segment::Wait(_)));
        assert!(matches!(segs[4], Segment::Wait(_)));
        assert!(matches!(segs[5], Segment::List(_)));
        assert!(matches!(segs[6], Segment::Wait(_)));

        assert_eq!(close(list), S_OK);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }

    #[cfg(windows)]
    #[test]
    fn wait_comparison_modes_and_64_bit_mask_on_queue() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

        if crate::host::host().is_none() {
            return;
        }
        type Execute = extern "system" fn(This, u32, *const This);
        type Write32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait64 = extern "system" fn(This, u64, u32, u64, u32);

        let null = std::ptr::null();
        let (queue, _) = crate::queue::test_queue();
        let execute = slot::<Execute>(queue, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        let gate32 = AtomicU32::new(0xdead_0001);
        let gate64 = std::sync::Arc::new(AtomicU64::new(7));
        let done = std::sync::Arc::new(AtomicU32::new(0));

        let call_write32 = slot::<Write32>(list, list_slot(list, 69));
        let call_wait32 = slot::<Wait32>(list, list_slot(list, 71));
        let call_wait64 = slot::<Wait64>(list, list_slot(list, 72));

        // 1 (NEVER) and 8 (ALWAYS) must not block at all.
        call_wait32(list, (&raw const gate32) as u64, 1, 0, u32::MAX);
        call_wait32(list, (&raw const gate32) as u64, 8, 0, u32::MAX);
        // Masked 32-bit EQUAL: (0xdead_0001 & 0xffff) == 1 passes at once.
        call_wait32(list, (&raw const gate32) as u64, 3, 1, 0xffff);
        call_write32(list, (&raw const *done) as u64, 1, 0, 0);
        // 64-bit EQUAL: low 32 bits already 7, upper 32 bits differ -> blocks until full 64 bits match.
        call_wait64(
            list,
            (&raw const *gate64) as u64,
            3,
            0x0001_0000_0000_0007,
            0,
        );
        call_write32(list, (&raw const *done) as u64, 2, 0, 0);
        assert_eq!(close(list), S_OK);

        execute(queue, 1, &list);
        assert_eq!(done.load(Ordering::Acquire), 1);

        let g64 = gate64.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            g64.store(0x0001_0000_0000_0007, Ordering::Release);
        });
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while done.load(Ordering::Acquire) < 2 && std::time::Instant::now() < limit {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(done.load(Ordering::Acquire), 2);

        for object in [list, allocator, queue] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn pending_segmented_list_reset_and_drop_keep_uploads_until_last_segment_runs() {
        use std::sync::atomic::{AtomicU32, Ordering};

        if crate::host::host().is_none() {
            return;
        }
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);
        type Wait32 = extern "system" fn(This, u64, u32, u32, u32);
        type Write32 = extern "system" fn(This, u64, u32, u32, u32);

        let null = std::ptr::null();
        let (queue, sig_slot) = crate::queue::test_queue();
        let wait_fence = slot::<Sync>(queue, sig_slot + 8);
        let execute = slot::<Execute>(queue, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut ready = null;
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );

        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );

        let call_write32 = slot::<Write32>(list, list_slot(list, 69));
        let call_wait32 = slot::<Wait32>(list, list_slot(list, 71));

        // Reset case: upload used after mid-list wait stays alive while second segment is pending.
        let gate = AtomicU32::new(0);
        let src1 = allocate_upload(list, 32, 16);
        assert!(!src1.is_null());
        // SAFETY: 32 bytes handed out above.
        unsafe { src1.write_bytes(0x7a, 32) };
        let mut dst1 = [0u8; 32];
        let mut first_done = 0u32;
        call_write32(list, (&raw mut first_done) as u64, 1, 0, 0);
        call_wait32(list, (&raw const gate) as u64, 3, 1, u32::MAX);
        copy_memory(list, dst1.as_mut_ptr() as u64, src1 as u64, 32, 0);
        assert_eq!(close(list), S_OK);

        execute(queue, 1, &list);
        assert_eq!(first_done, 1);
        assert_eq!(dst1, [0u8; 32]);

        // Reset while second segment is held on the memory wait.
        assert_eq!(reset(list, allocator, null), S_OK);
        assert!(segments(list).len() == 1 && matches!(segments(list)[0], Segment::List(_)));
        assert!(
            // SAFETY: the list is ours.
            !unsafe { &(*list.cast::<List>()).has_work }.load(Ordering::Acquire)
        );
        assert_eq!(dst1, [0u8; 32]);

        gate.store(1, Ordering::Release);
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while dst1[0] == 0 && std::time::Instant::now() < limit {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(dst1, [0x7a; 32]);

        // Drop case: segment 1 pending behind queue fence, trailing wait leaves current non-pending.
        let mut list2 = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list2),
            S_OK
        );
        let src2 = allocate_upload(list2, 32, 16);
        assert!(!src2.is_null());
        // SAFETY: 32 bytes handed out above.
        unsafe { src2.write_bytes(0x4e, 32) };
        let mut dst2 = [0u8; 32];
        assert_eq!(wait_fence(queue, ready, 1), S_OK);
        copy_memory(list2, dst2.as_mut_ptr() as u64, src2 as u64, 32, 0);
        slot::<Wait32>(list2, list_slot(list2, 71))(
            list2,
            (&raw const gate) as u64,
            3,
            1,
            u32::MAX,
        );
        assert_eq!(close(list2), S_OK);
        execute(queue, 1, &list2);
        assert_eq!(release(list2), 0);
        assert_eq!(dst2, [0u8; 32]);

        assert_eq!(crate::fence::signal(ready, 1), S_OK);
        assert_eq!(dst2, [0x4e; 32]);

        for object in [list, allocator, queue, ready] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn idle_list_state_does_not_pin_retirement_while_recorded_commands_do() {
        let _retire = crate::host::isolated_retire_scope();
        use std::sync::atomic::Ordering;

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        // Fixed-function state alone leaves the list idle.
        let vp = windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT::default();
        let rect = windows::Win32::Foundation::RECT::default();
        let blend = [1.0f32; 4];
        state::topology(list, 4);
        state::viewports(list, 1, &vp);
        state::scissors(list, 1, &rect);
        state::blend_factor(list, &blend);
        state::stencil_ref(list, 0);
        set_pipeline(list, null);
        let (c_idle, d_idle) = crate::host::DropCounter::new();
        crate::host::retire(c_idle);
        assert_eq!(
            d_idle.load(Ordering::SeqCst),
            1,
            "state-only list must not pin retired objects"
        );

        // Recording a GPU barrier marks the list active until reset/drop.
        barrier_groups(list, 1, null.cast());
        let (c_busy, d_busy) = crate::host::DropCounter::new();
        crate::host::retire(c_busy);
        assert_eq!(
            d_busy.load(Ordering::SeqCst),
            0,
            "recorded barrier must pin retired objects"
        );
        assert_eq!(close(list), S_OK);
        assert_eq!(reset(list, allocator, null), S_OK);
        assert_eq!(d_busy.load(Ordering::SeqCst), 1);

        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }
}
