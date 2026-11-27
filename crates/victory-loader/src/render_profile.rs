//! The optional render settings for one exact title build. Only short
//! interoperability signatures are used; other title builds are untouched.

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, VirtualProtect, VirtualQuery,
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

/// Override the render scale and/or the title's frame-rate setting.
///
/// # Safety
/// The image at `base` must be the mapped copy of `hash`. Imports have been
/// bound and no title instructions may execute until after this returns.
pub(crate) unsafe fn apply(
    base: usize,
    size: usize,
    hash: blake3::Hash,
    scale: Option<f32>,
    fps: Option<u32>,
) -> Result<()> {
    ensure!(
        hash.to_hex().as_str() == BUILD_BLAKE3,
        "render overrides are only supported for the Series 1.0.2.1 executable"
    );
    ensure!(
        scale.is_none_or(|scale| (0.5..=2.0 / 3.0).contains(&scale)),
        "render scale must be between one half and the stock two thirds"
    );
    ensure!(
        fps.is_none_or(|fps| fps <= 1000),
        "title frame rate exceeds 1000"
    );

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
    // SAFETY: all patches are for the mapped build and it is not running.
    unsafe { install(base, size, [scale, first, second, third]) }
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
        ensure!(
            rva.checked_add(patch.signature.len())
                .is_some_and(|end| end <= size)
                && patch
                    .offset
                    .checked_add(patch.replacement.len())
                    .is_some_and(|end| end <= patch.signature.len()),
            "render profile site {rva:#x} is outside the image or the replacement is too long"
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
                    PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
                )
                && at
                    .checked_add(patch.signature.len())
                    .is_some_and(|end| { mapped_end.is_some_and(|mapped_end| end <= mapped_end) }),
            "render profile site {rva:#x} is not in readable image code"
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
