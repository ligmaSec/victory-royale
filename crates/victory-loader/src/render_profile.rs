//! The optional render settings for one exact title build. Only short
//! interoperability signatures are used; other title builds are untouched.

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ,
    PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const BUILD_BLAKE3: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";

#[derive(Clone, Copy)]
struct Patch<'a> {
    rva: usize,
    signature: &'a [u8],
    offset: usize,
    replacement: &'a [u8],
}

const OUTPUT_RVA: usize = 0x224352b;
const OUTPUT_LEN: usize = 0x2243568 - OUTPUT_RVA;
const NATIVE_RENDER_PAGES: u32 = 0x5400;
const GRAPHICS_HEADROOM: u64 = 1024 << 20;

/// Generate the requested settings writes, then invoke the title's existing
/// size setter and height-change notification. No executable block is stored.
fn output_code(width: u32, height: u32) -> [u8; OUTPUT_LEN] {
    fn rel(target: usize, end: usize) -> [u8; 4] {
        ((target as i64 - (OUTPUT_RVA + end) as i64) as i32).to_le_bytes()
    }
    let mut code = [0xcc; OUTPUT_LEN];
    code[0] = 0xb8; // mov eax, width
    code[1..5].copy_from_slice(&width.to_le_bytes());
    code[5..11].copy_from_slice(&[0x89, 0x86, 4, 2, 0, 0]); // save width
    code[11] = 0xbf; // mov edi, height
    code[12..16].copy_from_slice(&height.to_le_bytes());
    code[16..22].copy_from_slice(&[0x89, 0xbe, 8, 2, 0, 0]); // save height
    code[22..24].copy_from_slice(&[0x8b, 0x2d]); // get previous screen height
    code[24..28].copy_from_slice(&rel(0xa9438d0, 28));
    code[28..33].copy_from_slice(&[0x89, 0xc1, 0x89, 0xfa, 0xe8]); // set size
    code[33..37].copy_from_slice(&rel(0x2478130, 37));
    code[37..44].copy_from_slice(&[0x39, 0xfd, 0x74, (OUTPUT_LEN - 41) as u8, 0xb1, 1, 0xe8]);
    code[44..48].copy_from_slice(&rel(0xd8c3a0, 48)); // notify on changed height
    code[48..50].copy_from_slice(&[0xeb, (OUTPUT_LEN - 50) as u8]);
    code
}

fn patch<'a>(rva: usize, signature: &'a [u8], offset: usize, replacement: &'a [u8]) -> Patch<'a> {
    Patch {
        rva,
        signature,
        offset,
        replacement,
    }
}

/// Override the title's output, own upscaler scale, or frame-rate setting.
///
/// # Safety
/// The image at `base` must be the mapped copy of `hash`. Imports have been
/// bound and no title instructions may execute until after this returns.
pub(crate) unsafe fn apply(
    base: usize,
    size: usize,
    hash: blake3::Hash,
    output: Option<(u32, u32)>,
    scale: Option<f32>,
    fps: Option<u32>,
) -> Result<()> {
    ensure!(
        hash.to_hex().as_str() == BUILD_BLAKE3,
        "render overrides are only supported for the Series 1.0.2.1 executable"
    );
    ensure!(
        scale.is_none_or(|scale| (0.5..=1.0).contains(&scale)),
        "render scale must be between one half and one"
    );
    ensure!(
        fps.is_none_or(|fps| fps <= 1000),
        "title frame rate exceeds 1000"
    );
    const MODES: [(u32, u32); 5] = [
        (1280, 720),
        (1664, 936),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];
    ensure!(
        output.is_none_or(|mode| MODES.contains(&mode)),
        "unsupported output mode"
    );
    ensure!(
        scale.is_none_or(|scale| scale <= 2.0 / 3.0) || output.is_some(),
        "render scales above stock require an explicit output size"
    );
    let native = output == Some((3840, 2160)) && scale.is_some_and(|s| s > 2.0 / 3.0);
    let output_bytes = output.map_or([0; OUTPUT_LEN], |(w, h)| output_code(w, h));
    let output_site = output.map(|_| {
        patch(
            OUTPUT_RVA,
            &[0x8b, 0x0d, 0x9b, 0x03, 0x70, 0x08],
            0,
            &output_bytes,
        )
    });
    // The donor retains the stock back-buffer allocation. For smaller known
    // modes only, skip its assertion about surplus space, not a capacity check.
    let surplus = output.filter(|&mode| mode != (3840, 2160)).map(|_| {
        patch(
            0x247af25,
            &[0x48, 0x3d, 0xff, 0xff, 0x1f, 0, 0x76, 1, 0xcc],
            6,
            &[0xeb],
        )
    });

    let scale_bytes = scale.unwrap_or_default().to_le_bytes();
    let [a, b, c, d] = fps.unwrap_or_default().to_le_bytes();
    let load_fps = [0xb8, a, b, c, d, 0x90]; // Load the rate into EAX.
    let [a, b, c, d] = fps.filter(|&fps| fps != 0).unwrap_or(1000).to_le_bytes();
    let simulation_fps = [0xb8, a, b, c, d, 0xeb, 0x0d]; // Continue with the timer step conversion.
    let scale = scale.map(|_| Patch {
        rva: 0x28f1e78,
        signature: &[0xc7, 0x81, 0x90, 0, 0, 0, 0xab, 0xaa, 0x2a, 0x3f],
        offset: 6,
        replacement: &scale_bytes,
    });
    let fps = fps.map(|_| {
        [
            Patch {
                rva: 0x1e0fc62,
                signature: &[0x8b, 0x87, 0x10, 0x02, 0, 0],
                offset: 0,
                replacement: &load_fps,
            },
            Patch {
                rva: 0x224359a,
                signature: &[0x8b, 0x86, 0x10, 0x02, 0, 0],
                offset: 0,
                replacement: &load_fps,
            },
            Patch {
                rva: 0x2241fdb,
                signature: &[0x83, 0xb8, 0x10, 0x02, 0, 0, 0],
                offset: 0,
                replacement: &simulation_fps,
            },
        ]
    });
    let [first, second, third] = fps.map_or([None, None, None], |fps| fps.map(Some));
    if !native {
        // SAFETY: all sites are in the exact mapped build, before execution.
        return unsafe {
            install(
                base,
                size,
                [output_site, surplus, scale, first, second, third],
            )
        };
    }

    // Only the Series X reservation and its reported total grow. Each of the
    // four users of the old inline page array must move to the new table.
    const TOTAL: u64 = 13312 << 20;
    const GRAPHICS_HEAP: u64 = 2199 << 20;
    let original_total = TOTAL.to_le_bytes();
    let original_heap = GRAPHICS_HEAP.to_le_bytes();
    let total = (TOTAL + GRAPHICS_HEADROOM).to_le_bytes();
    let heap = (GRAPHICS_HEAP + GRAPHICS_HEADROOM).to_le_bytes();
    let pages = NATIVE_RENDER_PAGES.to_le_bytes();
    // SAFETY: private committed, zeroed memory for the title's page numbers.
    let table = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            NATIVE_RENDER_PAGES as usize * size_of::<usize>(),
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    };
    ensure!(
        !table.is_null(),
        "could not allocate the native 4K render-page table"
    );
    let pointer = (table as usize).to_le_bytes();
    let patches = [
        output_site,
        scale,
        first,
        second,
        third,
        Some(patch(0x61862a0, &original_total, 0, &total)),
        Some(patch(0x61862b8, &original_heap, 0, &heap)),
        Some(patch(0x1d488b1, &[0xbe, 0x60, 0x2c, 0, 0], 1, &pages)),
        Some(patch(0x75eaee0, &[0; 8], 0, &pointer)),
        Some(patch(
            0x2473fa1,
            &[0x4c, 0x8d, 0x1d, 0x38, 0x6f, 0x17, 5],
            1,
            &[0x8b],
        )),
        Some(patch(
            0x2474079,
            &[0x4c, 0x8d, 0x1d, 0x60, 0x6e, 0x17, 5],
            1,
            &[0x8b],
        )),
        Some(patch(
            0x24740ec,
            &[0x4c, 0x8d, 0x1d, 0xed, 0x6d, 0x17, 5],
            1,
            &[0x8b],
        )),
        Some(patch(
            0x5eb278d,
            &[0x4c, 0x8d, 5, 0x4c, 0x87, 0x73, 1],
            1,
            &[0x8b],
        )),
    ];
    // SAFETY: checks all sites and permissions before writing the profile.
    let applied = unsafe { install(base, size, patches) };
    if applied.is_err() {
        // SAFETY: the run aborts without entering any code from this image.
        unsafe { VirtualFree(table, 0, MEM_RELEASE) };
    } else {
        // The title owns the table until the process exits, as with its static array.
        tracing::info!(
            pool_mib = NATIVE_RENDER_PAGES / 16,
            heap_extra_mib = GRAPHICS_HEADROOM >> 20,
            "native render reservations installed"
        );
    }
    applied
}

#[derive(Clone, Copy)]
struct Protection {
    at: *const core::ffi::c_void,
    len: usize,
    previous: u32,
}

/// Protection of every site we changed, in reverse order in case multiple
/// sites belong to the same page. Also restores pages on an early failure.
struct Protections<const N: usize> {
    entries: [Option<Protection>; N],
}

impl<const N: usize> Protections<N> {
    fn restore(&mut self) -> Result<()> {
        let mut error = None;
        for entry in self.entries.iter_mut().rev().filter_map(Option::take) {
            let mut ignored = 0;
            // SAFETY: the page remains mapped and this is the saved protection.
            if unsafe { VirtualProtect(entry.at, entry.len, entry.previous, &mut ignored) } == 0
                && error.is_none()
            {
                error = Some(std::io::Error::last_os_error());
            }
        }
        match error {
            Some(error) => Err(error).context("restoring the render profile code protection"),
            None => Ok(()),
        }
    }
}

impl<const N: usize> Drop for Protections<N> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Check all signatures and acquire all permissions before writing anything.
///
/// # Safety
/// The image at `base` stays mapped and none of its code is executing.
unsafe fn install<const N: usize>(
    base: usize,
    size: usize,
    patches: [Option<Patch<'_>>; N],
) -> Result<()> {
    for patch in patches.iter().flatten() {
        let rva = patch.rva;
        let span = patch.signature.len().max(
            patch
                .offset
                .checked_add(patch.replacement.len())
                .context("render profile replacement overflow")?,
        );
        ensure!(
            rva.checked_add(span).is_some_and(|end| end <= size),
            "render profile site {rva:#x} is outside the image"
        );
        let at = base
            .checked_add(rva)
            .context("render profile address overflow")?;
        // SAFETY: zero is valid for this output structure.
        let mut page: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: queries page metadata without reading the image itself.
        let queried = unsafe { VirtualQuery(at as *const _, &mut page, size_of_val(&page)) };
        let mapped_end = (page.BaseAddress as usize).checked_add(page.RegionSize);
        ensure!(
            queried != 0
                && page.AllocationBase as usize == base
                && page.State == MEM_COMMIT
                && page.Protect & PAGE_GUARD == 0
                && matches!(
                    page.Protect & 0xff,
                    PAGE_EXECUTE_READ
                        | PAGE_EXECUTE_READWRITE
                        | PAGE_EXECUTE_WRITECOPY
                        | PAGE_READONLY
                        | PAGE_READWRITE
                        | PAGE_WRITECOPY
                )
                && at
                    .checked_add(span)
                    .is_some_and(|end| { mapped_end.is_some_and(|mapped_end| end <= mapped_end) }),
            "render profile site {rva:#x} is not in readable image memory"
        );
        // SAFETY: the whole signature is in a committed, readable region of the mapped image.
        let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, patch.signature.len()) };
        ensure!(
            bytes == patch.signature,
            "render profile signature differs at {rva:#x}"
        );
    }

    let mut protections = Protections { entries: [None; N] };
    for (patch, slot) in patches.iter().flatten().zip(&mut protections.entries) {
        let at = (base + patch.rva + patch.offset) as *const _;
        let len = patch.replacement.len();
        let mut previous = 0;
        // SAFETY: every site is in one checked code region; the title has not started.
        if unsafe { VirtualProtect(at, len, PAGE_READWRITE, &mut previous) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("making the render profile code writable");
        }
        *slot = Some(Protection { at, len, previous });
    }
    for patch in patches.iter().flatten() {
        let at = (base + patch.rva + patch.offset) as *mut u8;
        // SAFETY: each site was made writable above and the title is not running yet.
        unsafe {
            std::ptr::copy_nonoverlapping(patch.replacement.as_ptr(), at, patch.replacement.len());
        }
    }
    protections.restore()?;
    for patch in patches.iter().flatten() {
        let at = (base + patch.rva + patch.offset) as *const _;
        // SAFETY: the title still owns the mapped page.
        if unsafe { FlushInstructionCache(GetCurrentProcess(), at, patch.replacement.len()) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("flushing the render profile code cache");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree};

    #[test]
    fn native_output_code_keeps_both_existing_calls_and_the_continuation() {
        let code = output_code(3840, 2160);
        assert_eq!(u32::from_le_bytes(code[1..5].try_into().unwrap()), 3840);
        assert_eq!(u32::from_le_bytes(code[12..16].try_into().unwrap()), 2160);
        let target = |end| {
            let disp = i32::from_le_bytes(code[end - 4..end].try_into().unwrap());
            (OUTPUT_RVA + end).wrapping_add_signed(disp as isize)
        };
        assert_eq!(target(28), 0xa9438d0);
        assert_eq!(target(37), 0x2478130);
        assert_eq!(target(48), 0xd8c3a0);
        assert_eq!(41 + usize::from(code[40]), OUTPUT_LEN);
        assert_eq!(50 + usize::from(code[49]), OUTPUT_LEN);
    }

    #[test]
    fn no_setting_changes_if_any_signature_differs() {
        // SAFETY: owns this allocation until the explicit free; it is never executed.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            page.write(1);
            page.add(4).write(2);
            let mut previous = 0;
            assert_ne!(
                VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut previous),
                0
            );
            let first = Patch {
                rva: 0,
                signature: &[1],
                offset: 0,
                replacement: &[9],
            };
            let second = Patch {
                rva: 4,
                signature: &[3],
                offset: 0,
                replacement: &[8],
            };
            assert!(install(page as usize, 4096, [Some(first), Some(second)]).is_err());
            assert_eq!((page.read(), page.add(4).read()), (1, 2));
            let second = Patch {
                signature: &[2],
                ..second
            };
            assert!(install(page as usize, 4096, [Some(first), Some(second)]).is_ok());
            assert_eq!((page.read(), page.add(4).read()), (9, 8));
            let mut region = std::mem::zeroed();
            VirtualQuery(page.cast(), &mut region, size_of_val(&region));
            assert_eq!(region.Protect, PAGE_EXECUTE_READ);
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
