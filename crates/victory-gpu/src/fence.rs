//! Fences: a 64-bit value and the events and queues waiting for it. Three
//! things move a value: the title's CPU signals, our queues' signals (a
//! queue's work runs to completion before its signal, so a queue not held
//! back by a wait signals at once) and bottom-of-pipe writes a command list
//! makes at the fence's GPU address. All three wake the waits, so an
//! event, a blocking wait or a queue's wait ends once the value it asked
//! for is reached, whichever moved it.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::object::{self, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 5] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    // ID3D12Fence1.
    guid(
        0x433685fe,
        0xe22b,
        0x4ca0,
        [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Fence {
    object: Object,
    /// Titles also read it straight from memory, at the fence's GPU
    /// address.
    value: AtomicU64,
    flags: u32,
    /// Events to set once the value reaches theirs (handles as integers).
    waiters: Mutex<Vec<(u64, usize)>>,
    moved: Condvar,
}

impl Drop for Fence {
    fn drop(&mut self) {
        live().remove(&((&raw const self.value) as usize));
    }
}

/// Live fences by the address of their value (their GPU address), for
/// bottom-of-pipe writes that land on one. Locked before a fence's
/// `waiters`, never after.
static LIVE: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

fn live() -> MutexGuard<'static, BTreeMap<usize, usize>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// How often a blocking wait looks at the value without being woken (a
/// value moved by a path we do not see), and how long it waits before
/// saying it is stuck.
const RECHECK: Duration = Duration::from_millis(100);
const STALL: Duration = Duration::from_secs(5);

/// Direct stores bypass the runtime. Recheck fence events and held queues
/// while they still have work waiting on memory.
#[cfg(windows)]
pub(crate) mod watch_events {
    use super::*;

    static GENERATION: Mutex<u64> = Mutex::new(0);
    static ADDED: Condvar = Condvar::new();

    pub(crate) fn notify() {
        static THREAD: std::sync::Once = std::sync::Once::new();
        {
            let mut g = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
            *g = g.wrapping_add(1);
            ADDED.notify_one();
        }
        THREAD.call_once(|| {
            if let Err(error) = std::thread::Builder::new()
                .name("victory-fence-watch".into())
                .spawn(run)
            {
                tracing::warn!(%error, "cannot watch direct fence writes");
            }
        });
    }

    fn run() {
        loop {
            let seen = *GENERATION.lock().unwrap_or_else(|e| e.into_inner());
            let mut pending = false;
            for &at in live().values() {
                // SAFETY: the destructor must acquire the registry lock held here.
                let fence = unsafe { &*(at as *const Fence) };
                let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
                if !waiters.is_empty() {
                    release_events(&mut waiters, fence.value.load(Ordering::Acquire));
                    pending |= !waiters.is_empty();
                }
            }
            pending |= crate::queue::poll_held();
            let generation = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
            if pending {
                drop(
                    ADDED.wait_timeout_while(generation, Duration::from_millis(10), |g| *g == seen),
                );
            } else {
                drop(ADDED.wait_while(generation, |g| *g == seen));
            }
        }
    }
}

fn fence<'a>(this: This) -> &'a Fence {
    // SAFETY: called from fence methods only.
    unsafe { &*this.cast::<Fence>() }
}

pub(crate) extern "system" fn create(
    _: This,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(feature = "diagnostics")]
    tracing::debug!(initial, flags, "fence");
    let fence = Box::new(Fence {
        object: Object::new(&FUNCTIONS),
        value: AtomicU64::new(initial),
        flags,
        waiters: Mutex::new(Vec::new()),
        moved: Condvar::new(),
    });
    live().insert(
        (&raw const fence.value) as usize,
        (&raw const *fence) as usize,
    );
    object::hand_out(FENCE, fence, iid, &FENCE_IIDS, out)
}

fn set_event(event: usize) {
    #[cfg(windows)]
    // SAFETY: the title passed an event handle.
    unsafe {
        windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void)
    };
    #[cfg(not(windows))]
    let _ = event;
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        FENCE,
        iid,
        object::is(iid, &FENCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Fence>(this)
}
pub(crate) extern "system" fn completed(this: This) -> u64 {
    fence(this).value.load(Ordering::Acquire)
}
/// The title's `GetCompletedValue`.
extern "system" fn completed_called(this: This) -> u64 {
    let value = completed(this);
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::polled(this as usize, value);
    value
}
extern "system" fn set_event_on_completion(this: This, value: u64, event: *mut c_void) -> HRESULT {
    let fence = fence(this);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    if fence.value.load(Ordering::Acquire) >= value {
        if !event.is_null() {
            set_event(event as usize);
        }
        return S_OK;
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::told("an event asked for", this as usize, value);
    if !event.is_null() {
        waiters.push((value, event as usize));
        drop(waiters);
        #[cfg(windows)]
        watch_events::notify();
        return S_OK;
    }
    #[cfg(windows)]
    if crate::host::late() {
        drop(waiters);
        crate::queue::before_sync();
        crate::host::settle_for_fence();
        waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    }
    // A null event: block until the value is reached, as D3D12 does.
    let started = Instant::now();
    let mut warned = Duration::ZERO;
    while fence.value.load(Ordering::Acquire) < value {
        waiters = fence
            .moved
            .wait_timeout(waiters, RECHECK)
            .unwrap_or_else(|e| e.into_inner())
            .0;
        let waited = started.elapsed();
        if waited >= warned + STALL {
            warned = waited;
            tracing::warn!(
                value,
                completed = fence.value.load(Ordering::Acquire),
                seconds = waited.as_secs(),
                "blocking fence wait still waiting"
            );
        }
    }
    S_OK
}
/// Moves the fence to `value` and sets the events that waited for it.
pub(crate) fn set(this: This, value: u64) {
    let fence = fence(this);
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::told("a fence set", this as usize, value);
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    fence.value.store(value, Ordering::Release);
    release_events(&mut waiters, value);
    fence.moved.notify_all();
    drop(waiters);
    #[cfg(windows)]
    // SAFETY: wake any guest thread polling/blocking via WaitOnAddress on the fence's GPU VA.
    unsafe {
        windows_sys::Win32::System::Threading::WakeByAddressAll((&raw const fence.value).cast());
    }
}
/// Owns the reference a queued fence operation needs until it is retired.
#[cfg(windows)]
pub(crate) struct FenceRef(This);

// SAFETY: the fence count and value are atomic and its waiters are locked.
#[cfg(windows)]
unsafe impl Send for FenceRef {}

#[cfg(windows)]
impl FenceRef {
    pub(crate) fn new(fence: This) -> Self {
        object::add_ref(fence);
        Self(fence)
    }

    pub(crate) fn get(&self) -> This {
        self.0
    }
}

#[cfg(windows)]
impl Clone for FenceRef {
    fn clone(&self) -> Self {
        Self::new(self.get())
    }
}

#[cfg(windows)]
impl Drop for FenceRef {
    fn drop(&mut self) {
        release(self.0);
    }
}

fn release_events(waiters: &mut Vec<(u64, usize)>, value: u64) {
    waiters.retain(|&(wanted, event)| {
        let reached = wanted <= value;
        if reached {
            set_event(event);
        }
        !reached
    });
}

/// A bottom-of-pipe write to a live fence is atomic, including stores to
/// either half of the value. Returns false for ordinary title memory.
pub(crate) fn write_value(at: usize, bytes: usize, value: u64) -> bool {
    let live = live();
    let Some((&base, &object)) = live.range(..=at).next_back() else {
        return false;
    };
    if !matches!((at - base, bytes), (0, 8) | (0 | 4, 4)) {
        return false;
    }
    // SAFETY: a registered fence cannot be deallocated while this lock is held.
    let fence = unsafe { &*(object as *const Fence) };
    let mut waiters = fence.waiters.lock().unwrap_or_else(|e| e.into_inner());
    let value = if bytes == 4 {
        let shift = (at - base) * 8;
        let mask = u64::from(u32::MAX) << shift;
        let bits = u64::from(value as u32) << shift;
        let old = fence
            .value
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some((old & !mask) | bits)
            });
        (old.expect("the update always returns a value") & !mask) | bits
    } else {
        fence.value.store(value, Ordering::Release);
        value
    };
    release_events(&mut waiters, value);
    fence.moved.notify_all();
    true
}

pub(crate) extern "system" fn signal(this: This, value: u64) -> HRESULT {
    let _called = crate::timeline::span("a fence set");
    set(this, value);
    // A queue may have waited for it: what it held runs now.
    #[cfg(windows)]
    crate::queue::release_held(false);
    S_OK
}

extern "system" fn creation_flags(this: This) -> u32 {
    fence(this).flags
}
/// Where the value lives (GPU addresses are CPU addresses).
extern "system" fn gpu_address(this: This) -> u64 {
    (&raw const fence(this).value) as u64
}

/// IUnknown 3, object 4, device child 1, fence 3, fence 1 (2, a console
/// one among them).
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<FENCE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = completed_called as *const (); // 0x40
    f[9] = set_event_on_completion as *const (); // 0x48
    f[10] = signal as *const (); // 0x50
    f[11] = creation_flags as *const (); // 0x58
    f[12] = gpu_address as *const (); // 0x60
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    type Wait = extern "system" fn(This, u64, *mut c_void) -> HRESULT;
    type Signal = extern "system" fn(This, u64) -> HRESULT;

    fn new_fence(initial: u64) -> This {
        let mut f = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), initial, 0, &FENCE_IIDS[0], &mut f),
            S_OK
        );
        f
    }

    #[test]
    fn fence1_is_created_and_queried_with_creation_flags() {
        let mut f = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), 0, 0x5, &FENCE_IIDS[1], &mut f),
            S_OK
        );
        assert!(!f.is_null());
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x58)(f), 0x5);
        let mut again = std::ptr::null();
        assert_eq!(
            slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(f, 0)(
                f,
                &FENCE_IIDS[1],
                &mut again
            ),
            S_OK
        );
        assert_eq!(again, f);
        // QueryInterface took a second reference.
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(again, 0x10)(again),
            1
        );
        release_fence(f);
    }

    fn release_fence(f: This) {
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    fn address(f: This) -> usize {
        slot::<extern "system" fn(This) -> u64>(f, 0x60)(f) as usize
    }

    /// A fence pointer that may cross threads (the fence is `Sync`).
    #[derive(Clone, Copy)]
    struct Shared(This);
    // SAFETY: fences are made for use from many threads.
    unsafe impl Send for Shared {}

    /// Runs a blocking wait for `value` on another thread; the receiver
    /// gets its result once it returns.
    fn blocking_wait(f: This, value: u64) -> std::sync::mpsc::Receiver<HRESULT> {
        let (done, result) = std::sync::mpsc::channel();
        let f = Shared(f);
        std::thread::spawn(move || {
            let f = f;
            let wait = slot::<Wait>(f.0, 0x48);
            let _ = done.send(wait(f.0, value, std::ptr::null_mut()));
        });
        result
    }

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let f = new_fence(1);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        // SAFETY: plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let wait = slot::<Wait>(f, 0x48);
        let signal = slot::<Signal>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        // SAFETY: a valid event.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(signal(f, 5), S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        // The title reads the value at the fence's address.
        // SAFETY: the fence's value.
        assert_eq!(unsafe { *(address(f) as *const u64) }, 5);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        release_fence(f);
    }

    #[test]
    fn blocking_wait_returns_at_once_when_reached() {
        let f = new_fence(7);
        let wait = slot::<Wait>(f, 0x48);
        assert_eq!(wait(f, 7, std::ptr::null_mut()), S_OK);
        assert_eq!(wait(f, 3, std::ptr::null_mut()), S_OK);
        release_fence(f);
    }

    /// Used to answer `E_NOTIMPL` at once, so the title went on before the
    /// value was reached.
    #[test]
    fn blocking_wait_blocks_until_signalled() {
        let f = new_fence(0);
        let result = blocking_wait(f, 2);
        let short = Duration::from_millis(200);
        assert!(
            result.recv_timeout(short).is_err(),
            "returned before any signal"
        );
        let signal = slot::<Signal>(f, 0x50);
        assert_eq!(signal(f, 1), S_OK);
        assert!(
            result.recv_timeout(short).is_err(),
            "returned below its value"
        );
        assert_eq!(signal(f, 2), S_OK);
        assert_eq!(result.recv_timeout(Duration::from_secs(5)), Ok(S_OK));
        release_fence(f);
    }

    #[test]
    fn bottom_of_pipe_write_releases_blocking_wait() {
        let f = new_fence(0);
        let result = blocking_wait(f, 0x1_0000_0003);
        std::thread::sleep(Duration::from_millis(50));
        // The low half alone does not reach the value.
        crate::queue::Written::Value(3).write(address(f) as *mut u8, 4);
        assert!(result.recv_timeout(Duration::from_millis(200)).is_err());
        crate::queue::Written::Value(0x1_0000_0003).write(address(f) as *mut u8, 8);
        assert_eq!(result.recv_timeout(Duration::from_secs(5)), Ok(S_OK));
        release_fence(f);
    }

    #[cfg(windows)]
    #[test]
    fn bottom_of_pipe_write_sets_waiting_event() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let f = new_fence(0);
        // SAFETY: plain manual-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null());
        assert_eq!(slot::<Wait>(f, 0x48)(f, 9, event), S_OK);
        crate::queue::Written::Value(9).write(address(f) as *mut u8, 8);
        // SAFETY: a valid event.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        release_fence(f);
    }

    #[cfg(windows)]
    #[test]
    fn direct_memory_write_sets_waiting_event() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let f = new_fence(0);
        // SAFETY: plain manual-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null());
        assert_eq!(slot::<Wait>(f, 0x48)(f, 11, event), S_OK);
        // SAFETY: a valid event.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        // Direct write to the fence's GPU address, bypassing `Signal` and
        // `Written::write`.
        // SAFETY: `address(f)` is `Fence::value`, an `AtomicU64`.
        unsafe { (*(address(f) as *const AtomicU64)).store(11, Ordering::Release) };
        // SAFETY: a valid event.
        assert_eq!(unsafe { WaitForSingleObject(event, 2_000) }, WAIT_OBJECT_0);
        // SAFETY: ours.
        unsafe { CloseHandle(event) };
        release_fence(f);
    }
}
