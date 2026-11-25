//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles every mip covered by the upload.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (from the first mip of a placed 2D view, otherwise every mip and array slice).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Bytes in the host mapping.
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// First host subresource and bytes read from the title allocation.
    first: u32,
    tiled_bytes: usize,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    /// Original packed 16-bit format (86 or 115) when the host texture was
    /// widened to R8G8B8A8_UNORM, so detiled 2-byte texels expand into the
    /// 4-byte staging buffer.
    widened_from: Option<u32>,
    last: Mutex<Last>,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<(u32, blake3::Hash)>,
    asked: u64,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

type Stagings = HashMap<usize, Vec<Arc<Staging>>>;

static STAGING: Mutex<Option<Stagings>> = Mutex::new(None);

thread_local! {
    /// Texture memory read for hashing and detiling: one per submitting
    /// thread, as large as the largest texture it read.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(
    list: &HostList,
    placed: This,
    source: This,
    on_dispatch: impl FnOnce(),
) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let packed = crate::resource::widened_packed_format(source);
    let through = if packed.is_none() {
        let Some(buffer) = alias_buffer(placed, source, to, from) else {
            return false;
        };
        Some(buffer)
    } else {
        None
    };
    let first = !list.covers_texture(placed as usize, 0);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list
        .transition(source, COPY_SOURCE)
        .map(|(texture, before)| {
            let state = crate::host::legal_state(texture, COPY_SOURCE);
            let barrier = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(state as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
            before
        });
    if !first && before.is_none() {
        return true;
    }
    crate::list::ensure_state(list, placed, COPY_DEST);
    let copied = if let Some(through) = through {
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
        copy_alias_bits(list, to, from, &through);
        true
    } else if let Some(format) = packed {
        let converted = crate::packed_copy::record_alias(list, to, from, format);
        if converted {
            on_dispatch();
        }
        converted
    } else {
        false
    };
    if copied {
        list.use_texture(placed as usize, 0);
    }
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    copied
}

fn alias_buffer(
    placed: This,
    source: This,
    to: &ID3D12Resource,
    from: &ID3D12Resource,
) -> Option<Arc<Through>> {
    let host = host()?;
    {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => Some(through.clone()),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return None;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return None;
                }
                let buffer = host.default_buffer(bytes[0])?;
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                Some(through)
            }
        }
    }
}

fn copy_alias_bits(list: &HostList, to: &ID3D12Resource, from: &ID3D12Resource, through: &Through) {
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&through.buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(&through.buffer, read, common)]);
    }
    list.keep(&through.buffer);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let staging = {
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_mut().and_then(|map| map.remove(&resource))
    };
    {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = set.as_mut() {
            set.remove(&resource);
        }
    }
    if let Some(staging) = staging {
        crate::host::retire(staging);
    }
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records the full-chain copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    upload_from(list, resource, texture, 0);
}

/// Uploads the levels the placed view can sample, starting at `first`.
pub(crate) fn upload_from(list: &HostList, resource: This, texture: &ID3D12Resource, first: u32) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || list.covers_texture(resource as usize, first) {
        return;
    }
    let Some(staging) = staging(resource, texture, first) else {
        return;
    };
    if !list.use_texture(resource as usize, staging.first) {
        return;
    }
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        let subresource = staging.first + i as u32;
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = staging.footprint_bytes(i) as u64;
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {subresource} from {} levels from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: subresource,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging);
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let size = self.tiled_bytes;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some((now, written)) = crate::watch::writes(start, start + size, false) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            last.hash = None;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_tiled_copy(self.memory, size, |tiled| self.detile(tiled));
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        last.asked = 0;
        let resident = committed_prefix(self.memory, size);
        // A mip tail can be selected by its resident prefix only when the
        // smallest mip occupies less than the whole layout. Arrays and 3D
        // textures can have resident slices after a hole; copy their full
        // range through the sparse reader below.
        if resident < size && self.layout.bytes_from(self.mips - 1) < self.layout.size {
            let end = self.mips;
            let first =
                (self.first..end).find(|&mip| self.layout.bytes_from(mip) <= resident as u64);
            let bytes = first.map_or(0, |mip| self.layout.bytes_from(mip) as usize);
            with_tiled_copy(self.memory, bytes, |tiled| {
                self.changed(last, tiled, first.unwrap_or(end));
            });
            return bytes;
        }
        // Read through the kernel, as above: the title may unmap the pages
        // (evicting a texture a big table lists) while we read them, and a
        // direct read would fault. The copy is reused, not kept per texture.
        SCRATCH.with_borrow_mut(|tiled| {
            tiled.resize(size, 0);
            crate::host::copy_mapped(tiled.as_mut_ptr(), self.memory, size, false);
            self.changed(last, tiled, self.first)
        })
    }

    /// After reading the texture's memory into `tiled`: detiles it unless
    /// its hash is the last one's. Returns the bytes read.
    fn changed(&self, last: &mut Last, tiled: &[u8], first: u32) -> usize {
        let size = tiled.len();
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some((first, hash)) {
            return size;
        }
        last.hash = Some((first, hash));
        self.detile_from(tiled, first);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    // D3D12 may omit the padding after the final row of the final footprint.
    fn footprint_bytes(&self, i: usize) -> usize {
        let fp = self.footprints[i];
        let padded = u64::from(fp.Footprint.RowPitch)
            * u64::from(self.rows[i])
            * u64::from(fp.Footprint.Depth);
        padded.min(self.size - fp.Offset) as usize
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        self.detile_from(tiled, self.first);
    }

    fn detile_from(&self, tiled: &[u8], first: u32) {
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        let mut packed = Vec::new();
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let sub = self.first + i as u32;
            let (mip, slice) = (sub % self.mips, sub / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = self.footprint_bytes(i);
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if sub < first {
                linear.fill(0);
                continue;
            }
            if let Err(e) = detile_subresource(
                &self.layout,
                tiled,
                mip,
                slice,
                linear,
                pitch,
                rows as usize,
                f.Depth as usize,
                self.widened_from,
                &mut packed,
            ) {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

/// Reads a texture for the duration of its detile. Keep the initialized
/// length so alternating large and small textures do not re-zero the tail.
fn with_tiled_copy(memory: *const u8, size: usize, detile: impl FnOnce(&[u8])) {
    SCRATCH.with_borrow_mut(|read| {
        if size > read.len() {
            read.reserve_exact(size - read.len());
            read.resize(size, 0);
        }
        let tiled = &mut read[..size];
        crate::host::copy_mapped(tiled.as_mut_ptr(), memory, size, false);
        detile(tiled);
    });
}

#[allow(clippy::too_many_arguments)]
fn detile_subresource(
    layout: &victory_formats::texture::Layout,
    tiled: &[u8],
    mip: u32,
    slice: u32,
    linear: &mut [u8],
    row_pitch: usize,
    rows: usize,
    depth: usize,
    widened_from: Option<u32>,
    packed: &mut Vec<u8>,
) -> Result<(), victory_formats::texture::Error> {
    let Some(packed_format) = widened_from else {
        return layout.detile(tiled, mip, slice, linear, row_pitch, row_pitch * rows);
    };
    let width = layout
        .mips
        .get(mip as usize)
        .map_or(0, |m| m.width as usize);
    let packed_pitch = width * 2;
    let packed_plane = packed_pitch * rows;
    packed.resize(packed_plane * depth, 0);
    layout.detile(tiled, mip, slice, packed, packed_pitch, packed_plane)?;
    crate::resource::expand_packed16_rows(
        packed_format,
        width,
        rows * depth,
        packed,
        packed_pitch,
        linear,
        row_pitch,
    );
    Ok(())
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    committed_prefix(at, len) == len
}

fn committed_prefix(at: *const u8, len: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            break;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    p.min(end) - at as usize
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource, requested: u32) -> Option<Arc<Staging>> {
    let mut cache = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    let first_mip = |layout: &victory_formats::texture::Layout, mips| {
        if requested < mips && layout.bytes_from(requested) < layout.size {
            requested
        } else {
            0
        }
    };
    if let Some(s) = map.get(&(resource as usize)).and_then(|ranges| {
        let cached = ranges.first()?;
        let first = first_mip(&cached.layout, cached.mips);
        ranges.iter().find(|s| s.first == first)
    }) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let first = first_mip(&layout, u32::from(d.MipLevels));
    let tiled_bytes = layout.bytes_from(first) as usize;
    let title_format = crate::resource::pc_format(desc.format);
    let widened_from = (d.Format.0 as u32 == crate::resource::RGBA8_UNORM
        && crate::resource::PACKED_16.contains(&title_format))
    .then_some(title_format);
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    } - first;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            first,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        buffer,
        size: total,
        first,
        tiled_bytes,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        memory,
        widened_from,
        last: Mutex::default(),
    });
    map.entry(resource as usize).or_default().push(s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_texture_reads_reuse_space_and_expose_only_current_bytes() {
        #[cfg(feature = "diagnostics")]
        if crate::copystat::mode() == 2 {
            return; // this mode skips all copies
        }
        let large = [0x67; 8192];
        let small = [0x21; 128];
        let next = [0x43; 8192];
        let mut allocation = None;
        for source in [&large[..], &small[..], &next[..]] {
            with_tiled_copy(source.as_ptr(), source.len(), |tiled| {
                assert_eq!(tiled, source);
                if let Some(first) = allocation {
                    assert_eq!(tiled.as_ptr(), first);
                } else {
                    allocation = Some(tiled.as_ptr());
                }
            });
        }
    }

    #[test]
    fn unmapped_texture_pages_do_not_keep_previous_pixels() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };

        #[cfg(feature = "diagnostics")]
        if crate::copystat::mode() != 0 {
            return; // diagnostic copy overrides do not handle unmapped pages
        }
        const PAGE: usize = 4096;
        let previous = [0x65; 2 * PAGE];
        with_tiled_copy(previous.as_ptr(), previous.len(), |tiled| {
            assert_eq!(tiled, previous);
        });
        // SAFETY: reserve two pages at an address chosen by Windows.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_NOACCESS) };
        assert!(!base.is_null());
        // SAFETY: commit the first page of that reservation.
        let mapped = unsafe { VirtualAlloc(base, PAGE, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(mapped, base);
        // SAFETY: the first page is writable.
        unsafe { base.cast::<u8>().write_bytes(0x32, PAGE) };
        with_tiled_copy(base.cast(), 2 * PAGE, |tiled| {
            assert!(tiled[..PAGE].iter().all(|&b| b == 0x32));
            assert!(tiled[PAGE..].iter().all(|&b| b == 0));
        });
        // SAFETY: this reservation is no longer used.
        assert_ne!(unsafe { VirtualFree(base, 0, MEM_RELEASE) }, 0);
    }

    fn test_texture(
        width: u32,
        mips: u16,
        slices: u16,
        address: Option<u64>,
    ) -> (This, &'static ID3D12Resource) {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use windows::core::Interface;

        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: width,
            DepthOrArraySize: slices,
            MipLevels: mips,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let iid = ID3D12Resource::IID;
        let mut tex = null;
        let result = if let Some(at) = address {
            crate::resource::create_at_address(
                null,
                at,
                (&raw const desc).cast(),
                0,
                null.cast(),
                (&raw const iid).cast(),
                &mut tex,
            )
        } else {
            crate::resource::create_committed(
                null,
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                0,
                null.cast(),
                (&raw const iid).cast(),
                &mut tex,
            )
        };
        assert_eq!(result, crate::object::S_OK);
        let host_tex = crate::resource::host_resource(tex).expect("host texture");
        (tex, host_tex)
    }

    #[test]
    fn partially_resident_arrays_keep_slices_on_both_sides_of_a_hole() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };

        if host().is_none() {
            eprintln!("skipping texture array staging: no D3D12 device");
            return;
        }
        #[cfg(feature = "diagnostics")]
        if crate::copystat::mode() != 0 {
            return;
        }
        const RESERVED: usize = 2 << 20;
        // SAFETY: create a new writable reservation for all three slices.
        let memory = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                RESERVED,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!memory.is_null());
        let (resource, texture) = test_texture(128, 1, 3, Some(memory as u64));
        let s = staging(resource, texture, 0).unwrap();
        let stride = s.layout.slice_size as usize;
        assert_eq!(s.tiled_bytes, 3 * stride);
        assert!(s.tiled_bytes <= RESERVED && stride.is_multiple_of(4096));
        let slice = |n: usize| memory.cast::<u8>().wrapping_add(n * stride);
        for (n, byte) in [0x16, 0x58, 0x9a].into_iter().enumerate() {
            // SAFETY: each slice fits within the committed reservation.
            unsafe { slice(n).write_bytes(byte, stride) };
        }
        let pixels = || {
            s.footprints
                .iter()
                .map(|fp| {
                    // SAFETY: each footprint begins within the live mapped staging buffer.
                    unsafe { s.data.add(fp.Offset as usize).read() }
                })
                .collect::<Vec<_>>()
        };
        s.refresh_now();
        assert_eq!(pixels(), [0x16, 0x58, 0x9a]);
        // SAFETY: slice 1 is page-aligned and occupies only its own pages.
        let freed = unsafe { VirtualFree(slice(1).cast(), stride, MEM_DECOMMIT) };
        assert_ne!(freed, 0);
        s.refresh_now();
        assert_eq!(pixels(), [0x16, 0, 0x9a]);
        // SAFETY: the middle slice remains reserved and may be committed again.
        let mapped = unsafe { VirtualAlloc(slice(1).cast(), stride, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(mapped, slice(1).cast());
        // SAFETY: the preceding call recommitted this slice.
        unsafe { slice(1).write_bytes(0xbc, stride) };
        s.refresh_now();
        assert_eq!(pixels(), [0x16, 0xbc, 0x9a]);

        drop(s);
        let release = crate::object::slot::<extern "system" fn(This) -> u32>(resource, 0x10);
        assert_eq!(release(resource), 0);
        // SAFETY: the resource and staging no longer refer to the reservation.
        assert_ne!(unsafe { VirtualFree(memory, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn placed_texture_upload_reads_the_resident_span_and_copies_to_the_requested_mip() {
        let Some(host) = host() else {
            eprintln!("skipping texture readback: no D3D12 device");
            return;
        };
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };
        // SAFETY: reserve space for the complete texture without committing its pages.
        let memory = unsafe { VirtualAlloc(std::ptr::null(), 2 << 20, MEM_RESERVE, PAGE_NOACCESS) };
        assert!(!memory.is_null());
        let (resource, texture) = test_texture(256, 9, 1, Some(memory as u64));
        let mut words = [0; 16];
        words[1] = D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32;
        words[4] = 8;
        words[5] = 1;
        let view = crate::descriptor::View::Placed(false, [0; 14], words);
        let first = crate::descriptor::host::placed_first_mip(&view);
        assert_eq!(first, 8);

        let list = host.new_list().unwrap();
        upload_from(&list, resource, texture, first);
        let tail = staging(resource, texture, first).unwrap();
        assert_eq!(tail.first, 8);
        assert_eq!(tail.footprints.len(), 1);
        assert_eq!(tail.tiled_bytes as u64, tail.layout.bytes_from(first));
        assert!((tail.tiled_bytes as u64) < tail.layout.size);
        // SAFETY: the tail fits in the reservation and is the only committed span.
        let mapped_tail =
            unsafe { VirtualAlloc(memory, tail.tiled_bytes, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(mapped_tail, memory);
        assert!(!committed(memory.cast(), tail.layout.size as usize));
        // SAFETY: the committed tail is writable through the resource's address.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(tail.memory.cast_mut(), tail.tiled_bytes) };
        let pixel = [0xa3, 0xb5, 0xc7, 0xd9];
        tail.layout.tile(bytes, first, 0, &pixel, 4, 4).unwrap();
        assert_eq!(tail.refresh_now(), tail.tiled_bytes);
        // Snapshot the partial copy on the GPU before another upload can overwrite it.
        let read_mip = || {
            crate::list::ensure_state(&list, resource, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
            let (back, mapped) = host.readback_buffer(tail.size).unwrap();
            let dest = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: the readback buffer lives through execution.
                pResource: unsafe { std::mem::transmute_copy(&back) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: tail.footprints[0],
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: the host texture lives through execution.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: first,
                },
            };
            // SAFETY: both locations describe the same 1x1 mip in the copy states.
            unsafe { list.list.CopyTextureRegion(&dest, 0, 0, 0, &src, None) };
            (back, mapped)
        };
        let (_partial_buffer, partial_pixel) = read_mip();
        // The placed table, full-chain copy source, and another list can all use
        // this texture before any of their deferred detiles have run.
        let other = host.new_list().unwrap();
        upload_from(&other, resource, texture, 2);
        let broader = staging(resource, texture, 2).unwrap();
        assert!(other.covers_texture(resource as usize, 2));
        assert!(!list.covers_texture(resource as usize, 2));
        upload(&list, resource, texture);
        assert!(list.covers_texture(resource as usize, 0));
        assert!(Arc::ptr_eq(
            &tail,
            &staging(resource, texture, first).unwrap()
        ));
        assert!(Arc::ptr_eq(
            &broader,
            &staging(resource, texture, 2).unwrap()
        ));
        let (_full_buffer, full_pixel) = read_mip();
        list.close();
        host.execute(&[&list]);
        // SAFETY: execution finished both copies and both mapped buffers are still live.
        let (partial, full) = unsafe {
            (
                partial_pixel.cast::<[u8; 4]>().read_unaligned(),
                full_pixel.cast::<[u8; 4]>().read_unaligned(),
            )
        };
        assert_eq!(partial, pixel, "partial upload wrote mip 8");
        assert_eq!(full, pixel, "full-chain upload preserved mip 8");
        other.close();
        other.reset();
        list.reset();
        assert!(!list.covers_texture(resource as usize, first));
        let release = crate::object::slot::<extern "system" fn(This) -> u32>(resource, 0x10);
        assert_eq!(release(resource), 0);
        // SAFETY: the lists have been reset and the resource has been released.
        assert_ne!(unsafe { VirtualFree(memory, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn forget_drops_staging_immediately_when_idle() {
        let _retire = crate::host::isolated_retire_scope();
        if host().is_none() {
            return;
        }
        let (tex, host_tex) = test_texture(4, 1, 1, None);
        let weak = {
            let s = staging(tex, host_tex, 0).expect("staging");
            Arc::downgrade(&s)
        };
        assert_eq!(weak.strong_count(), 1);
        forget(tex as usize);
        assert_eq!(weak.strong_count(), 0);
        let release = crate::object::slot::<extern "system" fn(This) -> u32>(tex, 0x10);
        assert_eq!(release(tex), 0);
    }

    #[test]
    fn forget_retires_staging_until_active_epoch_finishes() {
        let _retire = crate::host::isolated_retire_scope();
        let Some(host) = host() else {
            return;
        };
        let (tex, host_tex) = test_texture(4, 1, 1, None);
        let list = host.new_list().expect("host list");
        list.mark_work();

        let weak = {
            let s = staging(tex, host_tex, 0).expect("staging");
            Arc::downgrade(&s)
        };
        assert_eq!(weak.strong_count(), 1);

        forget(tex as usize);
        assert_eq!(
            weak.strong_count(),
            1,
            "forgotten texture staging must wait for active epochs"
        );

        list.close();
        list.reset();
        assert_eq!(weak.strong_count(), 0);
        let release = crate::object::slot::<extern "system" fn(This) -> u32>(tex, 0x10);
        assert_eq!(release(tex), 0);
    }

    /// A synthetic version of a title that packs light into an integer
    /// texture from compute and samples it through a view of another format
    /// placed at the same address: the view's texels are the written
    /// texture's, not the (empty) title memory behind it.
    #[test]
    fn view_placed_over_a_compute_written_texture_reads_what_compute_wrote() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32_UINT, DXGI_SAMPLE_DESC,
        };
        use windows::core::{Interface, s};

        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        const UNORDERED_ACCESS: u32 = 0x8;
        const TEXELS: usize = 16;

        /// A title object, released when the test ends.
        struct Title(This);
        impl Drop for Title {
            fn drop(&mut self) {
                crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
            }
        }

        let _retire = crate::host::isolated_retire_scope();
        let Some(host) = host() else {
            return; // no host GPU
        };

        // The title's memory for both textures. Nothing writes it, so a
        // view filled from it reads zeros. The placed view is never
        // released, so the memory is never freed.
        let memory: &'static mut [u8] = Box::leak(vec![0u8; 1 << 20].into_boxed_slice());
        let address = memory.as_ptr() as u64;

        // The words of a `ResourceDesc`: 4x4, one level and layer.
        let words = |format: i32, flags: u32| {
            let mut w = [0u32; 14];
            w[0] = 3; // texture 2D
            w[4] = 4; // width
            w[6] = 4; // height
            w[7] = 1 | (1 << 16); // layers, levels
            w[8] = format as u32;
            w[9] = 1; // samples
            w[12] = flags;
            w
        };
        const ALLOW_UNORDERED_ACCESS: u32 = 0x4;
        #[repr(C, align(8))]
        struct Aligned([u32; 14]);
        let written_desc = Aligned(words(DXGI_FORMAT_R32_UINT.0, ALLOW_UNORDERED_ACCESS));
        let iid = ID3D12Resource::IID;
        let mut written = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                address,
                written_desc.0.as_ptr().cast(),
                0,
                std::ptr::null(),
                (&raw const iid).cast(),
                &mut written,
            ),
            crate::object::S_OK
        );
        let _written = Title(written);
        let written_host = crate::resource::host_resource(written).expect("host texture");
        let placed = crate::resource::texture_at(address, &words(DXGI_FORMAT_R32_FLOAT.0, 0))
            .expect("placed view");
        let placed_host = crate::resource::host_resource(placed).expect("host texture");
        assert_ne!(placed, written);
        assert_eq!(
            crate::resource::alias_of(placed),
            Some(written),
            "the view is found to lie over the texture the host writes"
        );

        // Stores a distinct, ordinary float bit pattern per texel; reads
        // the view as floats and stores their bits.
        const SOURCE: &str = r#"
#define RS "DescriptorTable(UAV(u0, numDescriptors=2), SRV(t0, numDescriptors=1))"
RWTexture2D<uint> light : register(u0);
RWByteAddressBuffer dst : register(u1);
Texture2D<float> sampled : register(t0);
[RootSignature(RS)]
[numthreads(4, 4, 1)]
void write(uint3 id : SV_DispatchThreadID) {
    light[id.xy] = 0x3F000000u | ((id.y * 4 + id.x + 1) * 0x1357u);
}
[RootSignature(RS)]
[numthreads(4, 4, 1)]
void read(uint3 id : SV_DispatchThreadID) {
    dst.Store((id.y * 4 + id.x) * 4, asuint(sampled.Load(int3(id.xy, 0))));
}
"#;
        let compile = |entry: windows::core::PCSTR| -> ID3DBlob {
            let mut blob: Option<ID3DBlob> = None;
            // SAFETY: the source and names are static strings.
            unsafe {
                D3DCompile(
                    SOURCE.as_ptr().cast(),
                    SOURCE.len(),
                    s!("alias_view"),
                    None,
                    None,
                    entry,
                    s!("cs_5_1"),
                    0,
                    0,
                    &mut blob,
                    None,
                )
                .unwrap();
            }
            blob.unwrap()
        };
        let (write_code, read_code) = (compile(s!("write")), compile(s!("read")));
        let bytes = |blob: &ID3DBlob| -> Vec<u8> {
            // SAFETY: the blob holds its own bytes.
            unsafe {
                std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                )
            }
            .to_vec()
        };
        let (write_code, read_code) = (bytes(&write_code), bytes(&read_code));
        // SAFETY: the bytecode includes the root signature.
        let root: ID3D12RootSignature =
            unsafe { host.device.CreateRootSignature(0, &write_code) }.unwrap();
        let pipeline = |code: &[u8]| -> ID3D12PipelineState {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: borrowed for the call only.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            // SAFETY: a valid description.
            unsafe { host.device.CreateComputePipelineState(&desc) }.unwrap()
        };
        let (write_pipeline, read_pipeline) = (pipeline(&write_code), pipeline(&read_code));

        let buffer = |kind, flags, state| -> ID3D12Resource {
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
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: valid buffer creation parameters.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let out = buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let readback = buffer(
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // SAFETY: plain object creation and views of live resources, into
        // slots of a heap with room for all three.
        let (heap, table) = unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 3,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart();
            let slot = |n: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + n * step,
            };
            let light = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_UAV {
                        MipSlice: 0,
                        PlaneSlice: 0,
                    },
                },
            };
            host.device.CreateUnorderedAccessView(
                written_host,
                None::<&ID3D12Resource>,
                Some(&light),
                slot(0),
            );
            let dst = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: 0,
                        NumElements: TEXELS as u32,
                        StructureByteStride: 0,
                        CounterOffsetInBytes: 0,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    },
                },
            };
            host.device.CreateUnorderedAccessView(
                &out,
                None::<&ID3D12Resource>,
                Some(&dst),
                slot(1),
            );
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT,
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
            };
            host.device
                .CreateShaderResourceView(placed_host, Some(&view), slot(2));
            let table = heap.GetGPUDescriptorHandleForHeapStart();
            (heap, table)
        };

        let list = host.new_list().expect("host list");
        crate::list::ensure_state(&list, written, UNORDERED_ACCESS);
        // SAFETY: `list` is open; the pipelines, heap and textures are live
        // and the textures are in the states the barriers above name.
        unsafe {
            let l = &list.list;
            l.SetComputeRootSignature(&root);
            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetComputeRootDescriptorTable(0, table);
            l.SetPipelineState(&write_pipeline);
            l.Dispatch(1, 1, 1);
        }
        // What a draw does for a view it samples.
        let aliased = crate::texture::alias(&list, placed, written, || {
            panic!("same-width copy issued a compute dispatch")
        });
        assert!(aliased, "the written texture is copied into the view's");
        crate::list::ensure_state(&list, placed, SHADER_RESOURCE);
        // SAFETY: as above; the view's texture is now readable.
        unsafe {
            let l = &list.list;
            l.SetPipelineState(&read_pipeline);
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[crate::host::transition(
                &out,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            l.CopyBufferRegion(&readback, 0, &out, 0, (TEXELS * 4) as u64);
        }
        list.close();
        host.execute(&[&list]);

        let mut got = [0u32; TEXELS];
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the submission completed and the readback holds the texels.
        unsafe {
            let range = D3D12_RANGE {
                Begin: 0,
                End: TEXELS * 4,
            };
            readback.Map(0, Some(&range), Some(&mut mapped)).unwrap();
            std::ptr::copy_nonoverlapping(mapped.cast::<u32>(), got.as_mut_ptr(), TEXELS);
            readback.Unmap(0, None);
        }
        let expected: [u32; TEXELS] =
            std::array::from_fn(|i| 0x3F00_0000 | ((i as u32 + 1) * 0x1357));
        assert_eq!(got, expected);
        list.reset();
    }

    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

    #[test]
    fn detile_widened_packed16_expands_into_rgba8_staging() {
        for format in crate::resource::PACKED_16 {
            let desc = TextureDesc {
                dimension: Dimension::Tex2d,
                width: 17,
                height: 9,
                depth_or_array: 1,
                mips: 2,
                format,
                samples: 1,
                swizzle: Swizzle::SW_4KB_S,
            };
            let tex = Texture::new(&desc).unwrap();
            let layout = &tex.planes[0].layout;
            let mut tiled = vec![0u8; layout.size as usize];
            let mut expected_mips = Vec::new();

            for mip in 0..2u32 {
                let m = layout.mips[mip as usize];
                let row_bytes = m.width as usize * 2;
                let mut src_mip = vec![0u8; row_bytes * m.height as usize];
                for y in 0..m.height as usize {
                    for x in 0..m.width as usize {
                        let px = ((mip as u16 + 1) * 0x1111)
                            .wrapping_add((y as u16) * 0x0103 + x as u16);
                        let at = y * row_bytes + x * 2;
                        src_mip[at..at + 2].copy_from_slice(&px.to_le_bytes());
                    }
                }
                layout
                    .tile(&mut tiled, mip, 0, &src_mip, row_bytes, src_mip.len())
                    .unwrap();
                expected_mips.push(src_mip);
            }

            let mut packed = Vec::new();
            for (mip, src_mip) in expected_mips.iter().enumerate() {
                let m = layout.mips[mip];
                let width = m.width as usize;
                let rows = m.height as usize;
                let row_pitch = (width * 4).next_multiple_of(256);
                let exact_len = row_pitch * (rows - 1) + width * 4;
                let mut staging = vec![0x5au8; exact_len];
                detile_subresource(
                    layout,
                    &tiled,
                    mip as u32,
                    0,
                    &mut staging,
                    row_pitch,
                    rows,
                    1,
                    Some(format),
                    &mut packed,
                )
                .unwrap();
                for y in 0..rows {
                    let mut expected = vec![0u8; width * 4];
                    crate::resource::expand_packed16(
                        format,
                        &src_mip[y * width * 2..][..width * 2],
                        &mut expected,
                    );
                    assert_eq!(
                        &staging[y * row_pitch..][..width * 4],
                        &expected[..],
                        "format {format} mip {mip} row {y}"
                    );
                }
            }
        }
    }
}
