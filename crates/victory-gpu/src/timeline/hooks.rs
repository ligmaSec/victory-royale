//! The wrappers around what threads wait and wake each other with. The
//! title's imports are bound to them ([`import`]), and the entries of our
//! own import table point at them, if a timeline was asked for; else nothing
//! is wrapped. While nothing is recorded a wrapper is a load and a jump.

use std::ffi::{CStr, c_void};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::record::{kernel, on, standing, stood_at, ticks, waited, woken};

type Handle = *mut c_void;

macro_rules! wrapped {
    ($($index:literal $name:ident ($($arg:ident: $type:ty),*) $(-> $result:ty)? => |$call:ident| $body:expr;)*) => {
        /// The functions wrapped, by name: their own addresses, ours.
        static TABLE: [(&str, AtomicUsize, usize); [$($index),*].len()] = [
            $((stringify!($name), AtomicUsize::new(0), $index)),*
        ];

        fn wrapper(index: usize) -> usize {
            match index {
                $($index => wrappers::$name as *const () as usize,)*
                _ => 0,
            }
        }

        #[allow(non_snake_case)]
        mod wrappers {
            use super::*;
            $(
                pub(super) unsafe extern "system" fn $name($($arg: $type),*) $(-> $result)? {
                    let real: unsafe extern "system" fn($($type),*) $(-> $result)? =
                        // SAFETY: each index identifies the same name in TABLE and wrapper().
                        // arm() resolves the original exports once. Our import slots are
                        // patched only for populated entries; import() fills any unresolved
                        // slot from the title's resolved import before returning the wrapper.
                        // Only wrapped entries reach this code; their targets remain loaded
                        // and use the entry's declared system calling convention and signature.
                        unsafe { std::mem::transmute(TABLE[$index].1.load(Ordering::Relaxed)) };
                    let $call = || {
                        // SAFETY: entry through the original import requires its caller to
                        // satisfy the original function's argument contract. We forward unchanged.
                        unsafe { real($($arg),*) }
                    };
                    if !on() {
                        return $call();
                    }
                    $body
                }
            )*
        }
    };
}

/// Runs `call`, a wait on `object` that may be long; what the caller may
/// ask [`GetLastError`] about stays.
///
/// [`GetLastError`]: windows_sys::Win32::Foundation::GetLastError
fn wait<T>(name: &'static str, object: u64, call: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    let t0 = ticks();
    let result = {
        let _standing = standing(object);
        call()
    };
    // SAFETY: plain queries of this thread's own.
    unsafe {
        let error = GetLastError();
        waited(name, object, t0);
        SetLastError(error);
    }
    result
}

/// Runs `call`, a call of the kernel's on `bytes`.
fn measured<T>(name: &'static str, bytes: usize, call: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    let t0 = ticks();
    let result = call();
    // SAFETY: plain queries of this thread's own.
    unsafe {
        let error = GetLastError();
        kernel(name, bytes as u64, t0);
        SetLastError(error);
    }
    result
}

/// The function `name` of the table, as the system has it.
fn real(name: &str) -> usize {
    TABLE
        .iter()
        .find(|t| t.0 == name)
        .map_or(0, |t| t.1.load(Ordering::Relaxed))
}

/// `take(lock)`, a try that does not wait, then `wait`.
fn lock_of(name: &'static str, lock: Handle, take: &str, call: impl FnOnce()) {
    let take = real(take);
    if take != 0 {
        // SAFETY: `TryEnterCriticalSection` or a `TryAcquireSRWLock` call:
        // one pointer, a truth value (one byte of it for the latter).
        let taken = unsafe {
            std::mem::transmute::<usize, unsafe extern "system" fn(Handle) -> u8>(take)(lock)
        };
        if taken != 0 {
            return;
        }
    }
    wait(name, lock as u64, call);
}

fn released(name: &'static str, object: Handle) {
    if stood_at(object as u64) {
        woken(name, object as u64);
    }
}

wrapped! {
    0 WaitForSingleObject(object: Handle, ms: u32) -> u32 => |call| {
        wait("WaitForSingleObject", object as u64, call)
    };
    1 WaitForSingleObjectEx(object: Handle, ms: u32, alertable: i32) -> u32 => |call| {
        wait("WaitForSingleObjectEx", object as u64, call)
    };
    2 Sleep(ms: u32) => |call| {
        wait("Sleep", 0, call)
    };
    3 SleepEx(ms: u32, alertable: i32) -> u32 => |call| {
        wait("SleepEx", 0, call)
    };
    4 SetEvent(object: Handle) -> i32 => |call| {
        woken("SetEvent", object as u64);
        call()
    };
    5 ReleaseSemaphore(object: Handle, count: i32, before: *mut i32) -> i32 => |call| {
        woken("ReleaseSemaphore", object as u64);
        call()
    };
    6 ReleaseMutex(object: Handle) -> i32 => |call| {
        woken("ReleaseMutex", object as u64);
        call()
    };
    7 EnterCriticalSection(lock: Handle) => |call| {
        lock_of("EnterCriticalSection", lock, "TryEnterCriticalSection", call)
    };
    8 LeaveCriticalSection(lock: Handle) => |call| {
        released("LeaveCriticalSection", lock);
        call()
    };
    9 AcquireSRWLockExclusive(lock: Handle) => |call| {
        lock_of("AcquireSRWLockExclusive", lock, "TryAcquireSRWLockExclusive", call)
    };
    10 ReleaseSRWLockExclusive(lock: Handle) => |call| {
        released("ReleaseSRWLockExclusive", lock);
        call()
    };
    11 AcquireSRWLockShared(lock: Handle) => |call| {
        lock_of("AcquireSRWLockShared", lock, "TryAcquireSRWLockShared", call)
    };
    12 ReleaseSRWLockShared(lock: Handle) => |call| {
        released("ReleaseSRWLockShared", lock);
        call()
    };
    13 WaitOnAddress(at: Handle, compare: Handle, size: usize, ms: u32) -> i32 => |call| {
        wait("WaitOnAddress", at as u64, call)
    };
    14 WakeByAddressSingle(at: Handle) => |call| {
        released("WakeByAddressSingle", at);
        call()
    };
    15 WakeByAddressAll(at: Handle) => |call| {
        released("WakeByAddressAll", at);
        call()
    };
    16 SleepConditionVariableSRW(at: Handle, lock: Handle, ms: u32, flags: u32) -> i32 => |call| {
        wait("SleepConditionVariableSRW", at as u64, call)
    };
    17 SleepConditionVariableCS(at: Handle, lock: Handle, ms: u32) -> i32 => |call| {
        wait("SleepConditionVariableCS", at as u64, call)
    };
    18 WakeConditionVariable(at: Handle) => |call| {
        released("WakeConditionVariable", at);
        call()
    };
    19 WakeAllConditionVariable(at: Handle) => |call| {
        released("WakeAllConditionVariable", at);
        call()
    };
    20 WaitForMultipleObjects(count: u32, objects: *const Handle, all: i32, ms: u32) -> u32 => |call| {
        // Name the wait by its first handle, if present.
        let first = if count > 0 && !objects.is_null() {
            // SAFETY: the caller's wait-array contract guarantees that `objects`
            // points to `count` initialized, aligned handles readable during this call.
            (unsafe { *objects }) as u64
        } else {
            0
        };
        wait("WaitForMultipleObjects", first, call)
    };
    21 GetWriteWatch(flags: u32, at: Handle, size: usize, found: Handle, count: Handle, granularity: Handle) -> u32 => |call| {
        measured("GetWriteWatch", size, call)
    };
    22 VirtualProtect(at: Handle, size: usize, new: u32, old: Handle) -> i32 => |call| {
        measured("VirtualProtect", size, call)
    };
    23 ReadProcessMemory(process: Handle, at: Handle, to: Handle, size: usize, done: Handle) -> i32 => |call| {
        measured("ReadProcessMemory", size, call)
    };
    24 VirtualQuery(at: Handle, info: Handle, size: usize) -> usize => |call| {
        measured("VirtualQuery", 0, call)
    };
    25 VirtualAlloc(at: Handle, size: usize, kind: u32, protect: u32) -> Handle => |call| {
        measured("VirtualAlloc", size, call)
    };
    26 VirtualFree(at: Handle, size: usize, kind: u32) -> i32 => |call| {
        measured("VirtualFree", size, call)
    };
    27 _Mtx_lock(lock: Handle) -> i32 => |call| {
        wait("_Mtx_lock", lock as u64, call)
    };
    28 _Mtx_unlock(lock: Handle) -> i32 => |call| {
        released("_Mtx_unlock", lock);
        call()
    };
    29 TryEnterCriticalSection(lock: Handle) -> u8 => |call| call();
    30 TryAcquireSRWLockExclusive(lock: Handle) -> u8 => |call| call();
    31 TryAcquireSRWLockShared(lock: Handle) -> u8 => |call| call();
}

/// The wrappers from here on are there for their real functions only.
const ONLY_REAL: usize = 29;

/// Asks the system for the real functions, and points our own import table
/// at the wrappers: once, if a timeline is wanted.
fn arm() -> bool {
    static ONCE: Once = Once::new();
    if !super::record::wanted() {
        return false;
    }
    ONCE.call_once(wrap_own);
    true
}

/// See [`arm`].
pub(super) fn wrap_own() {
    find_real();
    // SAFETY: our own module's mapped image.
    let changed = unsafe { wrap_imports() };
    tracing::info!(changed, "the timeline's wrappers are in (VICTORY_TIMELINE)");
}

fn find_real() {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    for dll in [c"kernelbase.dll", c"kernel32.dll", c"ntdll.dll"] {
        // SAFETY: NUL-terminated names of modules every process has.
        let module = unsafe { GetModuleHandleA(dll.as_ptr().cast()) };
        for (name, at, _) in &TABLE {
            if at.load(Ordering::Relaxed) == 0 && !module.is_null() {
                let text = std::ffi::CString::new(*name).expect("a plain name");
                // SAFETY: as above.
                if let Some(f) = unsafe { GetProcAddress(module, text.as_ptr().cast()) } {
                    at.store(f as *const () as usize, Ordering::Relaxed);
                }
            }
        }
    }
}

/// What the title's import `name`, found at `address`, is bound to: the
/// address itself, or our wrapper of it.
pub fn import(name: &str, address: usize) -> usize {
    if !arm() {
        return address;
    }
    let Some((i, (_, at, _))) = TABLE
        .iter()
        .enumerate()
        .find(|(i, t)| *i < ONLY_REAL && t.0 == name)
    else {
        return address;
    };
    if at.load(Ordering::Relaxed) == 0 {
        at.store(address, Ordering::Relaxed);
    }
    wrapper(i)
}

/// Points the entries of our own import table that are in [`TABLE`] at the
/// wrappers: how many.
///
/// # Safety
/// The process's module is mapped as the loader left it.
unsafe fn wrap_imports() -> usize {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    // SAFETY: a plain query.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    let mut changed = 0;
    // SAFETY: the headers, the import directory and the tables it names lie
    // in the mapped image, as the loader read them; an entry is eight bytes,
    // aligned, and made writable for the store.
    unsafe {
        let at = |rva: u32| base + rva as usize;
        let read = |at: usize| (at as *const u32).read_unaligned();
        let headers = at(read(base + 0x3c));
        // The second data directory of the 64-bit optional header: imports.
        let mut descriptor = at(read(headers + 0x18 + 0x70 + 8));
        loop {
            let (names, first) = (read(descriptor), read(descriptor + 16));
            if first == 0 {
                break;
            }
            let names = if names != 0 { names } else { first };
            for i in 0.. {
                let entry = (at(names) as *const u64).add(i).read_unaligned();
                if entry == 0 {
                    break;
                }
                if entry >> 63 != 0 {
                    continue; // by number
                }
                let name = CStr::from_ptr((at(entry as u32) + 2) as *const _);
                let Some(index) = TABLE[..ONLY_REAL]
                    .iter()
                    .position(|t| t.0.as_bytes() == name.to_bytes())
                else {
                    continue;
                };
                if TABLE[index].1.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                let slot = (at(first) as *mut usize).add(i);
                let (mut old, mut ignored) = (0, 0);
                if VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) != 0 {
                    (*slot.cast::<AtomicUsize>()).store(wrapper(index), Ordering::SeqCst);
                    VirtualProtect(slot.cast(), 8, old, &mut ignored);
                    changed += 1;
                }
            }
            descriptor += 20;
        }
    }
    changed
}
