//! Raw read aliases and shader-visible lengths for buffer SRVs whose slot a
//! shader selects on the GPU. The title's descriptors stay at their original
//! slots with their original type. Aliases live in the host heap's spare slots.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::shadow::Shadow;

use super::{BufferRead, HostHeap, ID3D12Resource, host};
use crate::descriptor::Heap;

/// Writes to the title heap only mark records here. Rewriting a live alias
/// while the GPU can still be reading the previous submission is unsafe.
#[derive(Default)]
struct Changes {
    slots: HashSet<u32>,
    released: bool,
}

pub(super) struct Projection {
    changes: Mutex<Changes>,
    table: Mutex<BufferTable>,
    buffer: ID3D12Resource,
}

impl Projection {
    pub(super) fn new(heap: &HostHeap) -> Option<Self> {
        let table = BufferTable::new(heap)?;
        let buffer = table.buffer.clone();
        Some(Self {
            changes: Mutex::default(),
            table: Mutex::new(table),
            buffer,
        })
    }

    pub(super) fn buffer(&self) -> &ID3D12Resource {
        &self.buffer
    }

    pub(super) fn visit_reads(&self, mut visit: impl FnMut(&Arc<Shadow>, usize, usize)) {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        for range in table.refreshes.get_or_init(|| table.refresh_ranges()) {
            visit(&range.shadow, range.start, range.end - range.start);
        }
    }

    pub(super) fn changed(&self, slot: u32) {
        self.changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .insert(slot);
    }

    pub(super) fn released(&self) {
        self.changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .released = true;
    }

    /// Consume writes separately from resolving them: resolving a view pins
    /// its resource, and dropping the pin can re-enter the release hook.
    pub(super) fn sync(&self, heap: &HostHeap, title: &Heap) -> bool {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut changes =
            std::mem::take(&mut *self.changes.lock().unwrap_or_else(|e| e.into_inner()));
        if !table.initialized {
            let made = heap.made.lock().unwrap_or_else(|e| e.into_inner());
            changes.slots.extend(
                made.iter()
                    .enumerate()
                    .filter_map(|(i, &key)| (key != 0).then_some(i as u32)),
            );
            table.initialized = true;
        }
        if changes.released {
            changes.slots.extend(table.reads.keys().copied());
        }
        let mut complete = true;
        for slot in changes.slots {
            if !table.update(heap, title, slot) {
                self.changed(slot);
                complete = false;
            }
        }
        complete
    }
}

/// Views of a backing can share its raw descriptor. The per-slot metadata
/// still checks the logical view length and preserves its own stride.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RawRange {
    backing: usize,
    offset: u64,
    bytes: u64,
}

impl RawRange {
    fn from_view(read: &BufferRead) -> (Self, u32) {
        let offset = read.shadow.offset(read.address);
        let total = offset + read.shadow.left(read.address);
        let (start, bytes) = if total <= u64::from(u32::MAX - 15) {
            (0, total)
        } else {
            // Keep the shader's final 16-byte sentinel outside the alias.
            (offset & !15, read.bytes + offset % 16)
        };
        (
            Self {
                backing: Arc::as_ptr(&read.shadow) as usize,
                offset: start,
                bytes,
            },
            (offset - start) as u32,
        )
    }
}

struct RefreshRange {
    shadow: Arc<Shadow>,
    start: usize,
    end: usize,
}

pub(super) struct BufferTable {
    refreshes: OnceLock<Vec<RefreshRange>>,
    pub(super) buffer: ID3D12Resource,
    mapped: usize,
    count: u32,
    initialized: bool,
    null: u32,
    reads: HashMap<u32, (RawRange, BufferRead)>,
    aliases: HashMap<RawRange, (u32, u32)>,
}

impl BufferTable {
    pub(super) fn new(heap: &HostHeap) -> Option<Self> {
        let count = heap.count();
        let device = host()?;
        let null = heap.dynamic(1)?;
        let Some((buffer, mapped)) = device.upload_buffer((u64::from(count) + 2) * 16) else {
            heap.release(null, 1);
            return None;
        };
        heap.null_raw_at(null);
        let table = Self {
            refreshes: OnceLock::new(),
            buffer,
            mapped: mapped as usize,
            count,
            initialized: false,
            null,
            reads: HashMap::new(),
            aliases: HashMap::new(),
        };
        table.write(0, [count, 0, 0, 0]);
        // Holes and indices beyond the host heap both read a real null SRV.
        for i in 1..count + 2 {
            table.write(i, [null, 0, 0, 0]);
        }
        Some(table)
    }

    // Batch::add refreshes one enclosing range per backing. Many adjacent
    // descriptors select the same backing, so cache those exact unions until
    // a view changes the set of bytes that can be read.
    fn refresh_ranges(&self) -> Vec<RefreshRange> {
        let mut ranges = HashMap::new();
        for (_, read) in self.reads.values() {
            let start = read.address as usize;
            let end = start + read.bytes as usize;
            let range = ranges
                .entry(Arc::as_ptr(&read.shadow))
                .or_insert_with(|| RefreshRange {
                    shadow: read.shadow.clone(),
                    start,
                    end,
                });
            range.start = range.start.min(start);
            range.end = range.end.max(end);
        }
        ranges.into_values().collect()
    }

    fn write(&self, record: u32, data: [u32; 4]) {
        assert!(record < self.count + 2);
        // SAFETY: the live resource owns a mapping of (count+2) aligned
        // records. Initialization or submission owns writes while the GPU is idle.
        unsafe {
            (self.mapped as *mut [u32; 4])
                .add(record as usize)
                .write(data)
        }
    }

    /// Called with the host submission serialized, before executing its
    /// lists. All draws referencing this table see the slot at submission.
    pub(super) fn update(&mut self, heap: &HostHeap, title: &Heap, slot: u32) -> bool {
        if slot >= self.count {
            return false;
        }
        let next = title.selected_buffer(slot);
        if let (Some((_, old)), Some(read)) = (self.reads.get(&slot), &next)
            && Arc::ptr_eq(&old.shadow, &read.shadow)
            && (old.address, old.bytes, old.stride) == (read.address, read.bytes, read.stride)
        {
            return true;
        }
        let next_key = next.as_ref().map(RawRange::from_view);
        // A stride, offset or length change within the same raw range only
        // changes metadata. Keep the alias even when this is its sole reader.
        if let (Some((old, previous)), Some((key, bias))) = (self.reads.get(&slot), next_key)
            && *old == key
            && let Some(read) = next
        {
            if (previous.address, previous.bytes) != (read.address, read.bytes) {
                self.refreshes.take();
            }
            self.write_view(slot, self.aliases[&key].0, bias, &read);
            self.reads.insert(slot, (key, read));
            return true;
        }
        if let Some((old, _)) = self.reads.remove(&slot) {
            self.refreshes.take();
            let (index, refs) = self.aliases.get_mut(&old).expect("live buffer alias");
            *refs -= 1;
            if *refs == 0 {
                heap.null_raw_at(*index);
                heap.release(*index, 1);
                self.aliases.remove(&old);
            }
        }
        self.write(slot + 2, [self.null, 0, 0, 0]);
        let (Some(read), Some((key, bias))) = (next, next_key) else {
            return true;
        };
        let alias = match self.aliases.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let (index, refs) = entry.get_mut();
                *refs += 1;
                *index
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Some(index) = heap.dynamic(1) else {
                    return false;
                };
                heap.raw_at(index, &read.shadow.buffer, key.offset, key.bytes);
                entry.insert((index, 1));
                index
            }
        };
        self.refreshes.take();
        self.write_view(slot, alias, bias, &read);
        self.reads.insert(slot, (key, read));
        true
    }

    fn write_view(&self, slot: u32, alias: u32, bias: u32, read: &BufferRead) {
        self.write(
            slot + 2,
            [
                alias,
                read.stride,
                bias,
                (read.bytes / u64::from(read.stride)) as u32,
            ],
        );
    }
}
