//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_PRIVATE, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQuery,
    VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if at.is_null() {
        let error = std::io::Error::last_os_error();
        if kind & MEM_COMMIT != 0 {
            commit_refused(size, "virtual memory", error);
        } else {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
        }
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    if !at.is_null() {
        if kind & MEM_RESERVE_PLACEHOLDER != 0 {
            victory_gpu::title_placeholder_reserved(at as usize, size);
        } else {
            victory_gpu::title_memory_allocated(at as usize, size);
        }
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section {
    handle: HANDLE,
    pages: usize,
    /// A view of the whole section, kept ([`Section::window`]); 0 if the
    /// host gave none.
    window: std::sync::OnceLock<usize>,
    /// A bit per page that has been committed through any of our views.
    committed: Box<[std::sync::atomic::AtomicU64]>,
}

impl Section {
    /// Where bytes of `n` pages from page `index` on can be written,
    /// whether or not the pages are mapped anywhere, committed.
    ///
    /// Keep one address-space view per section instead of mapping and
    /// committing a separate view for every DirectStorage write.
    fn window(&self, index: usize, n: usize) -> Option<usize> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        if index.checked_add(n)? > self.pages {
            return None;
        }
        let base = *self.window.get_or_init(|| {
            // SAFETY: the whole of a section we own.
            unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, self.pages * PAGE) }.Value
                as usize
        });
        if base == 0 {
            return None;
        }
        let missing =
            |page: usize| self.committed[page / 64].load(Ordering::Acquire) >> (page % 64) & 1 == 0;
        let mut page = index;
        while page < index + n {
            if !missing(page) {
                page += 1;
                continue;
            }
            let run = (page..index + n).take_while(|&p| missing(p)).count();
            if run == 0 {
                continue; // another worker committed it meanwhile
            }
            if !commit(base + page * PAGE, run * PAGE) {
                return None;
            }
            for p in page..page + run {
                self.committed[p / 64].fetch_or(1 << (p % 64), Ordering::AcqRel);
            }
            page += run;
        }
        Some(base + index * PAGE)
    }
}

impl Section {
    /// Commits the pages from `index` on that a view at `base` shows, unless
    /// all are already committed through another view.
    fn commit_shown(&self, index: usize, base: usize, len: usize) -> bool {
        use std::sync::atomic::Ordering;
        let pages = index..(index + len / PAGE).min(self.pages);
        let bit = |p: usize| (&self.committed[p / 64], 1 << (p % 64));
        let done = || {
            pages.clone().all(|p| {
                let (word, bit) = bit(p);
                word.load(Ordering::Acquire) & bit != 0
            })
        };
        if done() {
            return true;
        }
        if !commit(base, len) {
            return false;
        }
        for p in pages {
            let (word, bit) = bit(p);
            word.fetch_or(bit, Ordering::AcqRel);
        }
        true
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        if let Some(&base) = self.window.get()
            && base != 0
        {
            // SAFETY: our view; whoever writes through it holds the section.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: base as *mut c_void,
                })
            };
        }
        victory_gpu::title_memory_section_gone(self.handle as usize);
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
}

impl Physical {
    /// What the page at `at` shows.
    fn slice_at(&self, at: usize) -> Option<Slice> {
        self.views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
    }
}

/// The title's pages by number.
struct Pages {
    next_page: usize,
    free_pages: Vec<usize>,
    pages: HashMap<usize, Slice>,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            next_page: FIRST_PAGE_NUMBER,
            free_pages: Vec::new(),
            pages: HashMap::new(),
        }
    }
}

impl Pages {
    fn allocate(&mut self, section: Arc<Section>, numbers: &mut [MaybeUninit<usize>]) -> bool {
        let fresh = numbers.len().saturating_sub(self.free_pages.len());
        if fresh > (PAGE_NUMBER_LIMIT - self.next_page) / FRAMES {
            return false;
        }
        for (index, out) in numbers.iter_mut().enumerate() {
            let number = self.free_pages.pop().unwrap_or_else(|| {
                let number = self.next_page;
                self.next_page += FRAMES;
                number
            });
            self.pages.insert(
                number,
                Slice {
                    section: section.clone(),
                    index,
                },
            );
            out.write(number);
        }
        true
    }

    fn free(&mut self, number: usize) -> Option<Slice> {
        let slice = self.pages.remove(&number)?;
        self.free_pages.push(number);
        Some(slice)
    }
}

/// Where the title sees its pages: [`Physical::views`] the other way.
/// Locked apart from them, and only for a moment ([`PAGES`]).
#[derive(Default)]
struct Shown {
    /// The addresses that show a page, by the page's section and index.
    /// Views of more than [`SHORT`] pages are in `long`.
    pages: HashMap<(usize, usize), Vec<usize>>,
    /// Address, bytes, section, first page.
    long: Vec<(usize, usize, usize, usize)>,
}

/// Pages of a view noted page by page in [`Shown::pages`].
const SHORT: usize = 64;

/// Taken after [`PHYSICAL`] where both are.
static SHOWN: LazyLock<RwLock<Shown>> = LazyLock::new(Default::default);

fn shown_mut() -> RwLockWriteGuard<'static, Shown> {
    SHOWN.write().unwrap_or_else(|e| e.into_inner())
}

impl Shown {
    fn note(&mut self, base: usize, len: usize, slice: &Slice) {
        let section = Arc::as_ptr(&slice.section) as usize;
        if len / PAGE > SHORT {
            self.long.push((base, len, section, slice.index));
            return;
        }
        for k in 0..len / PAGE {
            self.pages
                .entry((section, slice.index + k))
                .or_default()
                .push(base + k * PAGE);
        }
    }

    fn forget(&mut self, base: usize, len: usize, slice: &Slice) {
        if len / PAGE > SHORT {
            self.long.retain(|view| view.0 != base);
            return;
        }
        let section = Arc::as_ptr(&slice.section) as usize;
        for k in 0..len / PAGE {
            let key = (section, slice.index + k);
            if let Some(at) = self.pages.get_mut(&key) {
                at.retain(|&at| at != base + k * PAGE);
                if at.is_empty() {
                    self.pages.remove(&key);
                }
            }
        }
    }

    /// Calls `show` for every virtual address that aliases this section page.
    fn each(&self, section: usize, index: usize, mut show: impl FnMut(usize)) {
        if let Some(pages) = self.pages.get(&(section, index)) {
            pages.iter().copied().for_each(&mut show);
        }
        for &(base, len, of, first) in &self.long {
            if of == section && (first..first + len / PAGE).contains(&index) {
                show(base + (index - first) * PAGE);
            }
        }
    }
}

/// The views, changed by the title's mapping calls; held for as long as
/// those take (the host maps, commits and unmaps under it).
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(Default::default);

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title's pages. Apart from the views: DirectStorage looks pages up at
/// every request, from 24 workers, and used to wait there for the title's
/// mapping calls (12,000 waits of 1 to 170 ms in a load of 90 s). Written
/// only where the title allocates or frees pages. Taken after [`PHYSICAL`]
/// where both are.
static PAGES: LazyLock<RwLock<Pages>> = LazyLock::new(Default::default);

fn page_table() -> RwLockReadGuard<'static, Pages> {
    PAGES.read().unwrap_or_else(|e| e.into_inner())
}

fn page_table_mut() -> RwLockWriteGuard<'static, Pages> {
    PAGES.write().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    if n > (PAGE_NUMBER_LIMIT - FIRST_PAGE_NUMBER) / FRAMES {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section {
        handle,
        pages: n,
        window: std::sync::OnceLock::new(),
        committed: (0..n.div_ceil(64))
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect(),
    });
    // Copy page numbers to the caller without the table lock: a fault in
    // title memory may wait for a mapping call which takes that lock.
    let mut numbers = vec![MaybeUninit::uninit(); n];
    if !page_table_mut().allocate(section.clone(), &mut numbers) {
        // SAFETY: checked non-null above.
        unsafe { count.write(0) };
        return 0;
    }
    // SAFETY: allocate filled all n numbers; the caller gave n writable slots.
    unsafe { std::ptr::copy_nonoverlapping(numbers.as_ptr().cast::<usize>(), pages, n) };
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;
const PAGE_NUMBER_LIMIT: usize = (1 << 21) * FRAMES;
const FIRST_PAGE_NUMBER: usize = 0x10_0000;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut table = page_table_mut();
    // Dropped without the lock: the last page of a section unmaps its window.
    let gone: Vec<Slice> = numbers
        .into_iter()
        .filter_map(|number| table.free(number))
        .collect();
    drop(table);
    drop(gone);
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not mapped for a write");
            return None;
        }
        let view = View { at, len };
        if !slice
            .section
            .commit_shown(slice.index, at.Value as usize, len)
        {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not committed for a write");
            return None;
        }
        Some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    if len == 0 {
        return Some(Vec::new());
    }
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let table = page_table();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let Some(slice) = table.pages.get(page) else {
            tracing::warn!(
                page,
                offset,
                len,
                "a physical page the title does not own (any more)"
            );
            return None;
        };
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether the title owns every page under bytes `[offset, offset + len)`.
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    let Some(end) = offset.checked_add(len) else {
        return false;
    };
    let table = page_table();
    len == 0
        || pages
            .get(offset / PAGE..end.div_ceil(PAGE))
            .is_some_and(|pages| pages.iter().all(|page| table.pages.contains_key(page)))
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let k = (n * PAGE - within).min(data.len() - done);
        if let Some(at) = slice.section.window(slice.index, *n) {
            // SAFETY: committed bytes of the section's view, which `runs`
            // holds; the title's other uses of them race as on the console.
            unsafe {
                std::ptr::copy_nonoverlapping(data[done..].as_ptr(), (at + within) as *mut u8, k)
            };
        } else {
            let Some(mut view) = View::map(slice, *n) else {
                return false;
            };
            view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        }
        notify_written(slice, within, k);
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    // Keeps the exact slices we wrote alive, even if the title frees and
    // reuses their page numbers before the decoder finishes.
    slice: Slice,
    view: Window,
    within: usize,
    len: usize,
}

enum Window {
    Kept(usize),
    Own(View),
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        match &mut self.view {
            // SAFETY: these committed bytes are held by self.slice; the
            // title owns the destination until this request completes.
            Window::Kept(at) => unsafe {
                std::slice::from_raw_parts_mut((*at + within) as *mut u8, len)
            },
            Window::Own(view) => &mut view.bytes()[within..within + len],
        }
    }

    /// On error, the decoder may already have written any part of the window.
    pub(crate) fn finish(mut self, written: usize) {
        self.len = written.min(self.len);
    }
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        notify_written(&self.slice, self.within, self.len);
    }
}

/// A writable window when all touched pages are consecutive slices of one
/// section. A fragmented or unavailable window takes the copy path instead.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let view = match slice.section.window(slice.index, *n) {
        Some(at) => Window::Kept(at),
        None => Window::Own(View::map(slice, *n)?),
    };
    Some(PageWindow {
        slice: slice.clone(),
        view,
        within: offset % PAGE,
        len,
    })
}

/// A write through the section window does not fault at any title alias.
/// Report exactly the bytes that the currently mapped aliases now show.
fn notify_written(slice: &Slice, offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    // No alias can change between looking it up and notifying the GPU.
    let _reading = victory_gpu::title_memory_reading();
    let end = offset + len;
    let mut pieces = Vec::new();
    {
        let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
        let section = Arc::as_ptr(&slice.section) as usize;
        for i in offset / PAGE..end.div_ceil(PAGE) {
            let lo = offset.saturating_sub(i * PAGE);
            let hi = (end - i * PAGE).min(PAGE);
            shown.each(section, slice.index + i, |at| {
                pieces.push((at + lo, hi - lo))
            });
        }
    }
    pieces.sort_unstable();
    let mut run: Option<(usize, usize)> = None;
    for (at, bytes) in pieces {
        if let Some((first, stop)) = &mut run {
            if at <= *stop {
                *stop = (*stop).max(at + bytes);
                continue;
            }
            victory_gpu::kernel_will_write(*first, *stop - *first);
        }
        run = Some((at, at + bytes));
    }
    if let Some((first, stop)) = run {
        victory_gpu::kernel_will_write(first, stop - first);
    }
}

/// Maps n 64 KiB pages, or unmaps the same range when pages is null.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let Some(len) = n.checked_mul(PAGE).filter(|&len| valid_range(base, len)) else {
        return std::ptr::null_mut();
    };
    if len == 0 {
        return address;
    }
    let slices = if pages.is_null() {
        Vec::new()
    } else {
        // Read title memory before taking any HLE or GPU mapping lock. A
        // fault in the page list can wait for a mapping that holds them.
        // SAFETY: the title supplies n page numbers, or null.
        let pages = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
        let Some(slices) = runs(&pages, 0, len) else {
            return std::ptr::null_mut();
        };
        slices
    };
    let mut physical = physical();
    let _changing = victory_gpu::title_memory_remapping();
    if !unmap(&mut physical, base, len) {
        return std::ptr::null_mut();
    }
    let mut at = base;
    for (slice, n) in slices {
        if !map(&mut physical, at, n * PAGE, slice) {
            let _ = unmap(&mut physical, base, at - base);
            return std::ptr::null_mut();
        }
        at += n * PAGE;
    }
    address
}

/// Maps one page per address; a null page list or a zero page unmaps.
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    let wanted: Vec<(usize, usize)> = (0..n)
        // SAFETY: the title supplies n addresses and, unless null, n pages.
        .map(|i| unsafe {
            (*addresses.add(i) as usize, if pages.is_null() { 0 } else { *pages.add(i) })
        })
        .collect();
    scatter(&wanted).into()
}

fn valid_range(at: usize, len: usize) -> bool {
    len == 0
        || (at != 0
            && at.is_multiple_of(PAGE)
            && len.is_multiple_of(PAGE)
            && at.checked_add(len).is_some())
}

/// The GPU queue can alias pages from one title range into a tiled heap.
/// A zero source unmaps instead. Snapshot the source before changing any
/// destination, so an overlapping copy sees the original mappings.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    if !valid_range(to, len) || (from != 0 && !valid_range(from, len)) {
        return false;
    }
    if len == 0 {
        return true;
    }
    let mut physical = physical();
    let Some(ops) = (0..len)
        .step_by(PAGE)
        .map(|i| {
            let slice = if from == 0 {
                None
            } else {
                Some(physical.slice_at(from + i)?)
            };
            Some(Op::new(to + i, slice))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if from == to {
        return true;
    }
    let _changing = victory_gpu::title_memory_remapping();
    run_ops(&mut physical, ops)
}

/// Copies an ordered batch. Independent one-page copies can share a worker
/// batch. A copy that sees a previous destination starts a new batch.
pub fn map_aliases(ranges: &[(u64, u64, u64)]) -> bool {
    let mut ranges = ranges;
    while let Some(&(to, from, len)) = ranges.first() {
        if len != PAGE as u64 || to == from {
            if !map_alias(to, from, len) {
                return false;
            }
            ranges = &ranges[1..];
            continue;
        }
        let mut physical = physical();
        let mut ops = Vec::new();
        for &(to, from, len) in ranges.iter().take(GROUP) {
            let (to, from) = (to as usize, from as usize);
            if len != PAGE as u64
                || to == from
                || ops.iter().any(|op: &Op| op.at == to || op.at == from)
            {
                break;
            }
            if !valid_range(to, PAGE) || (from != 0 && !valid_range(from, PAGE)) {
                break;
            }
            let slice = physical.slice_at(from);
            if from != 0 && slice.is_none() {
                break;
            }
            ops.push(Op::new(to, slice));
        }
        let n = ops.len();
        if n == 0 {
            return false;
        }
        let _changing = victory_gpu::title_memory_remapping();
        if !change(&mut physical, ops) {
            return false;
        }
        ranges = &ranges[n..];
    }
    true
}

fn scatter(wanted: &[(usize, usize)]) -> bool {
    let mut wanted = wanted;
    while !wanted.is_empty() {
        let mut physical = physical();
        let mut ops = Vec::new();
        {
            let table = page_table();
            for &(to, number) in wanted.iter().take(GROUP) {
                if ops.iter().any(|op: &Op| op.at == to) || !valid_range(to, PAGE) {
                    break;
                }
                let slice = table.pages.get(&number).cloned();
                if number != 0 && slice.is_none() {
                    tracing::warn!(number, "XMemMapPhysicalPagesScatter: unknown page");
                    break;
                }
                ops.push(Op::new(to, slice));
            }
        }
        let n = ops.len();
        if n == 0 {
            return false;
        }
        let _changing = victory_gpu::title_memory_remapping();
        if !change(&mut physical, ops) {
            return false;
        }
        wanted = &wanted[n..];
    }
    true
}

/// A small batch limits the time the GPU's readers wait for its views.
const GROUP: usize = 64;

fn mapping_threads() -> usize {
    static THREADS: LazyLock<usize> = LazyLock::new(|| {
        std::thread::available_parallelism()
            .map_or(4, usize::from)
            .min(8)
    });
    *THREADS
}

fn run_ops(physical: &mut Physical, ops: Vec<Op>) -> bool {
    let mut ops = ops.into_iter();
    loop {
        let group: Vec<_> = ops.by_ref().take(GROUP).collect();
        if group.is_empty() {
            return true;
        }
        if !change(physical, group) {
            return false;
        }
    }
}

/// One address-space page to change. Distinct existing one-page views can
/// be unmapped independently. A larger view is split by the caller first.
struct Op {
    at: usize,
    slice: Option<Slice>,
    old: Option<Slice>,
}

impl Op {
    fn new(at: usize, slice: Option<Slice>) -> Self {
        Self {
            at,
            slice,
            old: None,
        }
    }

    fn run(&self) -> Outcome {
        if self.old.is_some() && !unmap_view(self.at) {
            return Outcome::UnmapFailed;
        }
        match &self.slice {
            Some(slice) if !show(self.at, PAGE, slice) => Outcome::MapFailed,
            _ => Outcome::Done,
        }
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Done,
    UnmapFailed,
    MapFailed,
}

fn change(physical: &mut Physical, mut ops: Vec<Op>) -> bool {
    let mut ok = true;
    ops.retain_mut(|op| {
        let there = physical
            .views
            .range(..=op.at)
            .next_back()
            .filter(|(base, view)| op.at < **base + view.0)
            .map(|(&base, view)| (base, view.0));
        match there {
            Some((base, PAGE)) if base == op.at => {
                op.old = physical.views.remove(&base).map(|(_, slice)| slice);
                if let Some(slice) = &op.old {
                    shown_mut().forget(base, PAGE, slice);
                }
                victory_gpu::title_memory_unmapping(base, PAGE);
            }
            Some(_) if !unmap(physical, op.at, PAGE) => {
                ok = false;
                return false;
            }
            _ => {}
        }
        if op.old.is_none() && op.slice.is_some() {
            // Two workers must not split the same placeholder concurrently.
            split_at(op.at);
            split_at(op.at + PAGE);
        }
        true
    });
    for (op, done) in pool::all(ops) {
        match (done, op.slice, op.old) {
            (Outcome::Done, Some(slice), _) => note_view(physical, op.at, PAGE, slice),
            (Outcome::Done, None, _) => {}
            (Outcome::UnmapFailed, _, Some(old)) => {
                note_view(physical, op.at, PAGE, old);
                ok = false;
            }
            _ => {
                tracing::warn!(
                    at = format_args!("{:#x}", op.at),
                    "mapping a physical page failed"
                );
                ok = false;
            }
        }
    }
    ok
}

/// A fixed pool parallelizes host map/unmap calls that are otherwise
/// serialized on the title's streaming thread.
mod pool {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    use super::{Op, Outcome, mapping_threads};

    struct Batch {
        ops: Vec<(Op, OnceLock<Outcome>)>,
        next: AtomicUsize,
        left: AtomicUsize,
        completed: (Mutex<bool>, Condvar),
    }

    impl Batch {
        fn work(&self) {
            while let Some((op, result)) = self.ops.get(self.next.fetch_add(1, Ordering::Relaxed)) {
                let _ = result.set(op.run());
                if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                    *self.completed.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    self.completed.1.notify_one();
                }
            }
        }
    }

    #[derive(Default)]
    struct Pool {
        batches: Mutex<VecDeque<Arc<Batch>>>,
        more: Condvar,
    }

    fn pool() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        static STARTED: std::sync::Once = std::sync::Once::new();
        let pool = POOL.get_or_init(Pool::default);
        STARTED.call_once(|| {
            for i in 1..mapping_threads() {
                if let Err(error) = std::thread::Builder::new()
                    .name(format!("mapping {i}"))
                    .spawn(|| serve(pool))
                {
                    tracing::warn!(%error, "could not start a mapping worker");
                }
            }
        });
        pool
    }

    fn serve(pool: &Pool) {
        let mut batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Some(batch) = batches.front().cloned() else {
                batches = pool.more.wait(batches).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            if batch.next.load(Ordering::Relaxed) >= batch.ops.len() {
                batches.pop_front();
                continue;
            }
            drop(batches);
            batch.work();
            batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn all(ops: Vec<Op>) -> Vec<(Op, Outcome)> {
        if ops.len() < 3 || mapping_threads() == 1 {
            return ops
                .into_iter()
                .map(|op| {
                    let done = op.run();
                    (op, done)
                })
                .collect();
        }
        let n = ops.len();
        let batch = Arc::new(Batch {
            ops: ops.into_iter().map(|op| (op, OnceLock::new())).collect(),
            next: AtomicUsize::new(0),
            left: AtomicUsize::new(n),
            completed: Default::default(),
        });
        let pool = pool();
        pool.batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(batch.clone());
        pool.more.notify_all();
        batch.work();
        let completed = batch.completed.0.lock().unwrap_or_else(|e| e.into_inner());
        drop(
            batch
                .completed
                .1
                .wait_while(completed, |done| !*done)
                .unwrap_or_else(|e| e.into_inner()),
        );
        batch
            .ops
            .iter()
            .map(|(op, done)| {
                (
                    Op {
                        at: op.at,
                        slice: op.slice.clone(),
                        old: op.old.clone(),
                    },
                    *done.get().expect("mapping worker completed the operation"),
                )
            })
            .collect()
    }
}

/// Registers a view only after all its section pages are visible and committed.
fn note_view(physical: &mut Physical, at: usize, len: usize, slice: Slice) {
    shown_mut().note(at, len, &slice);
    victory_gpu::title_memory_remapped(at, len);
    victory_gpu::title_memory_view(
        at,
        len,
        slice.section.handle as usize,
        (slice.index * PAGE) as u64,
    );
    physical.views.insert(at, (len, slice));
}

/// Map the run in one view if possible. Some titles split placeholders
/// themselves, and our earlier views may have split them too. Map each
/// such piece separately, without joining the title's reservations.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    if show(base, len, &slice) {
        note_view(physical, base, len, slice);
        return true;
    }
    let end = base + len;
    split_at(base);
    split_at(end);
    let mut at = base;
    while at < end {
        let next = placeholder(at).map_or(end, |(_, stop)| stop.min(end));
        let part = Slice {
            index: slice.index + (at - base) / PAGE,
            section: slice.section.clone(),
        };
        if next <= at || !(next - at).is_multiple_of(PAGE) || !show(at, next - at, &part) {
            let error = std::io::Error::last_os_error();
            let _ = unmap(physical, base, at - base);
            tracing::warn!(address = format_args!("{at:#x}"), len, %error, "mapping physical pages failed");
            return false;
        }
        note_view(physical, at, next - at, part);
        at = next;
    }
    true
}

/// Replaces exactly one placeholder. The host rejects ordinary reservations
/// or committed memory without modifying them.
fn show(base: usize, len: usize, slice: &Slice) -> bool {
    // SAFETY: the host only replaces a matching placeholder; the section
    // is ours and held through the operation by slice.
    let view = unsafe {
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        return false;
    }
    if !slice.section.commit_shown(slice.index, base, len) {
        let _ = unmap_view(base);
        return false;
    }
    true
}

fn unmap_view(at: usize) -> bool {
    // SAFETY: the caller owns this mapped view, originally a placeholder.
    let gone = unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS {
                Value: at as *mut c_void,
            },
            MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if gone == 0 {
        tracing::warn!(at = format_args!("{at:#x}"), error = %std::io::Error::last_os_error(), "unmapping a physical view failed");
    }
    gone != 0
}

/// The start and end of the placeholder holding `at`, if it may be one
/// (other reserved memory looks the same, but the host refuses to split it
/// or map over it).
fn placeholder(at: usize) -> Option<(usize, usize)> {
    // SAFETY: plain data; zero is a valid value.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: a query of our own address space into `info`.
    let found = unsafe { VirtualQuery(at as *const c_void, &mut info, size) } == size;
    (found && info.State == MEM_RESERVE && info.Type == MEM_PRIVATE).then(|| {
        (
            info.AllocationBase as usize,
            info.BaseAddress as usize + info.RegionSize,
        )
    })
}

/// Splits the placeholder holding `at` in two there, if it starts below.
/// The same call frees reserved memory that replaced a placeholder back to
/// one, committed pages and all, so everything below `at` in it must be
/// reserved: nothing committed is ever freed here.
fn split_at(at: usize) {
    if let Some((start, _)) = placeholder(at)
        && start < at
        && placeholder(start).is_some_and(|(_, stop)| stop >= at)
    {
        // SAFETY: `[start, at)` is reserved and holds no data; the host
        // fails the call on anything but a placeholder or memory that
        // replaced one, and then changes nothing.
        unsafe {
            VirtualFree(
                start as *mut c_void,
                at - start,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            )
        };
    }
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    commit_refused(len, "physical pages", std::io::Error::last_os_error());
    false
}

/// Reports memory the host would not give: at the Windows commit limit to
/// `crashes.log`, since the title usually faults soon after on the null it
/// gets; otherwise as a warning.
fn commit_refused(len: usize, what: &str, error: std::io::Error) {
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(
            len,
            what,
            &error,
            queried.then_some(&info),
        ));
    } else {
        tracing::warn!(len, %error, "committing {what} failed");
    }
}

fn commit_limit_message(
    len: usize,
    what: &str,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of {what}: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns the range to placeholders, remapping any neighboring parts of
/// views with the same sections and offsets as before.
fn unmap(physical: &mut Physical, base: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = base + len;
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(&at, _)| at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(&at, _)| at))
        .collect();
    let mut ok = true;
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        shown_mut().forget(at, size, &slice);
        if at < base || at + size > end {
            victory_gpu::title_memory_splitting(at, size);
        }
        victory_gpu::title_memory_unmapping(at, size);
        if !unmap_view(at) {
            note_view(physical, at, size, slice);
            ok = false;
            continue;
        }
        if at < base {
            ok &= map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            ok &= map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
    ok
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

/// The title's own `VirtualFree`. What the GPU side noted of the memory goes
/// first: the address may be other memory's the moment it is free.
extern "system" fn title_virtual_free(address: *mut c_void, size: usize, kind: u32) -> BOOL {
    const MEM_COALESCE_PLACEHOLDERS: u32 = 1;
    // Splitting and joining placeholders changes nothing they show.
    let _physical = physical();
    let _changing = victory_gpu::title_memory_remapping();
    if kind & (MEM_PRESERVE_PLACEHOLDER | MEM_COALESCE_PLACEHOLDERS) == 0 {
        victory_gpu::title_memory_freed(address as usize, size);
    }
    // SAFETY: the title's own arguments.
    unsafe { VirtualFree(address, size, kind) }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &[
            "xmem.dll",
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-memory-*",
        ],
        exports: vec![
            crate::HleExport {
                name: "VirtualFree",
                address: title_virtual_free as *const () as usize,
            },
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
        assert!(report.contains("4194304 bytes of physical pages"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
        assert!(fallback.contains("4194304 bytes of virtual memory"));
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// A scatter of pages at consecutive addresses maps each one:
    /// each address shows its own page, pages asked for in a different order
    /// still land where they are asked for, a zero page in the middle unmaps
    /// only its own address, and an unmap run goes in one call.
    #[test]
    fn scatter_maps_runs_of_consecutive_addresses() {
        let pages = allocate(3);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        for k in 0..3 {
            assert!(write_physical(&pages[k..], 0, &[b'0' + k as u8]));
        }

        // Three pages at consecutive addresses.
        let addresses = [at(0), at(1), at(2)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, pages.as_ptr()),
            1
        );
        for k in 0..3 {
            // A later replacement of a neighbor can unmap only its own host view.
            assert_eq!(query(at(k)).host.AllocationBase, at(k));
            assert_eq!(
                // SAFETY: mapped read-write.
                unsafe { at(k).cast::<u8>().read() },
                b'0' + k as u8,
                "page {k} at its address"
            );
        }

        // Ask for them out of order: each still lands where it is named.
        let addresses = [at(2), at(0)];
        let swapped = [pages[0], pages[2]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, swapped.as_ptr()),
            1
        );
        assert_eq!(
            // SAFETY: mapped read-write.
            unsafe { at(2).cast::<u8>().read() },
            b'0'
        );
        assert_eq!(
            // SAFETY: mapped read-write.
            unsafe { at(0).cast::<u8>().read() },
            b'2'
        );

        // A zero page ends the run it is in: its address is unmapped, the
        // page after it still maps.
        let addresses = [at(1), at(2)];
        let mixed = [0usize, pages[1]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, mixed.as_ptr()),
            1
        );
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(2).cast::<u8>().read() }, b'1');

        // Three consecutive addresses without pages unmap as one run.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, std::ptr::null()),
            1
        );
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        assert_eq!(
            XMemMapPhysicalPagesScatter([at(0)].as_ptr(), 1, std::ptr::null()),
            1
        );
        assert_eq!(query(at(0)).host.State, MEM_RESERVE);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// Unmapping or remapping one scatter-mapped page must not briefly unmap
    /// an adjacent scatter-mapped page while another thread reads it.
    #[test]
    fn scatter_neighbor_unmap_does_not_fault_concurrent_reader() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let pages = allocate(4);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        let addresses = [at(0), at(1), at(2), at(3)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, pages.as_ptr()),
            1
        );

        let sentinel: u64 = 0x1122_3344_5566_7788;
        let read_addr = at(2).wrapping_byte_add(PAGE - 0x110) as usize;
        // SAFETY: page 2 is mapped read-write and aligned for `u64`.
        unsafe { (read_addr as *mut u64).write(sentinel) };

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0u64;
            while !reader_stop.load(Ordering::Relaxed) {
                // SAFETY: page 2 stays mapped for the lifetime of this thread.
                let value = unsafe { (read_addr as *const u64).read_volatile() };
                assert_eq!(value, sentinel);
                reads += 1;
            }
            reads
        });

        let neighbor = [at(3)];
        let unmap_page = [0usize];
        let remap_page = [pages[3]];
        for _ in 0..256 {
            assert_eq!(
                XMemMapPhysicalPagesScatter(neighbor.as_ptr(), 1, unmap_page.as_ptr()),
                1
            );
            assert_eq!(
                XMemMapPhysicalPagesScatter(neighbor.as_ptr(), 1, remap_page.as_ptr()),
                1
            );
        }

        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().expect("reader thread should not fault");
        assert!(reads > 0);

        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, std::ptr::null()),
            1
        );
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                // (Once at least: the mapping thread may be done before this
                // one's first turn.)
                while reads == 0 || !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..100 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = page_table().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn freed_page_numbers_can_be_reused_at_the_limit() {
        let section = || {
            let numbers = allocate(1);
            let section = page_table().pages[&numbers[0]].section.clone();
            assert_eq!(XMemFreePhysicalPages(1, numbers.as_ptr()), 1);
            section
        };
        let mut table = Pages {
            next_page: PAGE_NUMBER_LIMIT - FRAMES,
            ..Pages::default()
        };
        let mut number = [MaybeUninit::new(0)];
        assert!(table.allocate(section(), &mut number));
        // SAFETY: the initialized slot was filled by allocate.
        let last = unsafe { number[0].assume_init() };
        assert_eq!(last >> 4, (1 << 21) - 1);
        assert!(!table.allocate(section(), &mut number));

        let mut old_view = View::map(&table.pages[&last], 1).unwrap();
        old_view.bytes()[0] = 7;
        assert!(table.free(last).is_some());
        assert!(table.free(last).is_none());
        // A second free must not grant another use of the same number.
        let mut two = [MaybeUninit::new(0); 2];
        assert!(!table.allocate(section(), &mut two));
        assert!(table.allocate(section(), &mut number));
        // SAFETY: the initialized slot was filled by allocate.
        assert_eq!(unsafe { number[0].assume_init() }, last);
        let mut new_view = View::map(&table.pages[&last], 1).unwrap();
        assert_eq!(new_view.bytes()[0], 0);
        assert_eq!(old_view.bytes()[0], 7);
    }

    /// Streaming allocations must keep reusing page numbers even when no more
    /// fit in the title's packed field. Keep the table private so concurrent
    /// tests cannot take the numbers freed between rounds.
    #[test]
    fn streaming_allocations_reuse_page_numbers_and_keep_slices_contiguous() {
        let numbers = allocate(2);
        let section = page_table().pages[&numbers[0]].section.clone();
        assert_eq!(XMemFreePhysicalPages(numbers.len(), numbers.as_ptr()), 1);

        let mut table = Pages {
            next_page: PAGE_NUMBER_LIMIT - 2 * FRAMES,
            ..Pages::default()
        };
        let expected = [PAGE_NUMBER_LIMIT - 2 * FRAMES, PAGE_NUMBER_LIMIT - FRAMES];
        let mut slots = [MaybeUninit::uninit(); 2];
        let mut rejected = [MaybeUninit::new(0)];
        for _ in 0..1024 {
            assert!(table.allocate(section.clone(), &mut slots));
            // SAFETY: a successful allocation filled both slots.
            let numbers = unsafe { [slots[0].assume_init(), slots[1].assume_init()] };
            let mut sorted = numbers;
            sorted.sort_unstable();
            assert_eq!(sorted, expected);
            assert!(!table.allocate(section.clone(), &mut rejected));
            // SAFETY: the slot was initialized with a sentinel.
            let untouched = unsafe { rejected[0].assume_init() };
            assert_eq!(untouched, 0);

            for (index, number) in numbers.into_iter().enumerate() {
                let slice = table.free(number).expect("live page");
                assert!(Arc::ptr_eq(&slice.section, &section));
                assert_eq!(slice.index, index);
                assert!(table.free(number).is_none());
            }
            assert!(table.free(PAGE_NUMBER_LIMIT).is_none());
        }

        let base = section.window(0, 2).expect("whole section");
        assert_eq!(section.window(1, 1), Some(base + PAGE));
        assert_eq!(section.window(1, 2), None);
        assert_eq!(section.window(usize::MAX, 1), None);
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn streamed_pages_refresh_only_the_aliases_present_at_completion() {
        let (pages, base) = pages_and_reservation(4);
        let at = |i: usize| base.wrapping_byte_add(i * PAGE);
        let map_one = |i: usize, page: usize| {
            assert_eq!(XMemMapPhysicalPages(at(i), 1, &page), at(i));
        };
        map_one(0, pages[0]);
        map_one(1, pages[0]);
        map_one(2, pages[2]);
        map_one(3, pages[3]);
        let mut pending = page_window(&pages, 5, 1).expect("one section page");

        // While the decoder owns a window, replace its first alias, add
        // another, and let the GPU refresh all four addresses.
        map_one(0, pages[3]);
        map_one(2, pages[0]);
        let changes = |since| victory_gpu::buffer_writes_since(base as usize, 4 * PAGE, since);
        let (before, _) = changes(0);
        assert!(changes(before).1.is_empty());
        pending.bytes()[0] = 71;
        drop(pending);
        let (after_window, dirty) = changes(before);
        assert_eq!(dirty, [at(1) as usize, at(2) as usize]);
        // Reordered physical pages take the copy path. Only the page
        // containing the final byte and the page following it changed.
        assert!(write_physical(&[pages[3], pages[0]], PAGE - 1, &[1, 2]));
        assert_eq!(
            changes(after_window).1,
            [at(0), at(1), at(2), at(3)].map(|p| p as usize)
        );

        assert_eq!(XMemMapPhysicalPages(base, 4, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// `n` physical pages (consecutive slices of one section) and a
    /// reservation of `n` 64 KiB pages for them.
    fn pages_and_reservation(n: usize) -> (Vec<usize>, *mut c_void) {
        let mut pages = vec![0usize; n];
        let mut count = n;
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
            1
        );
        let reservation = XMemVirtualAlloc(
            std::ptr::null_mut(),
            n * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!reservation.is_null());
        (pages, reservation)
    }

    #[test]
    fn an_overlapping_alias_copy_reads_the_original_pages() {
        let (pages, a) = pages_and_reservation(4);
        let at = |i: usize| a.wrapping_byte_add(i * PAGE);
        for (i, page) in pages.iter().enumerate() {
            assert!(write_physical(&[*page], 0, &[i as u8 + 1]));
        }
        assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
        let mapping =
            |dst: usize, src: usize, n: usize| (at(dst) as u64, at(src) as u64, (n * PAGE) as u64);
        // The first copy overlaps. Each subsequent copy must see the
        // source left by the previous command, even across worker batches.
        assert!(map_aliases(&[
            mapping(1, 0, 3),
            mapping(0, 3, 1),
            mapping(3, 0, 1)
        ]));
        let values = std::array::from_fn::<_, 4, _>(|i| {
            // SAFETY: the mapped pages are still committed and readable.
            unsafe { at(i).cast::<u8>().read() }
        });
        assert_eq!(values, [3, 1, 2, 3]);
        assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    #[test]
    fn unknown_source_page_does_not_destroy_an_existing_mapping() {
        let (pages, a) = pages_and_reservation(2);
        let second = a.wrapping_byte_add(PAGE);
        assert!(write_physical(&pages, 0, &[42]));
        assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
        assert!(!map_alias(a as u64, second as u64, PAGE as u64));
        assert!(XMemMapPhysicalPages(a, 2, [pages[1], 0].as_ptr()).is_null());
        // SAFETY: the original view still covers this address.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 42);
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// Unmapping part of a run leaves the range as several host
    /// placeholders; mapping a run over all of them again works, and the
    /// pages keep what was written through the first mapping.
    #[test]
    fn a_run_maps_again_after_a_partial_unmap() {
        let (pages, a) = pages_and_reservation(3);
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe {
            for page in 0..3 {
                at(page).cast::<u8>().write(10 + page as u8);
            }
        }
        assert_eq!(XMemMapPhysicalPages(at(1), 1, std::ptr::null()), at(1));
        assert_eq!(query(at(1)).host.State, MEM_RESERVE);

        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        for page in 0..3 {
            assert_eq!(query(at(page)).host.State, MEM_COMMIT, "page {page}");
            // SAFETY: mapped read-write.
            assert_eq!(unsafe { at(page).cast::<u8>().read() }, 10 + page as u8);
        }
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(query(a).host.State, MEM_RESERVE);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A run that starts inside one host placeholder and ends inside the
    /// next (left by an earlier, shorter run) maps, and the pages around it
    /// stay unmapped.
    #[test]
    fn a_run_maps_across_two_placeholders() {
        let (pages, a) = pages_and_reservation(4);
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        // Mapping and unmapping pages 0-1 splits the reservation in two.
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);

        assert_eq!(XMemMapPhysicalPages(at(1), 2, pages[1..].as_ptr()), at(1));
        assert_eq!(query(a).host.State, MEM_RESERVE);
        assert_eq!(query(at(1)).host.State, MEM_COMMIT);
        assert_eq!(query(at(2)).host.State, MEM_COMMIT);
        assert_eq!(query(at(3)).host.State, MEM_RESERVE);
        // Page 3, split off by the run, still takes a mapping: the run's
        // last page shows there too.
        // SAFETY: mapped read-write.
        unsafe {
            at(2).cast::<u8>().write(42);
            assert_eq!(XMemMapPhysicalPages(at(3), 1, pages[2..].as_ptr()), at(3));
            assert_eq!(at(3).cast::<u8>().read(), 42);
        }
        assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Placeholders a run spans are never joined: a piece the title split
    /// off itself is still its own after the run is unmapped, so the title
    /// can release it.
    #[test]
    fn a_run_keeps_the_placeholders_it_spans() {
        let (pages, a) = pages_and_reservation(4);
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        // SAFETY: splits the reservation's placeholder, as a title does.
        let split = unsafe { VirtualFree(a, 2 * PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
        assert_ne!(split, 0);
        assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
        assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        let info = query(at(2)).host;
        assert_eq!((info.AllocationBase, info.RegionSize), (at(2), 2 * PAGE));
        // SAFETY: the title's own piece, unmapped.
        assert_ne!(unsafe { VirtualFree(at(2), 0, MEM_RELEASE) }, 0);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// A run whose second part is no placeholder (memory committed in place
    /// of one) fails: its first part is unmapped again, and the committed
    /// memory, which the run ends inside, keeps its data.
    #[test]
    fn a_failed_run_leaves_no_view() {
        let (pages, a) = pages_and_reservation(3);
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        // SAFETY: splits the placeholder, makes pages 1-2 an ordinary
        // allocation in place and commits page 1 of it.
        unsafe {
            assert_ne!(
                VirtualFree(a, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
                0
            );
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                at(1),
                2 * PAGE,
                MEM_RESERVE | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(p, at(1));
            assert!(!VirtualAlloc(at(1), PAGE, MEM_COMMIT, PAGE_READWRITE).is_null());
            at(1).cast::<u8>().write(99);
        }
        assert!(XMemMapPhysicalPages(a, 2, pages.as_ptr()).is_null());
        assert_eq!(query(a).host.State, MEM_RESERVE);
        assert_eq!(query(at(1)).host.State, MEM_COMMIT);
        // SAFETY: still committed.
        assert_eq!(unsafe { at(1).cast::<u8>().read() }, 99);
        // Page 0 is a placeholder again: it takes a mapping.
        assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }
}
