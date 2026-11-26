//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once, unless the queue waits for a fence
//! value not reached yet: then its work is held back until it is (see
//! [`crate::timeline`]).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::fence::Held;
use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;
use crate::timeline::{Op, Timeline};

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    /// The work, in order, held back behind waits.
    timeline: Arc<Timeline>,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        timeline: Arc::default(),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Pins a queued [`crate::host::HostList`] epoch until execution completes or
/// the unrun closure is dropped on early queue teardown.
#[cfg(windows)]
struct InFlight(Option<Arc<crate::host::HostList>>);

#[cfg(windows)]
impl InFlight {
    fn new(list: Arc<crate::host::HostList>) -> Self {
        list.begin_submit();
        Self(Some(list))
    }

    fn list(&self) -> &crate::host::HostList {
        self.0.as_deref().expect("in-flight host list")
    }

    fn take(mut self) -> Arc<crate::host::HostList> {
        let list = self.0.take().expect("in-flight host list");
        list.end_submit();
        list
    }
}

#[cfg(windows)]
impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(list) = self.0.take() {
            list.cancel_submit();
        }
    }
}

/// Runs the lists' host lists and waits for them (decision 0007), once
/// the queue's waits before them are reached. Contiguous host list segments
/// run in a single host submission, ordered around mid-list memory waits.
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        let segments: Vec<_> = lists
            .iter()
            .flat_map(|&l| crate::list::segments(l))
            .collect();
        let timeline = &queue(this).timeline;
        let mut batch = Vec::new();
        let flush = |batch: &mut Vec<InFlight>| {
            if batch.is_empty() {
                return;
            }
            // Held until they run: the title may reset its lists before.
            // Every flushed batch (including segments queued behind a fence or
            // mid-list memory wait) keeps its `InFlight` pin until execution
            // completes.
            let in_flight = std::mem::take(batch);
            timeline.submit(Op::Run(Box::new(move || {
                if let Some(host) = crate::host::host() {
                    crate::descriptor::sync_all_heaps();
                    let runs: Vec<_> = in_flight.iter().map(InFlight::list).collect();
                    host.execute(&runs);
                    for entry in in_flight {
                        let list = entry.take();
                        if let Ok(orphan) = Arc::try_unwrap(list) {
                            host.recycle_list(orphan);
                        }
                    }
                }
            })));
        };
        for seg in segments {
            match seg {
                crate::list::Segment::List(list) => batch.push(InFlight::new(list)),
                crate::list::Segment::Wait(wait) => {
                    flush(&mut batch);
                    timeline.submit(Op::WaitMemory(wait, None));
                }
            }
        }
        flush(&mut batch);
        return;
    }
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// PresentX plane layout. The frame token and resource array head each 0x48-byte record.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
    _remaining: [u64; 6],
}
const _: () = assert!(size_of::<Plane>() == 0x48);
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they return, so it signals at once.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
/// Signals once the queue's work before it has run.
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let fence = Held::new(fence);
    queue(this)
        .timeline
        .submit(Op::Run(Box::new(move || fence.signal(value))));
    S_OK
}
/// Holds the queue's later work back until the fence reaches `value`; the
/// caller does not wait.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    queue(this)
        .timeline
        .submit(Op::Wait(Held::new(fence), value));
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`; returns the
    /// queues it woke, to run with no lock held.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) -> crate::fence::Woken {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        // Use atomic Release stores when aligned so concurrent readers (such
        // as a fence's `AtomicU64` value) observe them without a data race.
        unsafe {
            if bytes == 4 {
                if (at as usize).is_multiple_of(align_of::<AtomicU32>()) {
                    (*at.cast::<AtomicU32>()).store(value as u32, Ordering::Release);
                } else {
                    at.cast::<u32>().write_unaligned(value as u32);
                    std::sync::atomic::fence(Ordering::Release);
                }
            } else if (at as usize).is_multiple_of(align_of::<AtomicU64>()) {
                (*at.cast::<AtomicU64>()).store(value, Ordering::Release);
            } else {
                at.cast::<u64>().write_unaligned(value);
                std::sync::atomic::fence(Ordering::Release);
            }
        }
        // A write at a fence's GPU address moves its value.
        crate::fence::written(at as usize, bytes)
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

pub(crate) const MAX_PRESENT_PLANES: usize = 8;

/// Queue the first resource of each display plane in the title's stacking order.
extern "system" fn present(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    // Read now: the parameters are the title's, maybe on its stack. The
    // frame is shown in the queue's order, after the work before it.
    let mut held: [Option<object::Held>; MAX_PRESENT_PLANES] = Default::default();
    if !plane_parameters.is_null() {
        for (i, slot) in held.iter_mut().take(planes as usize).enumerate() {
            // SAFETY: PresentX supplies 0x48 bytes per plane, with this header.
            let plane = unsafe { &*plane_parameters.add(i) };
            if plane.count > 0 && !plane.resources.is_null() {
                // SAFETY: the plane has at least one resource.
                let resource = unsafe { *plane.resources };
                if !resource.is_null() {
                    *slot = Some(object::Held::new(resource));
                }
            }
        }
    }
    queue(this)
        .timeline
        .submit(Op::Run(Box::new(move || show(held))));
    S_OK
}

/// A present, once the queue's work before it has run: counts the frame
/// and shows its display planes.
fn show(held: [Option<object::Held>; MAX_PRESENT_PLANES]) {
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "title requested a present (draws and dispatches since the last report)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::frame_drew(frame, crate::draw::drawn());
        if crate::capture::detailed(frame) {
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        let copies = held.each_ref().map(|owner| {
            let id = owner.as_ref()?.get();
            Some((id, crate::resource::host_copy(id)?))
        });
        let mut layers = copies.iter().flatten();
        if let Some((id, primary)) = layers.next() {
            // Presentation restores every host texture before the queue proceeds.
            // Aliased planes therefore read the same original tracked state.
            let primary = (&primary.texture, primary.state.load(Ordering::Relaxed));
            let mut sources = [primary; MAX_PRESENT_PLANES];
            let mut count = 1;
            for (_, copy) in layers {
                sources[count] = (&copy.texture, copy.state.load(Ordering::Relaxed));
                count += 1;
            }
            let shown = host.present(&sources[..count], frame);
            #[cfg(feature = "diagnostics")]
            blinks(frame, *id, primary.1, shown);
            #[cfg(not(feature = "diagnostics"))]
            let _ = (id, shown);
        }
    }
    #[cfg(not(windows))]
    let _ = held;
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let draws = crate::draw::take_frame();
    let Some(black) = shown else {
        last.1 = drawn;
        return;
    };
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "frame probe entered a run of black samples"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// One page-mapping operation captured from a batch at submission time.
#[derive(Clone, Copy)]
enum PageOp {
    /// Map `len` consecutive pool bytes from `source` at `at`.
    Copy { at: u64, source: u64, len: u64 },
    /// Map the single pool page at `source` repeatedly `count` times from `at`.
    Fill { at: u64, source: u64, count: u64 },
    /// Unmap `len` bytes at `at`.
    Unmap { at: u64, len: u64 },
    /// Out-of-bounds pool index, overflow, or unknown range kind.
    Invalid,
}

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. The title streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Runs through the queue's
/// timeline so a held-back queue updates mappings after its earlier work.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    // Read now: the batch and range arrays belong to the title (often on its
    // stack) and may be reused before a held-back queue runs them.
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ops = Vec::new();
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() || b.count == 0 {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let Some(next) = at.checked_add(len) else {
                ops.push(PageOp::Invalid);
                break;
            };
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = || pool.checked_add(u64::from(r.start) * PAGE);
            match r.kind {
                0 if inside(r.count) => match source() {
                    Some(source) => ops.push(PageOp::Copy { at, source, len }),
                    None => ops.push(PageOp::Invalid),
                },
                1 if inside(1) => match source() {
                    Some(source) => ops.push(PageOp::Fill {
                        at,
                        source,
                        count: u64::from(r.count),
                    }),
                    None => ops.push(PageOp::Invalid),
                },
                2 => ops.push(PageOp::Unmap { at, len }),
                3 => {}
                _ => ops.push(PageOp::Invalid),
            }
            at = next;
        }
    }
    queue(this).timeline.submit(Op::Run(Box::new(move || {
        apply_page_mappings(map, pool, &ops)
    })));
    S_OK
}

fn apply_page_mappings(map: crate::resource::PageMapper, pool: u64, ops: &[PageOp]) {
    let mut ok = true;
    for &op in ops {
        ok &= match op {
            PageOp::Copy { at, source, len } => map(at, source, len),
            PageOp::Fill { at, source, count } => {
                (0..count).all(|k| map(at + k * PAGE, source, PAGE))
            }
            PageOp::Unmap { at, len } => map(at, 0, len),
            PageOp::Invalid => false,
        };
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed")
        });
    }
}

/// Returns `out`, as the x64 COM ABI requires for struct returns in `RAX`.
extern "system" fn desc(this: This, out: *mut QueueDesc) -> *mut QueueDesc {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
    out
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from the title's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
pub(crate) fn test_queue() -> (This, usize) {
    let desc = QueueDesc {
        kind: 0,
        priority: 0,
        flags: 0,
        nodes: 0,
    };
    let mut q = std::ptr::null();
    assert_eq!(
        create(std::ptr::null(), &desc, &QUEUE_IIDS[0], &mut q),
        S_OK
    );
    // SAFETY: one of our queues; its first field is the table.
    let table = unsafe { *q.cast::<*const *const ()>() };
    let signal_slot = if table == NEWER_FUNCTIONS.0.as_ptr() {
        0x80
    } else {
        0x70
    };
    (q, signal_slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    fn desc_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0xe0
        } else {
            0xd0
        }
    }

    #[test]
    fn queue_get_desc_returns_its_output_pointer() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 2,
            priority: 100,
            flags: 1,
            nodes: 1,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        let mut out = std::mem::MaybeUninit::<QueueDesc>::uninit();
        type GetDesc = extern "system" fn(This, *mut QueueDesc) -> *mut QueueDesc;
        let get_desc = slot::<GetDesc>(q, desc_slot(q));
        assert_eq!(get_desc(q, out.as_mut_ptr()), out.as_mut_ptr());
        // SAFETY: written above.
        let read = unsafe { out.assume_init() };
        assert_eq!(
            (read.kind, read.priority, read.flags, read.nodes),
            (desc.kind, desc.priority, desc.flags, desc.nodes)
        );
        assert!(get_desc(q, std::ptr::null_mut()).is_null());
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// Two queues: the first waits for a value the second signals later.
    /// The first queue's signal after its wait must not complete before.
    #[test]
    fn work_after_a_wait_runs_once_the_other_queue_signals() {
        type Call = extern "system" fn(This, This, u64) -> HRESULT;
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let (mut waits, mut signals) = (null, null);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut waits), S_OK);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut signals), S_OK);
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);

        let signal = |q: This| slot::<Call>(q, signal_slot(q));
        let wait = |q: This| slot::<Call>(q, signal_slot(q) + 8);
        assert_eq!(wait(waits)(waits, ready, 1), S_OK);
        assert_eq!(signal(waits)(waits, done, 1), S_OK);
        assert_eq!(completed(done), 0, "ran before the value was reached");

        assert_eq!(signal(signals)(signals, ready, 1), S_OK);
        assert_eq!(completed(done), 1);
        // Not held any more: work runs when submitted again.
        assert_eq!(signal(waits)(waits, done, 2), S_OK);
        assert_eq!(completed(done), 2);

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        for object in [waits, signals, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    /// A present submitted after a wait must not run until the wait is
    /// reached, must read its stack plane parameters at submission time and
    /// must keep its resource alive until it runs.
    #[test]
    fn present_runs_after_a_queues_wait() {
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Present = extern "system" fn(This, u32, *const Plane, *const u8) -> HRESULT;
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let (mut waits, mut signals) = (null, null);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut waits), S_OK);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut signals), S_OK);
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);
        let resources = [
            crate::resource::test_buffer(256),
            crate::resource::test_buffer(256),
        ];

        let signal = |q: This| slot::<Sync>(q, signal_slot(q));
        let wait = |q: This| slot::<Sync>(q, signal_slot(q) + 8);
        let present = |q: This| slot::<Present>(q, signal_slot(q) + 0x80);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let add_ref = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x08)(o);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let before = FRAMES.load(Ordering::Relaxed);
        assert_eq!(wait(waits)(waits, ready, 1), S_OK);
        {
            // Caller's plane parameters may live on its stack and be gone
            // before the queue wakes.
            let resources = Box::new(resources);
            let planes = resources.each_ref().map(|resource| Plane {
                token: 1,
                count: 1,
                resources: std::ptr::from_ref(resource),
                _remaining: [0; 6],
            });
            assert_eq!(
                present(waits)(waits, 2, planes.as_ptr(), std::ptr::null()),
                S_OK
            );
        }
        assert_eq!(signal(waits)(waits, done, 1), S_OK);

        assert_eq!(
            FRAMES.load(Ordering::Relaxed),
            before,
            "presented before the wait was reached"
        );
        assert_eq!(completed(done), 0);
        // The queued present holds a reference of its own.
        for resource in resources {
            assert_eq!(add_ref(resource), 3);
            assert_eq!(release(resource), 2);
        }

        assert_eq!(signal(signals)(signals, ready, 1), S_OK);
        assert_eq!(FRAMES.load(Ordering::Relaxed), before + 1);
        assert_eq!(completed(done), 1);
        for resource in resources {
            assert_eq!(release(resource), 0, "present gave back its reference");
        }

        // Not held any more: present runs at once.
        assert_eq!(
            present(waits)(waits, 0, std::ptr::null(), std::ptr::null()),
            S_OK
        );
        assert_eq!(FRAMES.load(Ordering::Relaxed), before + 2);

        for object in [waits, signals, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    /// Page mappings submitted after a wait must not run until the wait is
    /// reached, and must capture caller-owned batch/range arrays at call time.
    #[test]
    fn page_mappings_run_after_a_queues_wait() {
        use std::sync::Mutex;

        static CALLS: Mutex<Vec<(u64, u64, u64)>> = Mutex::new(Vec::new());
        fn record(to: u64, from: u64, len: u64) -> bool {
            CALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((to, from, len));
            true
        }
        crate::resource::use_page_mapper(record);

        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type CopyPages = extern "system" fn(This, u32, *const PageBatch, u64, u32, u32) -> HRESULT;
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let (mut waits, mut signals) = (null, null);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut waits), S_OK);
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut signals), S_OK);
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);

        let signal = |q: This| slot::<Sync>(q, signal_slot(q));
        let wait = |q: This| slot::<Sync>(q, signal_slot(q) + 8);
        let copy_pages = |q: This| slot::<CopyPages>(q, signal_slot(q) + 0x78);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let pool = 0x1000_0000_u64;
        let dst = 0x2000_0000_u64;
        CALLS.lock().unwrap().clear();
        assert_eq!(wait(waits)(waits, ready, 1), S_OK);
        {
            // Heap-allocated so they are freed before the queue wakes.
            let ranges = Box::new([
                PageRange {
                    kind: 0,
                    start: 1,
                    count: 2,
                },
                PageRange {
                    kind: 1,
                    start: 3,
                    count: 2,
                },
                PageRange {
                    kind: 2,
                    start: 0,
                    count: 1,
                },
                PageRange {
                    kind: 3,
                    start: 0,
                    count: 1,
                },
            ]);
            let batch = Box::new(PageBatch {
                destination: dst,
                count: ranges.len() as u32,
                ranges: ranges.as_ptr(),
            });
            assert_eq!(copy_pages(waits)(waits, 1, &*batch, pool, 4, 0), S_OK);
        }
        assert_eq!(signal(waits)(waits, done, 1), S_OK);

        assert!(
            CALLS.lock().unwrap().is_empty(),
            "page mappings ran before the wait was reached"
        );
        assert_eq!(completed(done), 0);

        assert_eq!(signal(signals)(signals, ready, 1), S_OK);
        assert_eq!(completed(done), 1);
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec![
                (dst, pool + PAGE, 2 * PAGE),
                (dst + 2 * PAGE, pool + 3 * PAGE, PAGE),
                (dst + 3 * PAGE, pool + 3 * PAGE, PAGE),
                (dst + 4 * PAGE, 0, PAGE),
            ]
        );

        // Not held any more: runs at once.
        CALLS.lock().unwrap().clear();
        let range = PageRange {
            kind: 2,
            start: 0,
            count: 1,
        };
        let batch = PageBatch {
            destination: dst,
            count: 1,
            ranges: &range,
        };
        assert_eq!(copy_pages(waits)(waits, 1, &batch, pool, 4, 0), S_OK);
        assert_eq!(*CALLS.lock().unwrap(), vec![(dst, 0, PAGE)]);

        for object in [waits, signals, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    /// A queue held back behind a fence whose memory is written directly
    /// (bypassing `Signal` and bottom-of-pipe writes) wakes without needing
    /// another submission on that queue.
    #[test]
    fn direct_memory_write_wakes_held_queue() {
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut waits = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut waits), S_OK);
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);

        let signal = |q: This| slot::<Sync>(q, signal_slot(q));
        let wait = |q: This| slot::<Sync>(q, signal_slot(q) + 8);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let address = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x60)(f) as usize;
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        assert_eq!(wait(waits)(waits, ready, 4), S_OK);
        assert_eq!(signal(waits)(waits, done, 1), S_OK);
        assert_eq!(completed(done), 0);

        // SAFETY: `address(ready)` is the fence's `AtomicU64` value.
        unsafe { (*(address(ready) as *const AtomicU64)).store(4, Ordering::Release) };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while completed(done) < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            completed(done),
            1,
            "held queue did not wake on direct write"
        );

        for object in [waits, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    /// The title's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002)
            .write(at.as_mut_ptr().cast(), 4)
            .run();
        assert_eq!(at[0], 2);
        Written::GpuTimestamp
            .write(at[1..].as_mut_ptr().cast(), 8)
            .run();
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp
            .write(at[1..].as_mut_ptr().cast(), 8)
            .run();
        assert!(at[1] > first);
    }

    #[cfg(windows)]
    #[test]
    fn execute_batches_contiguous_lists_across_mid_list_waits() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

        if crate::host::host().is_none() {
            return;
        }
        type Execute = extern "system" fn(This, u32, *const This);
        type Write32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait32 = extern "system" fn(This, u64, u32, u32, u32);
        type Wait64 = extern "system" fn(This, u64, u32, u64, u32);

        let null = std::ptr::null();
        let (queue, _) = test_queue();
        let execute = slot::<Execute>(queue, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let mut allocator = null;
        assert_eq!(
            crate::list::create_allocator(
                null,
                0,
                &crate::resource::PAGEABLE_IIDS[0],
                &mut allocator
            ),
            S_OK
        );
        let make_list = || {
            let mut l = null;
            let iid = guid(
                0x5b160d0f,
                0xac1b,
                0x4185,
                [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
            );
            assert_eq!(
                crate::list::create_list(null, 0, 0, allocator, null, &iid, &mut l),
                S_OK
            );
            l
        };

        let list1 = make_list();
        let list2 = make_list();
        let list_slot = crate::list::test_slot;

        let gate1 = AtomicU32::new(0);
        let gate2 = AtomicU64::new(0);
        let out = [
            AtomicU32::new(0),
            AtomicU32::new(0),
            AtomicU32::new(0),
            AtomicU32::new(0),
        ];

        slot::<Write32>(list1, list_slot(list1, 69))(list1, (&raw const out[0]) as u64, 1, 0, 0);
        slot::<Wait32>(list1, list_slot(list1, 71))(
            list1,
            (&raw const gate1) as u64,
            3,
            1,
            u32::MAX,
        );
        slot::<Write32>(list1, list_slot(list1, 69))(list1, (&raw const out[1]) as u64, 2, 0, 0);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list1, 0x48)(list1),
            S_OK
        );

        slot::<Write32>(list2, list_slot(list2, 69))(list2, (&raw const out[2]) as u64, 3, 0, 0);
        slot::<Wait64>(list2, list_slot(list2, 72))(
            list2,
            (&raw const gate2) as u64,
            3,
            0x1_0000_0001,
            0,
        );
        slot::<Write32>(list2, list_slot(list2, 69))(list2, (&raw const out[3]) as u64, 4, 0, 0);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list2, 0x48)(list2),
            S_OK
        );

        let lists = [list1, list2];
        execute(queue, 2, lists.as_ptr());

        assert_eq!(out[0].load(Ordering::Acquire), 1);
        assert_eq!(out[1].load(Ordering::Acquire), 0);
        assert_eq!(out[2].load(Ordering::Acquire), 0);
        assert_eq!(out[3].load(Ordering::Acquire), 0);

        gate1.store(1, Ordering::Release);
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while out[2].load(Ordering::Acquire) == 0 && std::time::Instant::now() < limit {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(out[1].load(Ordering::Acquire), 2);
        assert_eq!(out[2].load(Ordering::Acquire), 3);
        assert_eq!(out[3].load(Ordering::Acquire), 0);

        gate2.store(0x1_0000_0001, Ordering::Release);
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while out[3].load(Ordering::Acquire) == 0 && std::time::Instant::now() < limit {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(out[3].load(Ordering::Acquire), 4);

        for object in [list1, list2, allocator, queue] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    fn test_copy_list() -> (This, This, This, This) {
        type CopyBuf = extern "system" fn(This, This, u64, This, u64, u64);

        let null = std::ptr::null();
        let mut allocator = null;
        let alloc_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &alloc_iid, &mut allocator),
            S_OK
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        let mut list = null;
        assert_eq!(
            crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
            S_OK
        );
        let src = crate::resource::test_buffer(64);
        let dst = crate::resource::test_buffer(64);
        slot::<CopyBuf>(list, 0x78)(list, dst, 0, src, 0, 64);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
            S_OK
        );
        (list, allocator, src, dst)
    }

    #[cfg(windows)]
    #[test]
    fn queued_list_behind_fence_wait_pins_retired_objects_until_completion() {
        let _retire = crate::host::isolated_retire_scope();
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let (q, sig_slot) = test_queue();
        let wait = slot::<Sync>(q, sig_slot + 8);
        let signal = slot::<Sync>(q, sig_slot);
        let execute = slot::<Execute>(q, 0x50);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut ready, mut done) = (null, null);
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );
        assert_eq!(crate::fence::create(null, 0, 0, fence_iid, &mut done), S_OK);

        let (list, allocator, src, dst) = test_copy_list();

        assert_eq!(wait(q, ready, 1), S_OK);
        execute(q, 1, &list);
        assert_eq!(signal(q, done, 1), S_OK);
        // Reset the title list so only the queued submission holds the pending host list.
        assert_eq!(
            slot::<extern "system" fn(This, This, This) -> HRESULT>(list, 0x50)(
                list, allocator, null
            ),
            S_OK
        );

        let (counter, drops) = crate::host::DropCounter::new();
        crate::host::retire(counter);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "queued submission behind fence wait must pin retired objects"
        );

        assert_eq!(crate::fence::signal(ready, 1), S_OK);
        assert_eq!(completed(done), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "retired object must drop once queued work finishes"
        );

        for object in [list, allocator, src, dst, q, ready, done] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn double_submitted_list_pins_retired_objects_until_both_executions_complete() {
        let _retire = crate::host::isolated_retire_scope();
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let (q1, sig_slot1) = test_queue();
        let (q2, sig_slot2) = test_queue();
        let wait1 = slot::<Sync>(q1, sig_slot1 + 8);
        let signal1 = slot::<Sync>(q1, sig_slot1);
        let exec1 = slot::<Execute>(q1, 0x50);
        let wait2 = slot::<Sync>(q2, sig_slot2 + 8);
        let signal2 = slot::<Sync>(q2, sig_slot2);
        let exec2 = slot::<Execute>(q2, 0x50);
        let completed = |f: This| slot::<extern "system" fn(This) -> u64>(f, 0x40)(f);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let (mut f1, mut f2, mut d1, mut d2) = (null, null, null, null);
        for f in [&mut f1, &mut f2, &mut d1, &mut d2] {
            assert_eq!(crate::fence::create(null, 0, 0, fence_iid, f), S_OK);
        }

        let (list, allocator, src, dst) = test_copy_list();

        assert_eq!(wait1(q1, f1, 1), S_OK);
        exec1(q1, 1, &list);
        assert_eq!(signal1(q1, d1, 1), S_OK);

        assert_eq!(wait2(q2, f2, 1), S_OK);
        exec2(q2, 1, &list);
        assert_eq!(signal2(q2, d2, 1), S_OK);

        let (counter, drops) = crate::host::DropCounter::new();
        crate::host::retire(counter);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        assert_eq!(crate::fence::signal(f1, 1), S_OK);
        assert_eq!(completed(d1), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "retired object must stay alive until the second submission also finishes"
        );

        assert_eq!(crate::fence::signal(f2, 1), S_OK);
        assert_eq!(completed(d2), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        for object in [list, allocator, src, dst, q1, q2, f1, f2, d1, d2] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_queued_list_drops_retired_objects_when_queue_is_released() {
        let _retire = crate::host::isolated_retire_scope();
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let (q, sig_slot) = test_queue();
        let wait = slot::<Sync>(q, sig_slot + 8);
        let execute = slot::<Execute>(q, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut ready = null;
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );

        let (list, allocator, src, dst) = test_copy_list();

        assert_eq!(wait(q, ready, 1), S_OK);
        execute(q, 1, &list);
        assert_eq!(
            slot::<extern "system" fn(This, This, This) -> HRESULT>(list, 0x50)(
                list, allocator, null
            ),
            S_OK
        );

        let (counter, drops) = crate::host::DropCounter::new();
        crate::host::retire(counter);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Release the queue without signalling `ready`; dropping the held
        // closure must release the InFlight guard and drop the retired object.
        assert_eq!(release(q), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        for object in [list, allocator, src, dst, ready] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_queued_list_keeps_recording_epoch_while_title_still_owns_list() {
        let _retire = crate::host::isolated_retire_scope();
        type Sync = extern "system" fn(This, This, u64) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let (q, sig_slot) = test_queue();
        let wait = slot::<Sync>(q, sig_slot + 8);
        let execute = slot::<Execute>(q, 0x50);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);

        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut ready = null;
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut ready),
            S_OK
        );

        let (list, allocator, src, dst) = test_copy_list();

        assert_eq!(wait(q, ready, 1), S_OK);
        execute(q, 1, &list);

        let (counter, drops) = crate::host::DropCounter::new();
        crate::host::retire(counter);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Drop the queue without signalling `ready` while the title still owns
        // the closed list: cancelling the unexecuted submission must not clear
        // the list's recording epoch until the title resets or releases it.
        assert_eq!(release(q), 0);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "cancelling an unrun submission must not clear the epoch while the list is still live"
        );

        assert_eq!(
            slot::<extern "system" fn(This, This, This) -> HRESULT>(list, 0x50)(
                list, allocator, null
            ),
            S_OK
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        for object in [list, allocator, src, dst, ready] {
            assert_eq!(release(object), 0);
        }
    }
}
