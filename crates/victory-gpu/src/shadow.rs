//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATES, ID3D12Resource,
};

use crate::host::{HostList, host};
use crate::watch::PAGE;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// Bytes seen when each shader-write range was first declared in this submission.
    before: Mutex<Before>,
}

#[derive(Default)]
struct Before {
    segments: Vec<(usize, Vec<u8>)>,
    unchanged: UnchangedPages,
}

/// Pages already compared in full against this submission's snapshot.
#[derive(Default)]
struct UnchangedPages {
    start: usize,
    end: usize,
    pages: Vec<bool>,
}

impl UnchangedPages {
    fn begin(&mut self, start: usize, end: usize) {
        if self.pages.is_empty() {
            self.start = start;
            self.end = end;
            self.pages.resize(end.div_ceil(PAGE) - start / PAGE, false);
        }
    }

    fn differs(&mut self, address: usize, current: &[u8], before: &[u8]) -> bool {
        let page = address / PAGE;
        let unchanged = &mut self.pages[page - self.start / PAGE];
        if *unchanged {
            return false;
        }
        if current != before {
            return true;
        }
        // A later range can touch bytes a partial comparison missed. At
        // the edges, only bytes covered by the snapshot can be compared.
        *unchanged = address == (page * PAGE).max(self.start)
            && address + current.len() == ((page + 1) * PAGE).min(self.end);
        false
    }

    fn clear(&mut self) {
        self.pages.clear();
    }
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool (at most 8 kept, the largest).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    if pool.len() > 8 {
        pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
        pool.truncate(8);
    }
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start
    }

    /// `len` bytes at a title address from the host copy, for the log: the
    /// bytes the copy holds, as of its last refresh (None outside the buffer).
    pub(crate) fn bytes(&self, address: u64, len: u64) -> Option<&[u8]> {
        if address < self.start || len == 0 || len > self.left(address) {
            return None;
        }
        let at = usize::try_from(address - self.start).ok()?;
        let len = usize::try_from(len).ok()?;
        // SAFETY: inside the mapping, which is as long as the buffer.
        Some(unsafe { std::slice::from_raw_parts(self.data.add(at), len) })
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset. Cut to the copy.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        self.refresh_as(list, address, len, D3D12_RESOURCE_STATE_GENERIC_READ);
    }

    /// Refresh a range and prepare its host buffer for the specified GPU use.
    pub(crate) fn refresh_as(
        self: &Arc<Self>,
        list: &HostList,
        address: u64,
        len: u64,
        access: D3D12_RESOURCE_STATES,
    ) {
        list.use_shadow(&self.buffer, access);
        let from = address.max(self.start);
        let len = range_inside(
            from,
            len.saturating_sub(from.saturating_sub(address)),
            self.start,
            self.len,
        );
        if len == 0 {
            return;
        }
        list.defer_refresh(self, from as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![0; pages as usize]),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            before: Mutex::default(),
        }
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Save newly declared bytes after their refresh. Keep
    /// earlier snapshots intact when another range overlaps. The bytes between
    /// disjoint ranges can still be refreshed before a later declaration.
    pub(crate) fn writing(&self, address: usize, len: usize) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((address, address + len));
        let (from, to) = self.inside(address, len);
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let segments = &mut before.segments;
        let mut i = segments.partition_point(|(start, bytes)| start + bytes.len() <= from);
        let mut at = from;
        while at < to {
            let next = segments.get(i).map_or(to, |(start, _)| to.min(*start));
            if at < next {
                let mut bytes = pooled(next - at);
                // SAFETY: inside the mapped copy; this gap is newly declared.
                bytes.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(self.data.add(at), next - at)
                });
                segments.insert(i, (at, bytes));
                at = next;
            } else {
                at = segments[i].0 + segments[i].1.len();
            }
            i += 1;
        }
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let Before {
            segments,
            unchanged,
        } = &mut *before;
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let (Some((first, _)), Some((last, bytes))) = (segments.first(), segments.last())
            && from < to
        {
            unchanged.begin(start + first, start + last + bytes.len());
            let mut title = None;
            let mut synced = Vec::new();
            let first = segments.partition_point(|(at, bytes)| at + bytes.len() <= from);
            for (taken, bytes) in segments[first..].iter().take_while(|(at, _)| *at < to) {
                let mut at = from.max(*taken);
                let end = to.min(taken + bytes.len());
                while at < end {
                    let n = (PAGE - (start + at) % PAGE).min(end - at);
                    // SAFETY: inside the mapping; only the submitting thread
                    // writes it.
                    let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                    let old = &bytes[at - taken..at - taken + n];
                    if unchanged.differs(start + at, host, old)
                        && let title = &mut title.get_or_insert_with(|| vec![0u8; PAGE])[..n]
                        && crate::host::read_into((start + at) as *const u8, title)
                    {
                        // Title memory with the GPU's bytes put in; the copy
                        // takes the title's bytes in turn.
                        let mut changed = false;
                        for ((t, h), o) in title.iter_mut().zip(host.iter_mut()).zip(old) {
                            if *h != *o && *t != *h {
                                *t = *h;
                                changed = true;
                            }
                            *h = *t;
                        }
                        if changed {
                            crate::watch::writing(start + at, n);
                            crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                            written += n;
                        }
                        // A neighboring segment on this page can still have a newer
                        // title write. Only a full-page merge made the whole copy current.
                        if n == PAGE {
                            synced.push((start + at) / PAGE);
                        }
                    }
                    at += n;
                }
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if first < last
                && !synced.is_empty()
                && self.watched.load(Ordering::Relaxed) != NOT_WATCHED
                && let Some((now, _)) = crate::watch::writes(first * PAGE, last * PAGE, true)
            {
                let base = start / PAGE;
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                for page in synced.into_iter().filter(|p| (first..last).contains(p)) {
                    copied[page - base] = now;
                }
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|&r| r != (address, address + len));
        if pending.is_empty() {
            unchanged.clear();
            for (_, bytes) in segments.drain(..) {
                give_back(bytes);
            }
        }
        written
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
            // Around ranges shaders are writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame).
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
    fn update_free(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            // Only mark the shadow permanently unwatched if it has never been
            // observed as watched. A sub-range that lands entirely in an
            // unmapped hole inside an already-watched view pool must not
            // disable write-watching for the rest of the pool.
            let _ =
                self.watched
                    .compare_exchange(0, NOT_WATCHED, Ordering::Relaxed, Ordering::Relaxed);
        }
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
    }
}

/// A refresh that copies none of its range is skipped: the writes it was
/// asked about were already in the copy, the range is outside the buffer,
/// or shaders were still writing all of it (that part is left to a later
/// refresh). Counted for the frame-rate line as one skipped refresh of
/// `len` bytes, so `0 skipped` there means no refresh was answered
/// without a copy, not that nothing was checked. A range copied in part is
/// not skipped; of what it did not copy, `unchanged` counts only the parts
/// this refresh checked, not the sub-ranges shaders still write.
#[cfg(any(feature = "diagnostics", test))]
pub(crate) fn skipped(len: usize, copied: usize) -> Option<u64> {
    (len > 0 && copied == 0).then_some(len as u64)
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive,
/// and any active epoch keeps it until that work completes.
pub(crate) fn unregister(resource: usize, start: u64) {
    let shadow = {
        let mut map = buffers();
        map.remove(&(start, resource)).and_then(|e| e.shadow)
    };
    if let Some(shadow) = shadow {
        crate::host::retire(shadow);
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Arc::new(Shadow::new(buffer, data, gpu, key.0, len)));
    }
    entry.shadow.clone()
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, if one has already been made: the same lookup as [`find`] without
/// making a copy, for paths that must not allocate one.
pub(crate) fn peek(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let map = buffers();
    let shadow = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .and_then(|(_, e)| e.shadow.clone());
    if shadow.is_some() {
        return shadow;
    }
    drop(map);
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    LOOSE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(start, len))
        .cloned()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// Copies of just one view's bytes, by (start, length): for a view whose
/// start is not on an element of its host copy (`aligned`). The heap slot
/// holds the copy; this map only finds it again, so it holds weak references
/// and the copy goes when the slot is remade.
static ALIGNED: Mutex<BTreeMap<(u64, u64), std::sync::Weak<Shadow>>> = Mutex::new(BTreeMap::new());

/// A copy of `[address, address + len)` that starts at `address` itself, so a
/// structured view of it can have its elements where the title put them.
///
/// A title aligns a resource; the host copy of a buffer starts where the
/// buffer does, so a view of memory the title placed inside another object
/// (`View::Placed`) starts at an offset that need not be a multiple of the
/// view's stride. This copy starts at the view, and the slot that made it
/// keeps it alive.
///
/// Its own refresh copies the 64 KiB pages the title wrote since they were
/// last copied, straight from title memory: a submission's copy-back runs
/// before the next submission prepares (`Host::execute` waits for the GPU
/// before copying back), so by then title memory holds what the GPU wrote in
/// any copy, and what shaders wrote in this one has gone back there. Copying
/// from the buffer's copy instead would copy the view's whole range again on
/// every submission.
pub(crate) fn aligned(address: u64, len: u64) -> Option<Arc<Shadow>> {
    if len == 0 {
        return None;
    }
    let key = (address, len);
    if let Some(copy) = aligned_existing(address, len) {
        return Some(copy);
    }
    // Views hold hundreds of megabytes in the world; made as a buffer's copy
    // is, so that shaders may write it (unordered access).
    let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(64))?;
    crate::host::name(&buffer, || format!("aligned view {address:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let copy = Arc::new(Shadow::new(buffer, data, gpu, address, len));
    // Populate now so a ResourceHeap-only first use (which never walks the
    // table refresh path) still sees title memory rather than zeros (#553).
    // SAFETY: `data` is the mapping of `buffer`; `address` is title memory
    // of `len` bytes the view was created for.
    crate::host::copy_mapped(data, address as *const u8, len as usize, false);
    {
        let mut copies = ALIGNED.lock().unwrap_or_else(|e| e.into_inner());
        // Drop expired weaks so distinct (address, len) churn cannot grow
        // the map without bound (docs/code-standards.md host-cache bound).
        copies.retain(|_, weak| weak.strong_count() > 0);
        // Another recorder may have published the same range while we allocated.
        if let Some(existing) = copies.get(&key).and_then(std::sync::Weak::upgrade) {
            return Some(existing);
        }
        copies.insert(key, Arc::downgrade(&copy));
    }
    tracing::debug!(
        address = format_args!("{address:#x}"),
        len,
        "aligned view copy made"
    );
    Some(copy)
}

/// The copy `aligned` made for this range, if one is still alive.
pub(crate) fn aligned_existing(address: u64, len: u64) -> Option<Arc<Shadow>> {
    ALIGNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(address, len))
        .and_then(std::sync::Weak::upgrade)
}

/// How many bytes of `[address, address + len)` lie inside the copy that
/// starts at `start` of `size` bytes: what a refresh of that copy can bring up
/// to date. `0` when the range is not inside it at all.
fn range_inside(address: u64, len: u64, start: u64, size: u64) -> u64 {
    let end = start.saturating_add(size);
    let from = address.max(start);
    let to = address.saturating_add(len).min(end);
    to.saturating_sub(from)
}

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(buffer, data, gpu, start, len));
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    all.extend(
        ALIGNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|w| w.upgrade())
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;

    #[test]
    fn concurrent_aligned_views_share_the_copy_refreshed_at_submission() {
        if crate::host::host().is_none() {
            return;
        }

        const THREADS: usize = 8;
        let values = [[19u32; 14]; 8];
        for value in &values {
            let address = value.as_ptr() as u64;
            let len = size_of_val(value) as u64;
            let gate = std::sync::Barrier::new(THREADS);
            let copies = std::thread::scope(|scope| {
                let jobs: Vec<_> = (0..THREADS)
                    .map(|_| {
                        let gate = &gate;
                        scope.spawn(move || {
                            gate.wait();
                            aligned(address, len).expect("aligned host buffer")
                        })
                    })
                    .collect();
                jobs.into_iter()
                    .map(|job| job.join().unwrap())
                    .collect::<Vec<_>>()
            });
            let refreshed = aligned_existing(address, len).expect("cached copy");
            refreshed.update(address as usize, len as usize);
            for copy in copies {
                assert!(
                    Arc::ptr_eq(&copy, &refreshed),
                    "slot retained a different copy"
                );
                // SAFETY: the mapped buffer has 56 bytes and refresh has completed.
                let first = unsafe { copy.data.cast::<u32>().read_unaligned() };
                assert_eq!(first, value[0]);
            }
        }
    }

    /// A refresh of the aligned copy brings up to date the bytes of the view
    /// that lie inside it, cut at both ends, and nothing when the range is
    /// outside it.
    #[test]
    fn a_refresh_covers_the_views_bytes_inside_its_copy() {
        // The hit-detection view: 3136 bytes of 56-byte elements, 48 bytes
        // into its buffer, in a copy that starts at the view.
        assert_eq!(range_inside(0x3000_0030, 3136, 0x3000_0030, 3136), 3136);
        // A view reaching past its copy is cut to it.
        assert_eq!(range_inside(0x4000_0100, 900, 0x4000_0100, 0x100), 0x100);
        // A view starting before its copy loses the part before it.
        assert_eq!(range_inside(0x0ff8, 0x40, 0x1000, 0x1000), 0x38);
        // Outside the copy at either end: nothing to refresh.
        assert_eq!(range_inside(0x1000, 0x10, 0x2000, 0x100), 0);
        assert_eq!(range_inside(0x20f0, 0x20, 0x1000, 0x100), 0);
        // A size that would overflow does not.
        assert_eq!(range_inside(u64::MAX - 8, 16, 0, 16), 0);
    }

    /// A refresh that copied nothing of its range is what the frame-rate
    /// line's `skipped` counts; one that copied any part of it is not, and
    /// an empty range is neither.
    #[test]
    fn a_refresh_that_copies_nothing_is_counted_as_skipped() {
        assert_eq!(skipped(0, 0), None);
        assert_eq!(skipped(4 << 20, 0), Some(4 << 20));
        assert_eq!(skipped(4 << 20, 1), None);
        assert_eq!(skipped(4 << 20, 4 << 20), None);
        // Nothing in the buffer: the range was outside it.
        assert_eq!(skipped(4096, 0), Some(4096));
    }

    #[test]
    fn partial_match_leaves_unchecked_bytes_visible() {
        let mut unchanged = UnchangedPages::default();
        let before = vec![0; PAGE];
        let mut host = before.clone();
        host[PAGE - 1] = 1;
        unchanged.begin(PAGE, 2 * PAGE);

        assert!(!unchanged.differs(PAGE, &host[..4], &before[..4]));
        assert!(!unchanged.pages[0]);
        assert!(unchanged.differs(PAGE, &host, &before));
        assert!(!unchanged.pages[0]);
    }

    #[test]
    fn snapshot_edges_are_cacheable_and_survive_later_ranges() {
        let mut unchanged = UnchangedPages::default();
        unchanged.begin(PAGE - 3, PAGE + 4);
        assert!(!unchanged.differs(PAGE - 3, &[1; 3], &[1; 3]));
        assert!(!unchanged.differs(PAGE, &[2; 4], &[2; 4]));
        assert_eq!(unchanged.pages, [true, true]);

        unchanged.begin(PAGE - 3, PAGE + 4);
        assert!(!unchanged.differs(PAGE - 2, &[1; 2], &[1; 2]));
        assert!(!unchanged.differs(PAGE + 1, &[2; 2], &[2; 2]));
        assert_eq!(unchanged.pages, [true, true]);
    }

    #[test]
    fn next_submission_checks_the_same_page_again() {
        let mut unchanged = UnchangedPages::default();
        unchanged.begin(0, 4);
        assert!(!unchanged.differs(0, &[0; 4], &[0; 4]));
        unchanged.clear();
        unchanged.begin(0, 4);
        assert!(unchanged.differs(0, &[0, 0, 0, 1], &[0; 4]));
    }

    // Both versions walk exactly the same declared ranges, in their
    // original order. Only the page comparison differs.
    fn scan(
        host: &[u8],
        before: &[u8],
        ranges: &[Range<usize>],
        mut differs: impl FnMut(usize, &[u8], &[u8]) -> bool,
    ) -> usize {
        let mut changed = 0;
        for range in ranges {
            let mut at = range.start;
            while at < range.end {
                let n = (PAGE - at % PAGE).min(range.end - at);
                changed += usize::from(differs(at, &host[at..at + n], &before[at..at + n]));
                at += n;
            }
        }
        changed
    }

    #[test]
    fn overlapping_ranges_match_uncached_comparisons() {
        let before = vec![0; 4 * PAGE];
        let mut host = before.clone();
        for at in [2, PAGE - 1, PAGE + 5, 3 * PAGE + 7] {
            host[at] = 1;
        }
        let ranges = [
            0..4,
            2..2 * PAGE + 3,
            PAGE - 2..4 * PAGE,
            2 * PAGE..3 * PAGE,
        ];
        let uncached = scan(&host, &before, &ranges, |_, host, before| host != before);
        let mut unchanged = UnchangedPages::default();
        unchanged.begin(0, before.len());
        let cached = scan(&host, &before, &ranges, |at, host, before| {
            unchanged.differs(at, host, before)
        });
        assert_eq!(uncached, 6);
        assert_eq!(cached, uncached);
        assert_eq!(unchanged.pages, [false, false, true, false]);
    }

    // cargo test --release -p victory-gpu benchmark_overlapping_copy_back -- --ignored --nocapture
    #[test]
    #[ignore = "synthetic CPU release benchmark"]
    fn benchmark_overlapping_copy_back() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        const SIZE: usize = 128 << 20;
        let before = vec![0xA5; SIZE];
        let mut host = before.clone();
        for page in (0..SIZE / PAGE).step_by(64) {
            host[page * PAGE + PAGE / 2] ^= 0xFF;
        }
        let ranges: Vec<_> = (0..24).map(|i| i * PAGE + i..SIZE - i * 13).collect();
        let mut baseline = [Duration::ZERO; 5];
        let mut cached = [Duration::ZERO; 5];
        let mut unchanged = UnchangedPages::default();
        for (baseline, cached) in baseline.iter_mut().zip(&mut cached) {
            let started = Instant::now();
            let expected = scan(
                black_box(&host),
                black_box(&before),
                black_box(&ranges),
                |_, host, before| host != before,
            );
            *baseline = started.elapsed();

            unchanged.clear();
            let started = Instant::now();
            unchanged.begin(0, before.len());
            let actual = scan(
                black_box(&host),
                black_box(&before),
                black_box(&ranges),
                |at, host, before| unchanged.differs(at, host, before),
            );
            *cached = started.elapsed();
            assert_eq!(actual, expected);
        }
        baseline.sort_unstable();
        cached.sort_unstable();
        println!(
            "median of five runs, 24 overlapping ranges across 128 MiB: {:?} -> {:?} ({:.1}x)",
            baseline[2],
            cached[2],
            baseline[2].as_secs_f64() / cached[2].as_secs_f64(),
        );
    }

    #[test]
    fn later_gpu_write_range_keeps_the_titles_newer_bytes() {
        let _retire = crate::host::isolated_retire_scope();
        if host().is_none() {
            eprintln!("skipping shadow copy-back test: no D3D12 device");
            assert!(
                std::env::var_os("VICTORY_D3D12_WARP").is_none(),
                "WARP unavailable"
            );
            return;
        }
        let mut title = vec![0u8; 2 * PAGE];
        let start = title.as_mut_ptr() as usize;
        let edge = (start + 2).next_multiple_of(PAGE);
        let offset = edge - 2 - start;
        let resource = 0xdead_beef_0011;
        register(resource, start as u64, title.len() as u64);
        let copy = find(start as u64, title.len() as u64).expect("shadow");
        copy.update(start, title.len());
        copy.writing(edge - 2, 1);
        copy.writing(edge + 1, 1);

        // The two middle bytes cross a page boundary. Their first declaration
        // follows a refresh of values written after the outer declarations.
        title[offset + 1..offset + 3].copy_from_slice(&[0x44, 0x45]);
        copy.update(edge - 1, 2);
        assert_eq!(copy.bytes((edge - 1) as u64, 2), Some(&[0x44, 0x45][..]));
        copy.writing(edge - 2, 4);
        title[offset + 1..offset + 3].copy_from_slice(&[0x55, 0x56]);
        // SAFETY: the test owns the mapped buffer and simulates its GPU write.
        unsafe { copy.data.add(offset + 3).write(0xaa) };
        copy.copy_back(edge - 2, 1);
        copy.copy_back(edge + 1, 1);
        copy.copy_back(edge - 2, 4);
        unregister(resource, start as u64);
        assert_eq!(&title[offset..offset + 4], &[0, 0x55, 0x56, 0xaa]);
    }

    #[test]
    #[ignore = "times overlapping GPU-write declarations on a D3D12 host"]
    fn time_growing_gpu_write_snapshots() {
        let _retire = crate::host::isolated_retire_scope();
        let mut title = vec![0u8; 8 << 20];
        let start = title.as_mut_ptr() as usize;
        let resource = 0xdead_beef_0012;
        register(resource, start as u64, title.len() as u64);
        let copy = find(start as u64, title.len() as u64).expect("D3D12 shadow");
        copy.update(start, title.len());
        let step = title.len() / 32;
        for growing in [true, false] {
            let lengths: Vec<_> = (1..=32)
                .map(|i| if growing { i * step } else { (33 - i) * step })
                .collect();
            for batch in 0..10 {
                let begin = std::time::Instant::now();
                for &len in &lengths {
                    copy.writing(start, len);
                }
                let time = begin.elapsed();
                for &len in &lengths {
                    copy.copy_back(start, len);
                }
                if batch >= 2 {
                    println!("growing={growing} snapshot_us={}", time.as_micros());
                }
            }
        }
        unregister(resource, start as u64);
    }

    #[cfg(windows)]
    #[test]
    fn unregister_drops_shadow_immediately_when_idle() {
        let _retire = crate::host::isolated_retire_scope();
        if host().is_none() {
            return;
        }
        let data = vec![0u8; 4096];
        let start = data.as_ptr() as u64;
        let resource = 0xdead_beef_0001_usize;
        register(resource, start, data.len() as u64);
        let weak = {
            let shadow = find(start, 64).expect("shadow");
            Arc::downgrade(&shadow)
        };
        assert_eq!(weak.strong_count(), 1);
        unregister(resource, start);
        assert_eq!(weak.strong_count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn bytes_reads_the_copy_inside_the_buffer_only() {
        let _retire = crate::host::isolated_retire_scope();
        if host().is_none() {
            return;
        }
        let data = vec![0u8; 4096];
        let start = data.as_ptr() as u64;
        let resource = 0xdead_beef_0002_usize;
        register(resource, start, data.len() as u64);
        let shadow = find(start, 64).expect("shadow");
        assert_eq!(shadow.bytes(start, 16).map(<[u8]>::len), Some(16));
        assert_eq!(
            shadow
                .bytes(start + data.len() as u64 - 16, 16)
                .map(<[u8]>::len),
            Some(16),
            "a range ending in the buffer's last bytes is inside it"
        );
        assert!(shadow.bytes(start - 4, 4).is_none(), "before the buffer");
        assert!(
            shadow.bytes(start + data.len() as u64 - 8, 16).is_none(),
            "past the buffer"
        );
        assert!(shadow.bytes(start, 0).is_none(), "nothing to read");
        unregister(resource, start);
    }

    #[cfg(windows)]
    #[test]
    fn unregister_retires_shadow_until_active_epoch_finishes() {
        let _retire = crate::host::isolated_retire_scope();
        let Some(host) = host() else {
            return;
        };
        let list = host.new_list().expect("host list");
        list.mark_work();

        let data = vec![0u8; 4096];
        let start = data.as_ptr() as u64;
        let resource = 0xdead_beef_0002_usize;
        register(resource, start, data.len() as u64);
        let weak = {
            let shadow = find(start, 64).expect("shadow");
            Arc::downgrade(&shadow)
        };
        assert_eq!(weak.strong_count(), 1);

        unregister(resource, start);
        assert_eq!(
            weak.strong_count(),
            1,
            "unregistered shadow must wait for active epochs"
        );

        list.close();
        list.reset();
        assert_eq!(weak.strong_count(), 0);
    }
}
