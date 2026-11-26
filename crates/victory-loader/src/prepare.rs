//! Turns a title's image into something the Windows loader maps but does
//! not run. Pure byte manipulation, so it is testable on any host.

use victory_pe::layout::{self, dir, dll_flags, file_flags};
use victory_pe::{ImageInfo, PeError};

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Pe(#[from] PeError),
    #[error("cannot patch image: {0}")]
    Patch(&'static str),
}

/// A patched copy of an image plus everything we need from the original.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Parsed from the *original* bytes: imports, TLS callbacks and entry
    /// point as the title expects them.
    pub info: ImageInfo,
    /// The patched bytes to hand to the Windows loader.
    pub bytes: Vec<u8>,
    /// BLAKE3 of the original bytes: identifies the exact build.
    pub original_hash: blake3::Hash,
}

impl PreparedImage {
    /// File name for the patched copy, unique per original build.
    pub fn file_name(&self) -> String {
        format!("victory-image-{}.dll", &self.original_hash.to_hex()[..16])
    }
}

/// Produces the patched copy. The changes are:
///
/// * `IMAGE_FILE_DLL` set, entry point cleared: the OS maps it like a DLL
///   and calls nothing;
/// * import directory points to its empty terminator and the bound-import
///   directory is cleared: we bind the original slots using `info.imports`;
/// * TLS callback pointer cleared: the OS still allocates static TLS for
///   the image, but callbacks only run when we call them (after binding);
///   every base relocation that writes into it is made a no-op so the 0
///   survives a rebase;
/// * `FORCE_INTEGRITY` cleared, checksum zeroed: the copy is unsigned.
pub fn prepare(original: &[u8]) -> Result<PreparedImage, PrepareError> {
    let info = ImageInfo::parse(original)?;
    let mut bytes = original.to_vec();

    let fh = layout::file_header_offset(&bytes).ok_or(PrepareError::Patch("no file header"))?;
    let opt =
        layout::optional_header_offset(&bytes).ok_or(PrepareError::Patch("no optional header"))?;

    let characteristics = info.characteristics | file_flags::DLL;
    write_u16(
        &mut bytes,
        fh + layout::FILE_HEADER_CHARACTERISTICS,
        characteristics,
    )?;
    write_u32(&mut bytes, opt + layout::OPT_ADDRESS_OF_ENTRY_POINT, 0)?;
    write_u32(&mut bytes, opt + layout::OPT_CHECKSUM, 0)?;
    let dll_characteristics = info.dll_characteristics & !dll_flags::FORCE_INTEGRITY;
    write_u16(
        &mut bytes,
        opt + layout::OPT_DLL_CHARACTERISTICS,
        dll_characteristics,
    )?;

    // Windows 10 assigns static TLS to the prepared DLL when its empty
    // import table is present. The parser has already walked one descriptor
    // per module up to the null terminator, regardless of the recorded Size.
    if let Some(at) = layout::data_directory_offset(&bytes, dir::IMPORT) {
        let rva = layout::read_u32(&bytes, at).ok_or(PrepareError::Patch("no import RVA"))?;
        if rva != 0 {
            let terminator = u32::try_from(info.imports.len())
                .ok()
                .and_then(|count| count.checked_mul(IMPORT_DESCRIPTOR_SIZE))
                .and_then(|offset| rva.checked_add(offset))
                .ok_or(PrepareError::Patch("import terminator RVA overflow"))?;
            write_u32(&mut bytes, at, terminator)?;
            write_u32(&mut bytes, at + 4, IMPORT_DESCRIPTOR_SIZE)?;
        }
    }
    if let Some(at) = layout::data_directory_offset(&bytes, dir::BOUND_IMPORT) {
        write(&mut bytes, at, &[0; layout::DATA_DIRECTORY_SIZE])?;
    }

    if let Some(tls) = &info.tls
        && tls.callbacks_va != 0
    {
        let at = info
            .rva_to_file_offset(tls.directory_rva)
            .ok_or(PrepareError::Patch("TLS directory is not file-backed"))?;
        write_u64(
            &mut bytes,
            at as usize + layout::TLS64_ADDRESS_OF_CALLBACKS,
            0,
        )?;
        // A base relocation that writes into the field would turn the 0
        // into (part of) `delta` whenever the loader maps the copy away from
        // its preferred base (Wine always does for DYNAMIC_BASE images), and
        // the loader then reads callbacks from there. That is not only the
        // field's own DIR64 entry: a HIGHLOW, HIGH, LOW or HIGHADJ entry, or
        // a DIR64 entry a few bytes before it, patches some of its bytes
        // too. Make every such entry a no-op.
        let field_rva = tls
            .directory_rva
            .checked_add(layout::TLS64_ADDRESS_OF_CALLBACKS as u32)
            .ok_or(PrepareError::Patch("TLS callback field RVA overflow"))?;
        for reloc in relocations(&bytes, &info) {
            if reloc.overlaps(field_rva, 8) {
                for slot in 0..reloc.slots {
                    write_u16(&mut bytes, reloc.entry + 2 * slot, 0)?; // ABSOLUTE
                }
            }
        }
        // The parser reads the directory through `object`, which skips a
        // section with no virtual size; `rva_to_file_offset` maps that
        // section by its raw size. A damaged section table can make the two
        // disagree, and then the write above lands somewhere else while the
        // callbacks stay armed. The copy must never run them on its own.
        if ImageInfo::parse(&bytes)?
            .tls
            .is_some_and(|tls| tls.callbacks_va != 0)
        {
            return Err(PrepareError::Patch(
                "TLS callback pointer is not where the image was parsed from",
            ));
        }
    }

    Ok(PreparedImage {
        info,
        bytes,
        original_hash: blake3::hash(original),
    })
}

/// Size of an IMAGE_IMPORT_DESCRIPTOR in the PE file.
const IMPORT_DESCRIPTOR_SIZE: u32 = 20;

/// One base relocation entry that patches something.
#[derive(Debug, PartialEq, Eq)]
struct Relocation {
    /// File offset of the entry.
    entry: usize,
    /// 16-bit slots it occupies: two for HIGHADJ, whose second slot holds
    /// the low half of the adjusted value rather than an entry of its own.
    slots: usize,
    /// RVA it patches.
    target: u32,
    /// Bytes it writes at `target`. A type this table does not know is
    /// taken to write a whole pointer.
    width: u32,
}

impl Relocation {
    fn overlaps(&self, rva: u32, len: u32) -> bool {
        let (start, end) = (
            u64::from(self.target),
            u64::from(self.target) + u64::from(self.width),
        );
        start < u64::from(rva) + u64::from(len) && u64::from(rva) < end
    }
}

/// Every base relocation that patches something (all but ABSOLUTE
/// padding). Empty if the image has no readable relocation table.
fn relocations(bytes: &[u8], info: &ImageInfo) -> Vec<Relocation> {
    let Some(d) = layout::data_directory_offset(bytes, dir::BASERELOC) else {
        return Vec::new();
    };
    let (Some(rva), Some(size)) = (layout::read_u32(bytes, d), layout::read_u32(bytes, d + 4))
    else {
        return Vec::new();
    };
    if size == 0 {
        return Vec::new();
    }
    if let Some(start) = info.rva_to_file_offset(rva) {
        let start = start as usize;
        return relocation_entries(bytes, start, start.saturating_add(size as usize));
    }
    // The loaders read the table wherever the directory points in the
    // mapped image, and that includes the headers, which are mapped at the
    // RVA equal to their file offset. A table there that went unread would
    // leave the callback field's relocation armed.
    match header_bytes(bytes) {
        Some(headers) if (rva as usize) < headers => {
            let start = rva as usize;
            relocation_entries(
                bytes,
                start,
                start.saturating_add(size as usize).min(headers),
            )
        }
        _ => Vec::new(),
    }
}

/// How many bytes at the start of the file are mapped as the headers:
/// `SizeOfHeaders`, but never more than the file holds.
fn header_bytes(bytes: &[u8]) -> Option<usize> {
    let opt = layout::optional_header_offset(bytes)?;
    let size = layout::read_u32(bytes, opt.checked_add(layout::OPT_SIZE_OF_HEADERS)?)?;
    Some((size as usize).min(bytes.len()))
}

/// The entries of the relocation blocks in `bytes[start..end]`, ABSOLUTE
/// padding left out. An entry whose target does not fit in an RVA (a
/// damaged block near the top of the address space) patches nothing the
/// image could hold, so it is left out rather than wrapped around to the
/// RVA of some other field.
fn relocation_entries(bytes: &[u8], start: usize, end: usize) -> Vec<Relocation> {
    const ABSOLUTE: u16 = 0;
    const HIGH: u16 = 1;
    const LOW: u16 = 2;
    const HIGHLOW: u16 = 3;
    const HIGHADJ: u16 = 4;
    const DIR64: u16 = 10;
    let mut out = Vec::new();
    let mut at = start;
    while at + 8 <= end {
        let (Some(page), Some(block)) =
            (layout::read_u32(bytes, at), layout::read_u32(bytes, at + 4))
        else {
            break;
        };
        let block = block as usize;
        if block < 8 || at + block > end {
            break;
        }
        let mut entry = at + 8;
        while entry + 2 <= at + block {
            let Some(e) = bytes.get(entry..entry + 2) else {
                break;
            };
            let e = u16::from_le_bytes([e[0], e[1]]);
            let (kind, offset) = (e >> 12, u32::from(e & 0xFFF));
            let (width, slots) = match kind {
                ABSOLUTE => (0, 1),
                HIGH | LOW => (2, 1),
                HIGHADJ => (2, 2),
                HIGHLOW => (4, 1),
                DIR64 => (8, 1),
                _ => (8, 1),
            };
            if kind != ABSOLUTE
                && let Some(target) = page.checked_add(offset)
            {
                // A HIGHADJ whose parameter slot is past the block still
                // has its own slot cleared, and only that one.
                let slots = slots.min((at + block - entry) / 2);
                out.push(Relocation {
                    entry,
                    slots,
                    target,
                    width,
                });
            }
            entry += 2 * slots;
        }
        at += block;
    }
    out
}

fn write_u16(bytes: &mut [u8], at: usize, v: u16) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u32(bytes: &mut [u8], at: usize, v: u32) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u64(bytes: &mut [u8], at: usize, v: u64) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write(bytes: &mut [u8], at: usize, v: &[u8]) -> Result<(), PrepareError> {
    bytes
        .get_mut(at..at + v.len())
        .ok_or(PrepareError::Patch("header field outside of file"))?
        .copy_from_slice(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tests on the test binary itself need it to be a PE32+ image, which
    // it is only when built for Windows; `tests/synthetic_prepare.rs` covers
    // the same ground on any host.
    #[cfg(windows)]
    #[test]
    fn patches_own_test_binary() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let prepared = prepare(&original).unwrap();
        assert!(!prepared.info.is_dll(), "info describes the original");
        assert!(prepared.info.import_count() > 0);

        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        assert!(patched.is_dll());
        assert_eq!(patched.entry_rva, 0);
        assert!(patched.imports.is_empty());
        assert_eq!(patched.dll_characteristics & dll_flags::FORCE_INTEGRITY, 0);
        if let Some(tls) = &patched.tls {
            assert_eq!(tls.callbacks_va, 0);
            let field = tls.directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
            assert!(
                relocations(&prepared.bytes, &patched)
                    .iter()
                    .all(|r| !r.overlaps(field, 8)),
                "the cleared callback pointer must not be relocated"
            );
        }
        // Nothing else moved: same sections, same size.
        assert_eq!(patched.sections.len(), prepared.info.sections.len());
        assert_eq!(prepared.bytes.len(), original.len());
    }

    #[cfg(windows)]
    #[test]
    fn keeps_iat_slots_when_import_size_excludes_the_terminator() {
        let mut original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let info = ImageInfo::parse(&original).unwrap();
        let directory = layout::data_directory_offset(&original, dir::IMPORT).unwrap();
        // This linker layout counts the descriptors for real modules only.
        let descriptor_bytes = u32::try_from(info.imports.len()).unwrap() * IMPORT_DESCRIPTOR_SIZE;
        write_u32(&mut original, directory + 4, descriptor_bytes).unwrap();
        let prepared = prepare(&original).unwrap();
        let empty = layout::read_u32(&prepared.bytes, directory).unwrap();
        assert_eq!(
            empty,
            layout::read_u32(&original, directory).unwrap() + descriptor_bytes
        );
        assert_eq!(
            layout::read_u32(&prepared.bytes, directory + 4),
            Some(IMPORT_DESCRIPTOR_SIZE)
        );
        let at = info.rva_to_file_offset(empty).unwrap() as usize;
        assert_eq!(
            &prepared.bytes[at..at + IMPORT_DESCRIPTOR_SIZE as usize],
            &[0; 20]
        );
        let iat = layout::data_directory_offset(&original, dir::IAT).unwrap();
        assert_eq!(
            &prepared.bytes[iat..iat + layout::DATA_DIRECTORY_SIZE],
            &original[iat..iat + layout::DATA_DIRECTORY_SIZE]
        );
        assert_eq!(prepared.info.import_count(), info.import_count());
        for module in &prepared.info.imports {
            for entry in &module.entries {
                let at = info.rva_to_file_offset(entry.iat_slot_rva).unwrap() as usize;
                assert_eq!(&prepared.bytes[at..at + 8], &original[at..at + 8]);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_assigns_static_tls_to_a_prepared_dll() {
        // Model a title DLL using our own linked image. Loading a DLL leaves
        // its assigned index intact; the executable-specific redirect resets it.
        let mut original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let flags =
            layout::file_header_offset(&original).unwrap() + layout::FILE_HEADER_CHARACTERISTICS;
        let characteristics =
            ImageInfo::parse(&original).unwrap().characteristics | file_flags::DLL;
        write_u16(&mut original, flags, characteristics).unwrap();
        let prepared = prepare(&original).unwrap();
        let info = &prepared.info;
        let tls = info.tls.as_ref().expect("the test binary uses static TLS");
        let directory = info.rva_to_file_offset(tls.directory_rva).unwrap() as usize;
        let index_va = layout::read_u64(&original, directory + 16).unwrap();
        let index_rva = index_va.checked_sub(info.image_base).unwrap();
        assert!(index_rva + 4 <= u64::from(info.size_of_image));

        let folder = tempfile::tempdir().unwrap();
        let image = crate::load(&prepared, folder.path()).unwrap();
        // SAFETY: the mapped test image contains the checked DWORD above.
        let assigned =
            unsafe { ((image.base() + index_rva as usize) as *const u32).read_unaligned() };
        // SAFETY: the prepared DLL has never been attached or given to a caller.
        unsafe { image.unload() };
        assert_ne!(
            assigned, 0,
            "the process executable already owns TLS slot zero"
        );
    }

    /// One relocation block: page RVA, then its 16-bit entries.
    fn block(page: u32, entries: &[u16]) -> Vec<u8> {
        let mut b = page.to_le_bytes().to_vec();
        b.extend_from_slice(&(8 + 2 * entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend_from_slice(&e.to_le_bytes());
        }
        b
    }

    /// (entry, slots, target, width) of each relocation, for comparing.
    fn entries(bytes: &[u8], end: usize) -> Vec<(usize, usize, u32, u32)> {
        relocation_entries(bytes, 0, end)
            .into_iter()
            .map(|r| (r.entry, r.slots, r.target, r.width))
            .collect()
    }

    #[test]
    fn relocation_entries_walk_blocks_and_skip_padding() {
        // DIR64 at +0x10, an ABSOLUTE pad, a HIGHLOW at +0x20, a DIR64.
        let mut bytes = block(0x1000, &[0xA010, 0x0000, 0x3020, 0xA018]);
        bytes.extend(block(0x2000, &[0xA000, 0x0000]));
        assert_eq!(
            entries(&bytes, bytes.len()),
            [
                (8, 1, 0x1010, 8),
                (12, 1, 0x1020, 4),
                (14, 1, 0x1018, 8),
                (24, 1, 0x2000, 8)
            ]
        );
        // A block that claims more than the directory holds ends the walk.
        let mut bytes = block(0x1000, &[0xA010, 0x0000]);
        bytes[4] = 0x40;
        assert!(relocation_entries(&bytes, 0, bytes.len()).is_empty());
    }

    #[test]
    fn relocation_entries_know_each_width_and_the_highadj_parameter() {
        // HIGH, LOW, HIGHADJ (its next slot is a parameter, here 0xA040,
        // which must not be read as a DIR64 entry), then a type x64 loaders
        // do not define, taken as pointer-wide.
        let bytes = block(0x1000, &[0x1002, 0x2004, 0x4006, 0xA040, 0x5008, 0x0000]);
        assert_eq!(
            entries(&bytes, bytes.len()),
            [
                (8, 1, 0x1002, 2),
                (10, 1, 0x1004, 2),
                (12, 2, 0x1006, 2),
                (16, 1, 0x1008, 8)
            ]
        );
        // A HIGHADJ in the last slot of its block owns only that slot.
        let bytes = block(0x1000, &[0xA010, 0x4006]);
        assert_eq!(
            entries(&bytes, bytes.len()),
            [(8, 1, 0x1010, 8), (10, 1, 0x1006, 2)]
        );
    }

    #[test]
    fn a_relocation_overlaps_the_bytes_it_writes() {
        let at = |target, width| Relocation {
            entry: 0,
            slots: 1,
            target,
            width,
        };
        // An 8-byte field at 0x1118.
        assert!(at(0x1118, 8).overlaps(0x1118, 8));
        assert!(at(0x1118, 4).overlaps(0x1118, 8));
        assert!(at(0x111C, 4).overlaps(0x1118, 8), "upper half");
        assert!(at(0x1110, 9).overlaps(0x1118, 8));
        assert!(at(0x111F, 2).overlaps(0x1118, 8));
        assert!(!at(0x1110, 8).overlaps(0x1118, 8), "ends where it starts");
        assert!(!at(0x1120, 8).overlaps(0x1118, 8), "starts where it ends");
        assert!(at(0xFFFF_FFFC, 8).overlaps(0xFFFF_FFF8, 8), "no wrap");
    }

    #[test]
    fn relocation_entries_drop_targets_past_the_top_of_the_address_space() {
        // A damaged page RVA near u32::MAX: page + offset does not fit, and
        // wrapping it would name a low RVA (0x0 for 0xFFFF_FFF0 + 0x10) that
        // belongs to some other field. Entries whose target fits are kept.
        let bytes = block(0xFFFF_F000, &[0xA000, 0xAFFF, 0x0000]);
        assert_eq!(
            entries(&bytes, bytes.len()),
            [(8, 1, 0xFFFF_F000, 8), (10, 1, 0xFFFF_FFFF, 8)]
        );
        let bytes = block(0xFFFF_FFF0, &[0xA00F, 0xA010, 0xAFFF]);
        assert_eq!(entries(&bytes, bytes.len()), [(8, 1, 0xFFFF_FFFF, 8)]);
        // An end past the buffer stops at the bytes there are.
        assert_eq!(relocation_entries(&bytes, 0, usize::MAX).len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn file_name_is_stable_per_build() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let a = prepare(&original).unwrap();
        let b = prepare(&original).unwrap();
        assert_eq!(a.file_name(), b.file_name());
    }
}
