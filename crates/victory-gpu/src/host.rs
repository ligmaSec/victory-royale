//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap;
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{
    DXGI_STATUS_OCCLUDED, HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HRESULT, Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;
/// Maximum number of detached host lists kept for reuse after pending resets.
const LIST_POOL_CAP: usize = 64;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    /// Why no presenter was made (or resized), and when to try again: a run
    /// with no desktop to present on would otherwise ask DXGI at the frame
    /// rate.
    present_retry: Mutex<Retry>,
    /// Detached host lists whose submissions finished, ready to reuse.
    free_lists: Mutex<Vec<HostList>>,
    #[cfg(test)]
    reused_lists: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    d3d12_errors: std::sync::atomic::AtomicU64,
}

/// A presenter that could not be made, and when to try again. Without this,
/// a run behind a locked screen or a sleeping display retried every frame:
/// 1,654 `no swap chain` lines in 60 seconds, and one DXGI call per frame,
/// which buries the log and costs a submission's worth of work each time.
struct Retry {
    attempts: u32,
    /// The next attempt is due at (or after) this instant; `None` with
    /// `attempts == 0` means nothing has failed.
    next: Option<std::time::Instant>,
}

impl Retry {
    const fn new() -> Self {
        Retry {
            attempts: 0,
            next: None,
        }
    }

    /// Whether an attempt is due at `now`.
    fn due(&self, now: std::time::Instant) -> bool {
        self.next.is_none_or(|at| now >= at)
    }

    /// Records a failed attempt at `now` and when the next one may run.
    fn failed(&mut self, now: std::time::Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.next = Some(now + retry_delay(self.attempts));
    }

    /// Clears the failure state after an attempt worked, returning how many
    /// attempts had failed before it.
    fn reset(&mut self) -> u32 {
        let attempts = self.attempts;
        *self = Retry::new();
        attempts
    }
}

/// How long to wait before retry number `attempts`: 100 ms, doubling, up to
/// one second, so a state that clears still recovers within a second.
fn retry_delay(attempts: u32) -> std::time::Duration {
    const FIRST_MS: u64 = 100;
    const MOST_MS: u64 = 1_000;
    let doubling = FIRST_MS << attempts.saturating_sub(1).min(4);
    std::time::Duration::from_millis(doubling.min(MOST_MS))
}

/// Which step of making a presenter failed, so the log says whether the
/// window, the swap chain or its command list was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresentStep {
    Window,
    Factory,
    SwapChain,
    List,
}

impl PresentStep {
    fn name(self) -> &'static str {
        match self {
            PresentStep::Window => "window",
            PresentStep::Factory => "DXGI factory",
            PresentStep::SwapChain => "swap chain",
            PresentStep::List => "copy list",
        }
    }
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Warn once when a pinned epoch holds back this many retired host objects
/// (for example, an unreset command list or a submission stalled behind an
/// unsignalled fence).
const RETIRED_WARN_THRESHOLD: usize = 4096;

/// Host objects the title let go of while an open list or a queued/running
/// submission may still use them (a texture its descriptors name, a descriptor
/// slot's old resource, a buffer shadow, or texture staging), by the highest
/// epoch that existed when they were retired. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while lists are
/// recorded or submissions run for most of a second in the world: freeing there
/// was a GPU page fault on the freed texture (DXGI_ERROR_DEVICE_HUNG,
/// 2026-11-24, RX 7600).
///
/// Any stuck pinned epoch (such as a list that recorded GPU work and is never
/// reset, or a submission waiting behind a fence that never signals) pins all
/// later retirements until it is released; a one-time warning is logged once
/// the queue reaches [`RETIRED_WARN_THRESHOLD`].
///
/// Lock invariant: `RETIRE_STATE` is a strict leaf lock — never lock anything
/// else while holding it, and ALWAYS drop retired objects AFTER releasing it.
struct RetireState {
    next_epoch: u64,
    active_epochs: std::collections::BTreeMap<u64, usize>,
    retired: std::collections::VecDeque<(u64, Retired)>,
    warned: bool,
}

impl RetireState {
    const fn new() -> Self {
        Self {
            next_epoch: 1,
            active_epochs: std::collections::BTreeMap::new(),
            retired: std::collections::VecDeque::new(),
            warned: false,
        }
    }
}

static RETIRE_STATE: Mutex<RetireState> = Mutex::new(RetireState::new());

#[cfg(test)]
thread_local! {
    static RETIRE_SCOPE: std::cell::RefCell<Option<std::sync::Arc<Mutex<RetireState>>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct RetireTestScope {
    prev: Option<std::sync::Arc<Mutex<RetireState>>>,
}

#[cfg(test)]
impl Drop for RetireTestScope {
    fn drop(&mut self) {
        RETIRE_SCOPE.with(|cell| *cell.borrow_mut() = self.prev.take());
    }
}

#[cfg(test)]
fn current_retire_scope() -> Option<std::sync::Arc<Mutex<RetireState>>> {
    RETIRE_SCOPE.with(|cell| cell.borrow().clone())
}

#[cfg(test)]
fn enter_retire_scope(scope: Option<std::sync::Arc<Mutex<RetireState>>>) -> RetireTestScope {
    let prev = RETIRE_SCOPE.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), scope));
    RetireTestScope { prev }
}

/// Isolates retirement epochs and queued drops on the current test thread (and
/// any `HostList` created or acquired on it) from concurrent GPU tests.
#[cfg(test)]
pub(crate) fn isolated_retire_scope() -> RetireTestScope {
    let fresh = std::sync::Arc::new(Mutex::new(RetireState::new()));
    let prev = RETIRE_SCOPE.with(|cell| cell.borrow_mut().replace(fresh));
    RetireTestScope { prev }
}

/// A retired object, dropped on whatever thread finishes the blocking epoch.
struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Acquires a monotonically increasing epoch (never 0) that keeps retired
/// objects alive until [`release_epoch_in`] is called with it.
fn acquire_epoch_in(retire_state: &Mutex<RetireState>) -> u64 {
    let mut state = retire_state.lock().unwrap_or_else(|e| e.into_inner());
    let e = state.next_epoch;
    state.next_epoch += 1;
    *state.active_epochs.entry(e).or_default() += 1;
    e
}

/// Releases an epoch acquired with [`acquire_epoch_in`] and drops any retired
/// objects whose blocking epochs have all completed, after releasing the lock.
fn release_epoch_in(retire_state: &Mutex<RetireState>, epoch: u64) {
    if epoch == 0 {
        return;
    }
    let done: Vec<Retired> = {
        let mut state = retire_state.lock().unwrap_or_else(|e| e.into_inner());
        if let std::collections::btree_map::Entry::Occupied(mut entry) =
            state.active_epochs.entry(epoch)
        {
            if *entry.get() <= 1 {
                entry.remove();
            } else {
                *entry.get_mut() -= 1;
            }
        }
        let min_active = state.active_epochs.keys().next().copied();
        let mut done = Vec::new();
        while let Some(&(wait_for, _)) = state.retired.front() {
            if min_active.is_none_or(|min| min > wait_for) {
                if let Some((_, item)) = state.retired.pop_front() {
                    done.push(item);
                }
            } else {
                break;
            }
        }
        done
    };
    drop(done);
}

fn retire_in<T: 'static>(retire_state: &Mutex<RetireState>, object: T) {
    let warn = {
        let mut state = retire_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_epochs.is_empty() {
            drop(state);
            drop(object);
            return;
        }
        let wait_for = state.next_epoch.saturating_sub(1);
        state
            .retired
            .push_back((wait_for, Retired(Box::new(object))));
        if !state.warned && state.retired.len() >= RETIRED_WARN_THRESHOLD {
            state.warned = true;
            Some((
                state.retired.len(),
                state.active_epochs.keys().next().copied().unwrap_or(0),
                state.active_epochs.len(),
                wait_for,
            ))
        } else {
            None
        }
    };
    if let Some((pending, oldest_epoch, active_epochs, latest_epoch)) = warn {
        tracing::warn!(
            pending,
            oldest_epoch,
            active_epochs,
            latest_epoch,
            "retired host resource queue grew past threshold; an unreset command list or stalled wait may be pinning epochs"
        );
    }
}

/// Drops `object` once every epoch active now has finished: at once if none is.
/// Note: a stuck pinned epoch (such as an unreset list with recorded work or a
/// submission behind an unsignalled fence) holds back all later retirements
/// until released.
pub(crate) fn retire<T: 'static>(object: T) {
    #[cfg(test)]
    if let Some(scope) = current_retire_scope() {
        retire_in(&scope, object);
        return;
    }
    retire_in(&RETIRE_STATE, object);
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => {
                text.push_str("; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command")
            }
            None => {}
        }
    }
    tracing::error!("{text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug = cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DEBUG");
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if crate::switch::on("VICTORY_D3D12_GBV")
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        let mut chosen: Option<DXGI_ADAPTER_DESC1> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_WARP") {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            let filter = std::env::var("VICTORY_ADAPTER")
                .ok()
                .filter(|s| !s.trim().is_empty());
            match pick_hardware_adapter(filter.as_deref()) {
                Ok((adapter, desc)) => {
                    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                    chosen = Some(desc);
                }
                Err(e) if filter.is_some() => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "hardware adapter selection failed; trying the system default adapter"
                    );
                    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                }
            }
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        #[cfg(feature = "diagnostics")]
        if debug
            && crate::switch::on("VICTORY_D3D12_GBV")
            && let Some(mode) =
                parse_eager_gbv(std::env::var_os("VICTORY_D3D12_GBV_EAGER").as_deref())
        {
            match configure_eager_gbv(&device, mode) {
                Ok(()) => tracing::info!(
                    guarded = matches!(mode, EagerGbv::Guarded),
                    "GPU validation pipeline patching front-loaded"
                ),
                Err(e) if matches!(mode, EagerGbv::Guarded) => return Err(e),
                Err(e) => tracing::warn!(error = %e, "could not front-load GPU validation"),
            }
        }
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        if let Some(desc) = chosen {
            let name = adapter_name(&desc.Description);
            tracing::info!(
                adapter = %name,
                vendor = format_args!("{:#06x}", desc.VendorId),
                device = format_args!("{:#06x}", desc.DeviceId),
                vram_mb = desc.DedicatedVideoMemory >> 20,
                "host D3D12 device ready"
            );
        } else {
            tracing::info!("host D3D12 device ready");
        }
        Ok(Host {
            device,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
            present_retry: Mutex::new(Retry::new()),
            free_lists: Mutex::default(),
            #[cfg(test)]
            reused_lists: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            d3d12_errors: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EagerGbv {
    Current,
    Guarded,
}

#[cfg(feature = "diagnostics")]
fn parse_eager_gbv(value: Option<&std::ffi::OsStr>) -> Option<EagerGbv> {
    let value = value?;
    if value.is_empty() || value == "0" {
        return None;
    }
    if value == "guarded" {
        Some(EagerGbv::Guarded)
    } else {
        Some(EagerGbv::Current)
    }
}

#[cfg(feature = "diagnostics")]
fn front_load_flag_for(
    mode: D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE,
) -> windows::core::Result<D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAGS> {
    match mode {
        D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_UNGUARDED_VALIDATION => Ok(
            D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_UNGUARDED_VALIDATION_SHADERS,
        ),
        D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_GUARDED_VALIDATION => Ok(
            D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_GUARDED_VALIDATION_SHADERS,
        ),
        D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_STATE_TRACKING_ONLY => Ok(
            D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_TRACKING_ONLY_SHADERS,
        ),
        _ => Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        )),
    }
}

/// Requests validation shader compilation at PSO creation rather than first
/// bind, preserving unrelated flags and verifying read-back settings before any
/// pipelines are built.
#[cfg(feature = "diagnostics")]
fn configure_eager_gbv(device: &ID3D12Device, mode: EagerGbv) -> windows::core::Result<()> {
    let debug = device.cast::<ID3D12DebugDevice1>()?;
    let kind = D3D12_DEBUG_DEVICE_PARAMETER_GPU_BASED_VALIDATION_SETTINGS;
    let size = size_of::<D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS>() as u32;
    let mut settings = D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS::default();
    // SAFETY: `kind` takes `D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS` of exact `size`.
    unsafe { debug.GetDebugParameter(kind, (&raw mut settings).cast(), size)? };
    let prior_flags = settings.PipelineStateCreateFlags.0;
    if mode == EagerGbv::Guarded {
        settings.DefaultShaderPatchMode =
            D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_GUARDED_VALIDATION;
    }
    let flag = front_load_flag_for(settings.DefaultShaderPatchMode)?;
    settings.PipelineStateCreateFlags.0 |= flag.0;
    let mut observed = D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS::default();
    // SAFETY: both structs are stack-owned for the synchronous COM calls.
    unsafe {
        debug.SetDebugParameter(kind, (&raw const settings).cast(), size)?;
        debug.GetDebugParameter(kind, (&raw mut observed).cast(), size)?;
    }
    if observed.DefaultShaderPatchMode != settings.DefaultShaderPatchMode
        || observed.PipelineStateCreateFlags.0 & flag.0 == 0
        || observed.PipelineStateCreateFlags.0 & prior_flags != prior_flags
    {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ));
    }
    Ok(())
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// Actual D3D12 access state per host buffer for this recording.
    shadow_states: Mutex<std::collections::HashMap<usize, (ID3D12Resource, D3D12_RESOURCE_STATES)>>,
    /// Incremented whenever `states` inserts a new texture or transitions a
    /// texture to a different state; reset to 0 when the list is reset.
    state_gen: std::sync::atomic::AtomicU64,
    /// Title resource pointers whose tracked state changed on this list since
    /// reset (append-only; cleared on reset). Lets `draw::State::bindless_textures`
    /// re-sync only the textures touched since the previous draw instead of
    /// walking every resident bindless texture on RT switches or copies.
    dirty_resources: Mutex<Vec<usize>>,
    /// The list that runs those transitions just before this one.
    prelude: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// Memory handed out for the list's recording, kept until it is finished
    /// when the title reset or dropped the list while a submission of it was
    /// still to run.
    kept_uploads: Mutex<Vec<Vec<u8>>>,
    /// Dynamic descriptor slots kept alongside `kept_uploads` until the pending
    /// submission completes.
    kept_dynamic: Mutex<Vec<(usize, u32, u32)>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// GPU-writable scratch for indirect rectangle expansion (`rectangles`):
    /// expanded vertices, rewritten draw arguments and per-draw atomic
    /// counters, reused across draws and across resets.
    rect_scratch: Mutex<RectScratch>,
    /// Active retirement epoch pinned while this list has recorded work or is
    /// queued/executing (0 when idle).
    epoch: std::sync::atomic::AtomicU64,
    /// Queued or executing submissions currently pinning this list.
    in_flight: std::sync::atomic::AtomicUsize,
    /// Serializes `in_flight` transitions with `epoch` updates so a concurrent
    /// `end_submit` never clears an epoch pinned by a new `begin_submit`.
    epoch_lock: Mutex<()>,
    /// Test-only isolated retirement state captured when the list is created or
    /// acquired so parallel GPU tests do not pin each other's drops.
    #[cfg(test)]
    retire_scope: Option<std::sync::Arc<Mutex<RetireState>>>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

impl Drop for HostList {
    fn drop(&mut self) {
        self.release_kept();
        self.release_epoch();
    }
}

// SAFETY: D3D12 lists and allocators may be used from any thread, one at
// a time: the title records a list on one thread at a time, and a
// submission runs it after its recording ended (a reset while one is to
// run moves recording to a new list). The rest is locked.
unsafe impl Send for HostList {}
// SAFETY: as above.
unsafe impl Sync for HostList {}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

/// A single first access can promote implicitly. Use an explicit state for
/// a combined read mask and retain existing read permissions on later reads.
fn shadow_transition(
    before: D3D12_RESOURCE_STATES,
    wanted: D3D12_RESOURCE_STATES,
) -> Option<D3D12_RESOURCE_STATES> {
    if before == D3D12_RESOURCE_STATE_COMMON {
        return (wanted.0.count_ones() != 1).then_some(wanted);
    }
    if before.contains(wanted) {
        return None;
    }
    let after = if before != D3D12_RESOURCE_STATE_UNORDERED_ACCESS
        && wanted != D3D12_RESOURCE_STATE_UNORDERED_ACCESS
    {
        before | wanted
    } else {
        wanted
    };
    Some(after)
}

type States = std::collections::HashMap<
    usize,
    (
        u32,
        u32,
        Option<u32>,
        std::sync::Arc<crate::resource::HostTexture>,
    ),
>;

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DRED"))
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_GPU_TIMING"))
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// Upload chunks handed out front to back: the chunks, the one in use and
/// the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(ID3D12Resource, usize, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

const SCRATCH_CHUNK: u64 = 4 << 20;

/// Per-list GPU-writable buffers for indirect rectangle expansion
/// (`crate::rectangles`): one vertex scratch buffer (reused per draw after
/// its VB -> UAV barrier), one rewritten-arguments buffer (grown if a draw
/// has more records), and chunks of 4-byte atomic counters zeroed before
/// submission.
#[derive(Default)]
struct RectScratch {
    vertices: Option<ID3D12Resource>,
    args: Option<(ID3D12Resource, u64)>,
    counters: Vec<(ID3D12Resource, usize, u64)>,
    counter_chunk: usize,
    counter_used: u32,
}

// SAFETY: D3D12 resources are free-threaded; the counter pointers are
// persistent mappings written only before submission.
unsafe impl Send for RectScratch {}

/// Counters per chunk (4 KiB = 1024 words).
const RECT_COUNTER_CHUNK: u32 = 1024;
/// Initial rewritten-arguments buffer size (64 KiB = 4096 draw records).
const RECT_ARGS_MIN: u64 = 64 << 10;

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see. Runs with
    /// `title_dst = true` so unmapped title pages keep the commitment guard.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory. Runs with
    /// `title_dst = false`: the destination is host-mapped, so Wine/vkd3d
    /// `VirtualQuery` quirks must not skip it (#518).
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
}

/// A texture-to-buffer readback copied from a mapped readback buffer into
/// title memory once the GPU submission completes.
struct Readback {
    dst: usize,
    src: usize,
    dst_pitch: usize,
    src_pitch: usize,
    lines: usize,
    row_bytes: usize,
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload/readback buffers its GPU commands name, kept until reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    /// Post-submission readbacks from GPU textures into title buffers.
    readbacks: Vec<Readback>,
    uploads: Vec<ID3D12Resource>,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes), and upload fills made since (destination, source,
    /// bytes): doing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Most detailed mip this list has copied for each texture.
    textures: std::collections::HashMap<usize, u32>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Monotonic count of deferred title-memory copies (`defer_copy`) recorded
    /// since reset, used to partition fresh-table CBV snapshot reuse across
    /// intervening buffer writes.
    copy_gen: u64,
    /// Whether a step zeroes the indirect-work budget.
    budget_zeroed: bool,
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// Compute wave translations that this adapter can execute.
    pub(crate) fn wave_widths(&self) -> victory_shader::emit::ComputeWaves {
        static WIDTHS: OnceLock<victory_shader::emit::ComputeWaves> = OnceLock::new();
        *WIDTHS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            // SAFETY: query writes its own `D3D12_FEATURE_DATA_D3D12_OPTIONS1` struct.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS1,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let widths = wave_widths(
                ok && options.WaveOps.as_bool(),
                options.WaveLaneCountMin,
                options.WaveLaneCountMax,
            );
            tracing::info!(
                wave32 = widths.wave32,
                narrow_quads = widths.narrow_quads,
                min = options.WaveLaneCountMin,
                max = options.WaveLaneCountMax,
                "host compute wave widths"
            );
            widths
        })
    }

    /// A recycled host list (reset and open), or a new one when the pool is
    /// empty.
    pub(crate) fn acquire_list(&self) -> windows::core::Result<HostList> {
        let pooled = self
            .free_lists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop();
        #[cfg(test)]
        if let Some(mut list) = pooled {
            self.reused_lists.fetch_add(1, Ordering::Relaxed);
            list.reset();
            list.retire_scope = current_retire_scope();
            return Ok(list);
        }
        #[cfg(not(test))]
        if let Some(list) = pooled {
            list.reset();
            return Ok(list);
        }
        self.new_list()
    }

    #[cfg(test)]
    pub(crate) fn reused_lists(&self) -> u64 {
        self.reused_lists.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn d3d12_errors(&self) -> u64 {
        self.d3d12_errors.load(Ordering::Relaxed)
    }

    /// Returns a finished, unreferenced host list to the pool up to
    /// [`LIST_POOL_CAP`], releasing any kept upload blocks and dynamic slots
    /// immediately.
    pub(crate) fn recycle_list(&self, list: HostList) {
        list.release_kept();
        let mut pool = self.free_lists.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < LIST_POOL_CAP {
            pool.push(list);
        }
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude_allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let prelude: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &prelude_allocator,
                None,
            )?;
            prelude.Close()?;
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                shadow_states: Mutex::default(),
                state_gen: std::sync::atomic::AtomicU64::new(0),
                dirty_resources: Mutex::default(),
                prelude: (prelude_allocator, prelude),
                kept_uploads: Mutex::default(),
                kept_dynamic: Mutex::default(),
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                rect_scratch: Mutex::default(),
                epoch: std::sync::atomic::AtomicU64::new(0),
                in_flight: std::sync::atomic::AtomicUsize::new(0),
                epoch_lock: Mutex::default(),
                #[cfg(test)]
                retire_scope: current_retire_scope(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// Runs closed lists and waits for them.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        // D3D12 requires NumCommandLists >= 1; an empty submission is a no-op
        // (VIC-BT-035 / #440).
        if runs.is_empty() {
            return;
        }
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        let exec_scope = runs
            .iter()
            .find_map(|l| l.retire_scope.clone())
            .or_else(current_retire_scope);
        #[cfg(test)]
        let exec_state: &Mutex<RetireState> = exec_scope.as_deref().unwrap_or(&RETIRE_STATE);
        #[cfg(not(test))]
        let exec_state: &Mutex<RetireState> = &RETIRE_STATE;
        let exec_epoch = acquire_epoch_in(exec_state);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        {
            #[cfg(test)]
            let _scope = enter_retire_scope(exec_scope.clone());
            for list in runs {
                list.run_deferred(&mut batch);
                firsts.push(lists.len());
                if let Some(prelude) = list.prelude() {
                    lists.push(Some(prelude));
                }
                lists.push(list.list.cast().ok());
            }
        }
        batch.flush();
        let prepared = std::time::Instant::now();
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                match self.queue.Signal(&self.fence, *submitted) {
                    Ok(()) => match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            let waited = WaitForSingleObject(self.event, INFINITE);
                            if waited != WAIT_OBJECT_0 {
                                tracing::warn!(
                                    wait = waited.0,
                                    "host fence event wait failed; polling fence"
                                );
                                self.poll_fence(*submitted);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "host fence event registration failed; polling fence"
                            );
                            self.poll_fence(*submitted);
                        }
                    },
                    Err(e) => fail(format_args!(
                        "host GPU fence signal failed after a submission of {} lists ({e}){}{}",
                        runs.len(),
                        self.removal_data(runs),
                        last_notes(runs)
                    )),
                }
            }
            #[cfg(feature = "diagnostics")]
            if groups.len() > 1 && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i]
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                tracing::warn!(
                    ms = started.elapsed().as_millis(),
                    commands,
                    "slow host list"
                );
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            tracing::warn!(
                ms = gpu.as_millis(),
                lists = runs.len(),
                ?pipelines,
                "slow host submission"
            );
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        for (shadow, address, len) in batch.gpu_writes {
            let written = shadow.copy_back(address, len);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        }
        for list in runs {
            for rb in &list.deferred().readbacks {
                let written = if rb.dst_pitch == rb.row_bytes && rb.src_pitch == rb.row_bytes {
                    let total = rb.lines * rb.row_bytes;
                    crate::watch::writing(rb.dst, total);
                    write_mapped(rb.dst as *mut u8, rb.src as *const u8, total);
                    total
                } else {
                    let mut n = 0;
                    for line in 0..rb.lines {
                        let to = rb.dst + line * rb.dst_pitch;
                        let from = rb.src + line * rb.src_pitch;
                        crate::watch::writing(to, rb.row_bytes);
                        write_mapped(to as *mut u8, from as *const u8, rb.row_bytes);
                        n += rb.row_bytes;
                    }
                    n
                };
                COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            }
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        // Queues these writes release run their work once no lock is held:
        // it submits lists itself (this lock, the lists' `deferred`).
        let mut woken = crate::fence::Woken::default();
        for list in runs {
            for &(at, what, bytes) in &list.deferred().writes {
                woken.add(what.write(at as *mut u8, bytes));
            }
        }
        self.log_messages();
        drop(submitted);
        release_epoch_in(exec_state, exec_epoch);
        woken.run();
    }

    /// Fallback completion wait when registering or waiting on `self.event`
    /// fails after `self.queue.Signal(&self.fence, target)` succeeded, so
    /// copy-back, writes, and epoch retirement never run while the GPU may
    /// still be executing.
    fn poll_fence(&self, target: u64) {
        let mut spins = 0u32;
        loop {
            // SAFETY: plain fence and device queries on our live host objects.
            let completed = unsafe { self.fence.GetCompletedValue() };
            if completed >= target || completed == u64::MAX {
                return;
            }
            // SAFETY: plain device-removed query.
            if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                return;
            }
            if spins < 64 {
                spins += 1;
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        // Without it the queries fail as unsupported, which reads like a
        // missing GPU feature.
        if !dred_on() {
            return "; a diagnostics build with VICTORY_D3D12_DRED=1 records where the GPU stopped"
                .to_owned();
        }
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                        #[cfg(test)]
                        self.d3d12_errors.fetch_add(1, Ordering::Relaxed);
                        if first_error.is_none() {
                            first_error = Some(text.to_owned());
                        }
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource or a typed null view.
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    /// Keeps `blocks` (memory handed out for the recording) until the list
    /// finishes executing and is recycled or dropped.
    pub(crate) fn keep_uploads(&self, blocks: Vec<Vec<u8>>) {
        if !blocks.is_empty() {
            self.kept_uploads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(blocks);
        }
    }

    /// Keeps dynamic descriptor slots alive until this host list finishes
    /// executing and is recycled or dropped.
    pub(crate) fn keep_dynamic(&self, slots: Vec<(usize, u32, u32)>) {
        if !slots.is_empty() {
            self.kept_dynamic
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(slots);
        }
    }

    /// Frees kept upload blocks and returns kept dynamic descriptor slots.
    fn release_kept(&self) {
        self.kept_uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let dynamic =
            std::mem::take(&mut *self.kept_dynamic.lock().unwrap_or_else(|e| e.into_inner()));
        for (heap, first, n) in dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }

    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC {
                                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                                Count: STAMPS,
                                NodeMask: 0,
                            },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: (count + 1) * 8,
        };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (
                    ticks as f64 * 1e3 / frequency.max(1) as f64,
                    notes[i].0.chars().take(300).collect(),
                )
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    /// Track resolved buffers by resource identity, including aliasing title addresses.
    pub(crate) fn use_shadow(&self, buffer: &ID3D12Resource, wanted: D3D12_RESOURCE_STATES) {
        let mut states = self.shadow_states.lock().unwrap_or_else(|e| e.into_inner());
        let (_, state) = states
            .entry(buffer.as_raw() as usize)
            .or_insert_with(|| (buffer.clone(), D3D12_RESOURCE_STATE_COMMON));
        let before = *state;
        if let Some(after) = shadow_transition(before, wanted) {
            // SAFETY: this open list retains the resource and tracks its previous state.
            unsafe {
                self.list
                    .ResourceBarrier(&[transition(buffer, before, after)])
            };
            *state = after;
        } else if before == D3D12_RESOURCE_STATE_COMMON {
            *state = wanted;
        }
    }

    pub(crate) fn close(&self) {
        // Lists can execute together, in a different order, or more than once.
        // Buffers do not decay between lists in the same ExecuteCommandLists.
        // Restore a known entry state for the next recording.
        let states = self.shadow_states.lock().unwrap_or_else(|e| e.into_inner());
        if !states.is_empty() {
            let barriers: Vec<_> = states
                .values()
                .map(|(buffer, state)| transition(buffer, *state, D3D12_RESOURCE_STATE_COMMON))
                .collect();
            // SAFETY: an open list and resources retained until reset.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
        drop(states);
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count =
                (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list
                    .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    fn retire_state(&self) -> &Mutex<RetireState> {
        #[cfg(test)]
        if let Some(scope) = self.retire_scope.as_deref() {
            return scope;
        }
        &RETIRE_STATE
    }

    fn ensure_epoch(&self, _guard: &std::sync::MutexGuard<'_, ()>) {
        if self.epoch.load(Ordering::Acquire) == 0 {
            let e = acquire_epoch_in(self.retire_state());
            self.epoch.store(e, Ordering::Release);
        }
    }

    /// Pins a retirement epoch for this list once it records GPU work, so
    /// retired host resources remain alive until the list executes or resets.
    pub(crate) fn mark_work(&self) {
        if self.epoch.load(Ordering::Acquire) != 0 {
            return;
        }
        let guard = self.epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_epoch(&guard);
    }

    /// Pins an epoch and increments the in-flight submission count for this
    /// list before queuing it on a command queue timeline.
    pub(crate) fn begin_submit(&self) {
        let guard = self.epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.ensure_epoch(&guard);
    }

    /// Releases a queued submission pin and, if it was the last in-flight
    /// submission for this list, releases the list's retirement epoch.
    pub(crate) fn end_submit(&self) {
        let released = {
            let _guard = self.epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
            if self.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.epoch.swap(0, Ordering::AcqRel)
            } else {
                0
            }
        };
        if released != 0 {
            release_epoch_in(self.retire_state(), released);
        }
    }

    /// Releases a queued submission pin for a cancelled (unexecuted) submission
    /// without clearing the list's recording epoch: if the title still owns
    /// this closed list, its epoch stays pinned until it executes, resets, or
    /// drops; if the cancelled submission held the last reference, dropping
    /// the `HostList` immediately releases the epoch.
    pub(crate) fn cancel_submit(&self) {
        let _guard = self.epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn has_epoch(&self) -> bool {
        self.epoch.load(Ordering::Acquire) != 0
    }

    fn release_epoch(&self) {
        let released = {
            let _guard = self.epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
            if self.in_flight.load(Ordering::Acquire) == 0 {
                self.epoch.swap(0, Ordering::AcqRel)
            } else {
                0
            }
        };
        if released != 0 {
            release_epoch_in(self.retire_state(), released);
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        self.release_kept();
        self.shadow_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.deferred() = Deferred::default();
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.state_gen.store(0, Ordering::Relaxed);
        self.dirty_resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
        }
        {
            let mut rect = self.rect_scratch.lock().unwrap_or_else(|e| e.into_inner());
            (rect.counter_chunk, rect.counter_used) = (0, 0);
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        self.release_epoch();
    }

    /// The host state currently tracked for `resource` on this list, falling
    /// back to the resource's last submitted/initial host state if this list
    /// has not transitioned it yet.
    #[cfg(test)]
    pub(crate) fn tracked_state(&self, resource: crate::object::This) -> Option<u32> {
        let copy = crate::resource::host_copy(resource)?;
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&(_, last, _, _)) = states.get(&(std::sync::Arc::as_ptr(&copy) as usize)) {
            Some(last)
        } else {
            Some(copy.state.load(Ordering::Relaxed))
        }
    }

    /// The last state explicitly requested for `resource` on this list (via a
    /// title barrier, copy, clear, target bind, or non-bindless descriptor
    /// table), ignoring states chosen by bindless heap sweeps.
    pub(crate) fn explicit_state(&self, resource: crate::object::This) -> Option<u32> {
        let copy = crate::resource::host_copy(resource)?;
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states
            .get(&(std::sync::Arc::as_ptr(&copy) as usize))
            .and_then(|&(_, _, explicit, _)| explicit)
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        self.transition_inner(resource, wanted, true)
    }

    /// Same as [`HostList::transition`], for bindless heap sweeps: updates the
    /// tracked host state without overwriting the title's explicit access
    /// intent recorded by barriers or explicit binds.
    pub(crate) fn transition_bindless(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        self.transition_inner(resource, wanted, false)
    }

    fn transition_inner(
        &self,
        resource: crate::object::This,
        wanted: u32,
        explicit: bool,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        self.mark_work();
        let wanted = legal_state(texture, wanted);
        let (changed, result) = {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert((wanted, wanted, explicit.then_some(wanted), copy));
                    (true, None)
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let entry = o.get_mut();
                    let before = std::mem::replace(&mut entry.1, wanted);
                    if explicit {
                        entry.2 = Some(wanted);
                    }
                    if before != wanted {
                        (true, Some((texture, before)))
                    } else {
                        (false, None)
                    }
                }
            }
        };
        if changed {
            self.dirty_resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(resource as usize);
            self.state_gen.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// The current state established by this list. A texture the list has
    /// never used can arrive in a different state at submission.
    pub(crate) fn tracked_texture_state(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
    ) -> Option<u32> {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(std::sync::Arc::as_ptr(copy) as usize))
            .map(|&(_, last, _, _)| last)
    }

    /// Monotonic generation incremented whenever a texture is first tracked
    /// or changes state in this list.
    pub(crate) fn state_gen(&self) -> u64 {
        self.state_gen.load(Ordering::Relaxed)
    }

    /// Current cursor length of [`HostList::dirty_resources`].
    pub(crate) fn dirty_len(&self) -> usize {
        self.dirty_resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Returns `(new_cursor, resources)` of title texture pointers whose
    /// tracked state changed at or after `cursor`.
    pub(crate) fn dirty_since(&self, cursor: usize) -> (usize, Vec<usize>) {
        let dirty = self
            .dirty_resources
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let len = dirty.len();
        let slice = dirty.get(cursor..).unwrap_or(&[]);
        (len, slice.to_vec())
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with.
    fn prelude(&self) -> Option<ID3D12CommandList> {
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = states
            .values()
            .filter_map(|(first, last, _, copy)| {
                let real = copy.state.swap(*last, Ordering::Relaxed);
                (real != *first).then(|| {
                    transition(
                        &copy.texture,
                        D3D12_RESOURCE_STATES(real as i32),
                        D3D12_RESOURCE_STATES(*first as i32),
                    )
                })
            })
            .collect();
        if barriers.is_empty() {
            return None;
        }
        let (allocator, list) = &self.prelude;
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                list.ResourceBarrier(&barriers);
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if dred_on() {
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps()
                && at < STAMPS - 1
            {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list
                    .SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
        }
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        self.mark_work();
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to title memory at `dst` when the list
    /// is submitted (`Step::Copy` → `copy_mapped(..., title_dst = true)`).
    ///
    /// Title destinations keep the `VirtualQuery` commitment guard. Host-owned
    /// mapped upload memory must use [`Self::defer_fill`] instead: under
    /// Wine+vkd3d, `Map` pointers can report as `MEM_FREE` and the guard would
    /// skip the copy (#518).
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.mark_work();
        let mut deferred = self.deferred();
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
        deferred.copy_gen += 1;
    }

    /// Monotonic count of deferred title-memory copies recorded on this list
    /// since reset. Fresh-table dedupe partitions reuse by this generation so
    /// an intervening `CopyBufferRegion`/`CopyResource` creates a new CBV
    /// snapshot (`Fill -> Copy -> Fill`).
    pub(crate) fn copy_gen(&self) -> u64 {
        self.deferred().copy_gen
    }

    /// Copies `len` bytes of title memory at `src` into host-owned upload
    /// memory at `dst` when the list is submitted (`Step::Fill` →
    /// `copy_mapped(..., title_dst = false)`).
    ///
    /// Buffer→texture staging in `copy_texture_region` uses this; buffer→buffer
    /// title copies stay on [`Self::defer_copy`].
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.mark_work();
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.mark_work();
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.mark_work();
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.mark_work();
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// Copies a GPU texture readback from `src` into title memory at `dst`
    /// once the list has finished running on the host GPU.
    pub(crate) fn defer_readback(
        &self,
        dst: *mut u8,
        src: *const u8,
        dst_pitch: usize,
        src_pitch: usize,
        lines: usize,
        row_bytes: usize,
    ) {
        self.mark_work();
        self.deferred().readbacks.push(Readback {
            dst: dst as usize,
            src: src as usize,
            dst_pitch,
            src_pitch,
            lines,
            row_bytes,
        });
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        self.mark_work();
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// A mapped readback buffer of `size` bytes, alive until the list is
    /// reset.
    pub(crate) fn readback(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *const u8)> {
        let (buffer, data) = host.readback_buffer(size)?;
        self.mark_work();
        name(&buffer, || format!("list readback {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.mark_work();
            self.deferred().uploads.push(buffer);
            return Some((gpu, data));
        }
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        if s.used + len > SCRATCH_CHUNK && s.current < s.chunks.len() {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let (buffer, data) = host.upload_buffer(SCRATCH_CHUNK)?;
            name(&buffer, || "list scratch".into());
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            s.chunks.push((buffer, data as usize, gpu));
            s.used = 0;
        }
        let (_, data, gpu) = s.chunks[s.current];
        let at = s.used;
        s.used += len;
        drop(s);
        self.mark_work();
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        self.mark_work();
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            // SAFETY: the list's own mapped word, written before it runs.
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                (at as *mut u32).write_volatile(0)
            })));
        }
        // SAFETY: a live buffer.
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    /// The list's GPU-writable scratch buffer for expanded indirect
    /// rectangle vertices (`capacity` bytes), created on first use.
    pub(crate) fn rect_vertices(&self, capacity: u64) -> Option<ID3D12Resource> {
        let mut rect = self.rect_scratch.lock().unwrap_or_else(|e| e.into_inner());
        if rect.vertices.is_none() {
            let buffer = host()?.default_buffer(capacity)?;
            name(&buffer, || format!("rect scratch {capacity:#x}"));
            rect.vertices = Some(buffer);
        }
        let buffer = rect.vertices.clone()?;
        drop(rect);
        self.mark_work();
        Some(buffer)
    }

    /// A GPU-writable buffer of at least `bytes` for rewritten indirect
    /// draw arguments, kept alive until the list is reset.
    pub(crate) fn rect_args(&self, bytes: u64) -> Option<ID3D12Resource> {
        let want = bytes.max(RECT_ARGS_MIN).next_multiple_of(256);
        let mut rect = self.rect_scratch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buf, len)) = &rect.args
            && *len >= want
        {
            let buf = buf.clone();
            drop(rect);
            self.mark_work();
            return Some(buf);
        }
        let buffer = host()?.default_buffer(want)?;
        name(&buffer, || format!("rect args {want:#x}"));
        let old = rect.args.replace((buffer.clone(), want));
        drop(rect);
        self.mark_work();
        if let Some((old, _)) = old {
            self.keep(&old);
        }
        Some(buffer)
    }

    /// The host GPU address of a 4-byte atomic counter for one indirect
    /// rectangle draw, zeroed at each submission before the list runs.
    pub(crate) fn rect_counter(&self) -> Option<u64> {
        let mut rect = self.rect_scratch.lock().unwrap_or_else(|e| e.into_inner());
        if rect.counter_used == RECT_COUNTER_CHUNK {
            rect.counter_chunk += 1;
            rect.counter_used = 0;
        }
        if rect.counter_chunk == rect.counters.len() {
            let bytes = u64::from(RECT_COUNTER_CHUNK) * 4;
            let (buffer, data) = host()?.shared_buffer(bytes)?;
            name(&buffer, || "rect counters".into());
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            rect.counters.push((buffer, data as usize, gpu));
            rect.counter_used = 0;
        }
        let (_, data, gpu) = rect.counters[rect.counter_chunk];
        let slot = rect.counter_used;
        rect.counter_used += 1;
        drop(rect);
        self.mark_work();
        if slot == 0 {
            let bytes = RECT_COUNTER_CHUNK as usize * 4;
            // SAFETY: the list's own mapped chunk, zeroed before it runs.
            self.defer_host(move || unsafe {
                std::ptr::write_bytes(data as *mut u8, 0, bytes);
            });
        }
        Some(gpu + u64::from(slot) * 4)
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.mark_work();
            self.deferred().kept.push(object);
        }
    }

    /// Keeps `object` alive until the list is reset without pinning a
    /// retirement epoch (used when mirroring fixed-function target bindings
    /// onto a newly split trailing list before any GPU work is recorded).
    pub(crate) fn keep_unmarked(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    #[cfg(test)]
    pub(crate) fn kept_contains(&self, object: &impl Interface) -> bool {
        object.cast::<windows::core::IUnknown>().is_ok_and(|u| {
            let raw = u.as_raw();
            self.deferred().kept.iter().any(|k| k.as_raw() == raw)
        })
    }

    #[cfg(test)]
    fn defer_unmarked(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Run(Box::new(work)));
    }

    /// Whether this list already covers the requested mip and all smaller ones.
    pub(crate) fn covers_texture(&self, key: usize, first: u32) -> bool {
        self.deferred()
            .textures
            .get(&key)
            .is_some_and(|&seen| seen <= first)
    }

    /// Records coverage and reports whether the list gained any mip levels.
    pub(crate) fn use_texture(&self, key: usize, first: u32) -> bool {
        use std::collections::hash_map::Entry;
        match self.deferred().textures.entry(key) {
            Entry::Occupied(mut e) if first < *e.get() => {
                e.insert(first);
                true
            }
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(first);
                true
            }
        }
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.mark_work();
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Run(Box::new(work)));
        deferred.refreshed.clear();
        deferred.copy_gen += 1;
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.mark_work();
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, keeping it alive until the list is reset.
    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.mark_work();
        self.deferred().copies.push(Step::Texture(staging));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        for step in &self.deferred().copies {
            let mut started = std::time::Instant::now();
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    batch.title_written();
                    // The flush counted its own time; the copy starts here.
                    started = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    len
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                        #[cfg(feature = "diagnostics")]
                        {
                            started = std::time::Instant::now();
                        }
                    }
                    work();
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    batch.gpu_write(shadow, *address, *len);
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        staging.refresh();
                    }
                    continue;
                }
            };
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
    }
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them. A shader-write snapshot needs its own buffer
/// current. A title-memory write or GPU submission needs them all current.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    gpu_writes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    declared_writes: std::collections::HashSet<(usize, usize, usize)>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
}

impl Batch {
    fn new_gpu_write<T>(&mut self, shadow: &std::sync::Arc<T>, address: usize, len: usize) -> bool {
        self.declared_writes
            .insert((std::sync::Arc::as_ptr(shadow) as usize, address, len))
    }

    fn gpu_write(
        &mut self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        if self.new_gpu_write(shadow, address, len) {
            // The first declaration needs the refreshed bytes for its snapshot.
            // Another declaration of this exact range is already pending until
            // the GPU submission finishes, so it needs no refresh boundary.
            if let Some(index) = self
                .refreshes
                .iter()
                .position(|(copy, ..)| std::sync::Arc::ptr_eq(copy, shadow))
            {
                let (copy, from, to) = self.refreshes.remove(index);
                Self::refresh(&copy, from, to);
            }
            shadow.writing(address, len);
            self.gpu_writes.push((shadow.clone(), address, len));
        }
    }

    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        match self
            .refreshes
            .iter_mut()
            .find(|(s, ..)| std::sync::Arc::ptr_eq(s, shadow))
        {
            Some((_, from, to)) => (*from, *to) = ((*from).min(address), (*to).max(end)),
            None => self.refreshes.push((shadow.clone(), address, end)),
        }
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures
            .insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        for (shadow, from, to) in self.refreshes.drain(..) {
            Self::refresh(&shadow, from, to);
        }
    }

    fn refresh(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        if let Some(bytes) = crate::shadow::skipped(to - from, copied) {
            crate::copystat::add(crate::copystat::C::Skipped, 1);
            crate::copystat::add(crate::copystat::C::SkippedBytes, bytes);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
    }
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped takes nothing from its
/// first unmapped page on.
///
/// Host-owned staging uploads pass `title_dst = false` (`Step::Fill` /
/// [`HostList::defer_fill`]): those destinations are known mapped, and under
/// Wine+vkd3d `VirtualQuery` can report them as `MEM_FREE`, so the commitment
/// guard must not apply (#518). Title buffer destinations keep `title_dst =
/// true` (`Step::Copy` / [`HostList::defer_copy`]).
///
/// The copy runs in the kernel (`ReadProcessMemory`), which reports an
/// unmapped page of the source, or of the destination it writes, instead of
/// faulting. Asking `VirtualQuery` first cost about 2 ms per copy in the
/// title's big pools (it walks the page tables to the region's end), which
/// made copies run at 100-200 MB/s; a title destination is asked only once
/// its copy has failed (asked before each, the world's 80 copies per frame
/// cost 60 ms).
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::MoveUs,
        moved.elapsed().as_micros() as u64,
    );
    if whole {
        return;
    }
    // The copy stopped at an unmapped page. One of a title destination ends
    // it: the retries below write the destination themselves (zeros for
    // what cannot be read), and would fault there.
    if title_dst && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: zero is valid for the fields of this Windows output structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the query inspects an address and fills the local structure.
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize)
            .saturating_add(info.RegionSize)
            .saturating_sub(at as usize)
            .min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` in the kernel: false if a source
/// page is not readable or a destination page not writable (the bytes
/// before that page may have been copied).
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is `len` bytes the caller may write, host
    // memory or title memory; the kernel checks both ranges, and a page it
    // cannot read or write fails the call instead of faulting.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

/// Copies host-owned bytes into title memory without dereferencing a
/// potentially unmapped destination on a worker thread.
pub fn write_title_memory(destination: usize, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    crate::watch::kernel_will_write(destination, bytes.len());
    let written = read(bytes.as_ptr(), destination as *mut u8, bytes.len());
    crate::watch::kernel_wrote(destination, bytes.len());
    written
}

#[derive(Clone, Copy)]
enum CopyBack {
    WineRead,
    NativeWrite(NtWrite),
}

/// Select the checked same-process copy once. Proton's read path is much
/// cheaper for the small rows and pages written back after a submission.
fn copy_back_method() -> Option<CopyBack> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    static METHOD: std::sync::OnceLock<Option<CopyBack>> = std::sync::OnceLock::new();
    *METHOD.get_or_init(|| {
        // SAFETY: ntdll is always loaded. Wine identifies itself by this export;
        // on native Windows the write routine has the NtWrite signature.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            if GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some() {
                return Some(CopyBack::WineRead);
            }
            let f = GetProcAddress(ntdll, c"NtWriteVirtualMemory".as_ptr().cast())?;
            Some(CopyBack::NativeWrite(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                NtWrite,
            >(f)))
        }
    })
}

type NtWrite = unsafe extern "system" fn(
    process: isize,
    base: *mut core::ffi::c_void,
    buffer: *const core::ffi::c_void,
    len: usize,
    written: *mut usize,
) -> i32;

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
///
/// Wine uses the checked read into the destination. Native Windows uses
/// the NT write directly, avoiding the region query and temporary changes
/// to read-only page protections that WriteProcessMemory can perform.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let Some(method) = copy_back_method() else {
        return;
    };
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let CopyBack::NativeWrite(call) = method else {
            return read(from, at as *mut u8, n);
        };
        let mut done = 0;
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe {
            // STATUS_SUCCESS only: a partial copy is a failure here too.
            call(
                GetCurrentProcess() as isize,
                at as _,
                from.cast(),
                n,
                &mut done,
            ) == 0
        }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

#[cfg(test)]
mod title_copies {
    use super::{copy_mapped, write_mapped, write_title_memory};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const BLOCK: usize = 64 << 10;

    /// Three reserved blocks, the first and the last committed and filled
    /// with `fill`: title memory with an unmapped block in it.
    fn holed(fill: u8) -> *mut u8 {
        // SAFETY: a fresh reservation; the committed blocks are ours to fill.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 3 * BLOCK, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            for block in [0, 2] {
                let at = base.add(block * BLOCK);
                assert!(!VirtualAlloc(at.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
                at.write_bytes(fill, BLOCK);
            }
            base
        }
    }

    fn block<'a>(base: *const u8, n: usize) -> &'a [u8] {
        // SAFETY: tests ask for committed blocks only.
        unsafe { std::slice::from_raw_parts(base.add(n * BLOCK), BLOCK) }
    }

    fn release(base: *mut u8) {
        // SAFETY: a reservation of `holed`.
        assert_ne!(unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }, 0);
    }

    fn pattern() -> Vec<u8> {
        (0..3 * BLOCK).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(
                    VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                    at.cast()
                );
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(
                        VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old),
                        0
                    );
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(
                        VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                        at.cast()
                    );
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn title_copies_reach_mapped_destinations_and_stop_at_unmapped_pages() {
        let from = pattern();
        // All mapped: copied, through a destination that was never asked.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), BLOCK, true);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        release(to);
        // An unmapped block in the destination: nothing faults, and nothing
        // arrives from that block on.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), 3 * BLOCK, true);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
        // An unmapped block in the source reads as zeros.
        let holes = holed(7);
        let mut to = vec![0xeeu8; 3 * BLOCK];
        copy_mapped(to.as_mut_ptr(), holes, 3 * BLOCK, true);
        assert!(to[..BLOCK].iter().all(|&b| b == 7));
        assert!(to[BLOCK..2 * BLOCK].iter().all(|&b| b == 0));
        assert!(to[2 * BLOCK..].iter().all(|&b| b == 7));
        release(holes);
    }

    /// Host-owned staging (`title_dst = false`, `defer_fill`) still copies;
    /// title destinations (`title_dst = true`, `defer_copy`) keep the
    /// commitment guard. Wine+vkd3d can report Map memory as MEM_FREE, which
    /// is why buffer→texture staging must not use the title path (#518).
    #[test]
    fn host_staging_fill_copies_while_title_dst_keeps_the_commitment_guard() {
        let from = pattern();
        // Fill path: a heap buffer stands in for mapped upload memory.
        let mut staging = vec![0u8; BLOCK];
        copy_mapped(staging.as_mut_ptr(), from.as_ptr(), BLOCK, false);
        assert_eq!(&staging[..], &from[..BLOCK]);
        // Copy path: an unmapped hole in the title destination stops the
        // write; the trailing committed block stays untouched.
        let title = holed(0xee);
        copy_mapped(title, from.as_ptr(), 3 * BLOCK, true);
        assert!(block(title, 2).iter().all(|&b| b == 0xee));
        release(title);
    }

    // Run on both revisions with --ignored --nocapture to measure the active path.
    #[test]
    #[ignore = "local copy-back timing"]
    fn time_small_copy_backs() {
        const ROW: usize = 4096;
        let source = vec![0xf3; ROW * 1024];
        let mut dest = vec![0; source.len()];
        let mut samples = Vec::new();
        for round in 0..10 {
            dest.fill(0);
            let start = std::time::Instant::now();
            for offset in (0..source.len()).step_by(ROW) {
                write_mapped(dest[offset..].as_mut_ptr(), source[offset..].as_ptr(), ROW);
            }
            let elapsed = start.elapsed();
            assert_eq!(dest, source);
            if round >= 2 {
                samples.push(elapsed);
            }
        }
        samples.sort_unstable();
        eprintln!(
            "1024 x 4 KiB copy-backs, median {:?}",
            (samples[3] + samples[4]) / 2
        );
    }

    #[test]
    fn copy_back_writes_mapped_pages_only() {
        let from = pattern();
        let to = holed(0xee);
        write_mapped(to, from.as_ptr(), 3 * BLOCK);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        assert_eq!(block(to, 2), &from[2 * BLOCK..]);
        release(to);
        // Neither aligned nor whole: the bytes around the range stay.
        let to = holed(0xee);
        // SAFETY: inside the first block.
        write_mapped(unsafe { to.add(100) }, from.as_ptr(), BLOCK - 300);
        let first = block(to, 0);
        assert_eq!(first[100..BLOCK - 200], from[..BLOCK - 300]);
        let around = first[..100].iter().chain(&first[BLOCK - 200..]);
        assert!(around.into_iter().all(|&b| b == 0xee));
        // A page that is mapped but not writable is not written either
        // (guarded title blocks are opened before they are copied back).
        let mut old = 0;
        // SAFETY: a committed block of ours.
        let protected =
            unsafe { VirtualProtect(to.add(2 * BLOCK).cast(), BLOCK, PAGE_READONLY, &mut old) };
        assert_ne!(protected, 0);
        // SAFETY: the third block.
        write_mapped(unsafe { to.add(2 * BLOCK) }, from.as_ptr(), BLOCK);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
    }

    #[test]
    fn title_memory_write_reports_unmapped_destination_without_faulting() {
        let from = pattern();
        let to = holed(0xee);
        assert!(write_title_memory(to as usize, &from[..BLOCK]));
        assert_eq!(block(to, 0), &from[..BLOCK]);
        assert!(!write_title_memory(to as usize, &from));
        assert_eq!(block(to, 0), &from[..BLOCK]);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }

    /// A buffer in the readback heap, mapped for reading.
    pub(crate) fn readback_buffer(&self, size: u64) -> Option<(ID3D12Resource, *const u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a readback buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no readback buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

/// Luminance bounds of the host display when HDR (HDR10 / PQ) is active on
/// Windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrDisplayInfo {
    pub min_luminance: f32,
    pub max_luminance: f32,
    pub max_full_frame_luminance: f32,
}

/// Queries the host display outputs for an active HDR10 (`G2084_NONE_P2020`)
/// desktop mode. Returns `None` when HDR is disabled or unsupported.
pub fn display_hdr_info() -> Option<HdrDisplayInfo> {
    // SAFETY: plain DXGI factory, adapter and output enumeration.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = if let Some(host) = host() {
            let luid = host.device.GetAdapterLuid();
            factory
                .EnumAdapterByLuid::<IDXGIAdapter1>(luid)
                .or_else(|_| factory.EnumAdapters1(0))
                .ok()?
        } else {
            factory.EnumAdapters1(0).ok()?
        };
        let mut i = 0u32;
        while let Ok(output) = adapter.EnumOutputs(i) {
            i += 1;
            let Ok(output6) = output.cast::<IDXGIOutput6>() else {
                continue;
            };
            let Ok(desc) = output6.GetDesc1() else {
                continue;
            };
            if desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
                return Some(HdrDisplayInfo {
                    min_luminance: desc.MinLuminance,
                    max_luminance: desc.MaxLuminance,
                    max_full_frame_luminance: desc.MaxFullFrameLuminance,
                });
            }
        }
        None
    }
}

fn desired_color_space(format: DXGI_FORMAT, hdr_active: bool) -> DXGI_COLOR_SPACE_TYPE {
    if !hdr_active {
        return DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;
    }
    match format {
        DXGI_FORMAT_R10G10B10A2_UNORM => DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        DXGI_FORMAT_R16G16B16A16_FLOAT => DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
        _ => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    }
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    color_space: DXGI_COLOR_SPACE_TYPE,
    copy: HostList,
    hwnd: HWND,
    meter: Meter,
    last_present_issue: Option<HRESULT>,
}

/// A successful DXGI status may still mean nothing was presented.
fn present_result(status: HRESULT) -> windows::core::Result<bool> {
    status.ok()?;
    Ok(status != DXGI_STATUS_OCCLUDED)
}

impl Presenter {
    fn sync_color_space(&mut self, hdr_active: bool) {
        let wanted = desired_color_space(self.format, hdr_active);
        if self.color_space == wanted {
            return;
        }
        // SAFETY: `self.swap_chain` is a live `IDXGISwapChain3`.
        let supported = unsafe { self.swap_chain.CheckColorSpaceSupport(wanted) }.unwrap_or(0);
        if supported & (DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32) == 0 {
            tracing::debug!(
                color_space = wanted.0,
                supported,
                "swap chain color space unsupported on host output"
            );
            return;
        }
        // SAFETY: `wanted` is supported for presentation on `self.swap_chain`.
        match unsafe { self.swap_chain.SetColorSpace1(wanted) } {
            Ok(()) => {
                self.color_space = wanted;
                tracing::info!(
                    format = self.format.0,
                    color_space = wanted.0,
                    "swap chain color space set"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, color_space = wanted.0, "SetColorSpace1 failed");
            }
        }
    }
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    frame_times: crate::frame_times::FrameTimes,
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        #[cfg(feature = "diagnostics")]
        crate::profile::start();
        let now = std::time::Instant::now();
        Meter {
            frame_times: Default::default(),
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let now = std::time::Instant::now();
        if let Some(s) = self.frame_times.present(now) {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            tracing::info!(
                "present intervals: {} frames, mean {:.2} ms, median {:.2} ms, p95 {:.2} ms, \
                 longest {:.2} ms, {} over 100 ms, {} over 1 s",
                s.intervals,
                s.mean_ms,
                ms(s.median),
                ms(s.p95),
                ms(s.longest),
                s.over_100ms,
                s.over_1s
            );
        }
        let elapsed = now - self.since;
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] =
            [0, 1].map(|i| SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] =
            [0, 1].map(|i| COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames);
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let indirect = crate::list::take_indirect() as f64 / frames;
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect",
                making = crate::pipeline::making()
            );
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown or the diagnostic probe has no reading. Otherwise diagnostics
    /// builds report whether the sampled pixels were black.
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let mut retry = self.present_retry.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        // After a failure, wait for the backoff: one attempt per state, not
        // one per frame.
        if !retry.due(now) {
            return None;
        }
        if presenter.is_none() {
            match self.presenter(width, height, format) {
                Ok(p) => {
                    let attempts = retry.reset();
                    if attempts > 0 {
                        tracing::info!(attempts, "swap chain created after the failed attempts");
                    }
                    *presenter = Some(p);
                }
                Err((step, e)) => {
                    retry.failed(now);
                    if retry.attempts == 1 {
                        tracing::warn!(
                            error = %e,
                            step = step.name(),
                            width,
                            height,
                            format = format.0,
                            "no swap chain; the next attempt waits for the backoff"
                        );
                    } else {
                        tracing::debug!(
                            error = %e,
                            step = step.name(),
                            attempts = retry.attempts,
                            "no swap chain"
                        );
                    }
                    return None;
                }
            }
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    retry.reset();
                    (p.width, p.height, p.format) = (width, height, format);
                }
                Err(e) => {
                    retry.failed(now);
                    if retry.attempts == 1 {
                        tracing::warn!(
                            error = %e,
                            width,
                            height,
                            "swap chain not resized; the next attempt waits for the backoff"
                        );
                    } else {
                        tracing::debug!(
                            error = %e,
                            attempts = retry.attempts,
                            "swap chain not resized"
                        );
                    }
                    return None;
                }
            }
        }
        if matches!(
            format,
            DXGI_FORMAT_R10G10B10A2_UNORM | DXGI_FORMAT_R16G16B16A16_FLOAT
        ) {
            p.sync_color_space(display_hdr_info().is_some());
        } else {
            p.sync_color_space(false);
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            p.copy.reset();
            let list = &p.copy.list;
            list.ResourceBarrier(&[
                transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            // Diagnostics: `VICTORY_SHOW_TARGET` shows one of the title's render targets instead.
            #[cfg(feature = "diagnostics")]
            let shown = crate::capture::show(list, &back);
            #[cfg(not(feature = "diagnostics"))]
            let shown = false;
            if !shown {
                list.CopyResource(&back, texture);
            }
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = crate::capture::probe(&self.device, list, texture);
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(&[
                transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ),
            ]);
            p.copy.close();
            self.execute(&[&p.copy]);
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let status = p.swap_chain.Present(0, DXGI_PRESENT(0));
            match present_result(status) {
                Ok(false) => {
                    if p.last_present_issue.replace(status) != Some(status) {
                        tracing::info!(frame, "DXGI reports the window is occluded");
                    }
                    return None;
                }
                Ok(true) => {
                    if p.last_present_issue.take().is_some() {
                        p.meter.frame_times.interrupted();
                        tracing::info!(frame, "DXGI resumed presenting frames");
                    }
                }
                Err(error) => {
                    if self.device.GetDeviceRemovedReason().is_err() {
                        fail(format_args!(
                            "host GPU device removed during Present: {error}{}{}",
                            self.removal_data(&[&p.copy]),
                            last_notes(&[&p.copy])
                        ));
                    }
                    if p.last_present_issue.replace(status) != Some(status) {
                        tracing::warn!(%error, frame, "swap chain present failed");
                    }
                    return None;
                }
            }
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return probe.and_then(|b| crate::capture::black(&b));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Presenter, (PresentStep, windows::core::Error)> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height).map_err(|e| (PresentStep::Window, e))?,
        };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(|e| (PresentStep::Factory, e))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)
                .map_err(|e| (PresentStep::SwapChain, e))?
                .cast::<IDXGISwapChain3>()
                .map_err(|e| (PresentStep::SwapChain, e))?;
            let copy = self.new_list().map_err(|e| (PresentStep::List, e))?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            let mut p = Presenter {
                swap_chain,
                width,
                height,
                format,
                color_space: DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                copy,
                hwnd,
                meter: Meter::new(),
                last_present_issue: None,
            };
            p.sync_color_space(display_hdr_info().is_some());
            Ok(p)
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// Whether the host may make render target views of `texture`. A texture
/// the host refused as a render target is made without that use (see
/// `resource::host_texture`; NVIDIA refuses B4G4R4A4), and a render target
/// view of it is an invalid call that removes the device (0x887A0001).
pub(crate) fn renders_to(texture: &ID3D12Resource) -> bool {
    // SAFETY: a live resource.
    let flags = unsafe { texture.GetDesc() }.Flags;
    flags.0 & D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 != 0
}

/// A typed null render target view for a slot whose texture the host
/// cannot render to: draws to it are discarded and the other slots keep
/// their place.
pub(crate) fn null_target() -> D3D12_RENDER_TARGET_VIEW_DESC {
    D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        ..Default::default()
    }
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    /// A GPU-only default-heap buffer that shaders can write (unordered
    /// access). Starts in the common state.
    pub(crate) fn default_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a default-heap buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no default buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!(
                    "; list {i} of {}, of its {}: {n}",
                    runs.len(),
                    notes.len()
                ));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(
    history: &[D3D12_AUTO_BREADCRUMB_OP],
    markers: &[(u32, String)],
    done: usize,
) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done]
        .iter()
        .rposition(barrier)
        .map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    for i in from..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn adapter_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

fn adapter_matches(name: &str, filter: &str) -> bool {
    name.to_ascii_lowercase()
        .contains(&filter.trim().to_ascii_lowercase())
}

/// Picks the adapter that runs the title: the first, in high-performance order (so a
/// hybrid laptop's discrete GPU comes before its integrated one), with shader model 6.6,
/// resource binding tier 3 and mesh shaders; when none has all three, the one that runs
/// most of the title, with a warning naming what each adapter lacks (`crate::adapter`).
/// When `filter` is given (`VICTORY_ADAPTER`), only adapters whose description contains
/// it (case-insensitively) take part, software ones included.
fn pick_hardware_adapter(
    filter: Option<&str>,
) -> windows::core::Result<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> {
    // SAFETY: plain DXGI enumeration and D3D12 capability probes.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let factory6 = factory.cast::<IDXGIFactory6>().ok();
        let mut found: Vec<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> = Vec::new();
        let mut candidates: Vec<crate::adapter::Candidate> = Vec::new();
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match &factory6 {
                Some(f6) => {
                    match f6.EnumAdapterByGpuPreference(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) {
                        Ok(a) => a,
                        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(e) => return Err(e),
                    }
                }
                None => match factory.EnumAdapters1(i) {
                    Ok(a) => a,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e),
                },
            };
            i += 1;
            let desc = adapter.GetDesc1()?;
            let name = adapter_name(&desc.Description);
            let is_sw = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            if let Some(needle) = filter {
                if !adapter_matches(&name, needle) {
                    continue;
                }
            } else if is_sw {
                continue;
            }
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                tracing::info!(adapter = %name, "adapter skipped: no D3D12 feature level 12_0");
                continue;
            }
            let Some(device) = device else { continue };
            let caps = adapter_caps(&device);
            #[cfg(feature = "diagnostics")]
            tracing::debug!(adapter = %name, ?caps, "adapter candidate");
            candidates.push(crate::adapter::Candidate { name, caps });
            found.push((adapter, desc));
        }
        let Some(choice) = crate::adapter::choose(&candidates) else {
            return Err(windows::core::Error::from_hresult(DXGI_ERROR_NOT_FOUND));
        };
        if let Some(s) = crate::adapter::shortfall(&candidates, &choice) {
            tracing::warn!(
                adapter = %s.chosen,
                lacks = %s.lacks,
                adapters = %s.adapters,
                "no adapter has everything the title needs; using the one that runs most"
            );
        }
        Ok(found.swap_remove(choice.index))
    }
}

/// What `device` offers of the capabilities the title needs.
fn adapter_caps(device: &ID3D12Device) -> crate::adapter::Caps {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    let mut options7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    // SAFETY: each query writes its own structure of the given size; a failed query leaves
    // the defaults, which read as "not supported".
    let (options_ok, options7_ok, model_ok) = unsafe {
        (
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok(),
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS7,
                    (&raw mut options7).cast(),
                    size_of_val(&options7) as u32,
                )
                .is_ok(),
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_SHADER_MODEL,
                    (&raw mut model).cast(),
                    size_of_val(&model) as u32,
                )
                .is_ok(),
        )
    };
    crate::adapter::Caps {
        shader_model_6_6: model_ok && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0,
        binding_tier_3: options_ok
            && options.ResourceBindingTier.0 >= D3D12_RESOURCE_BINDING_TIER_3.0,
        mesh_shaders: options7_ok
            && options7.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED,
    }
}

#[cfg(test)]
pub(crate) struct DropCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl DropCounter {
    pub(crate) fn new() -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (Self(count.clone()), count)
    }
}

#[cfg(test)]
impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_result_distinguishes_occlusion_from_success_and_failure() {
        assert!(present_result(HRESULT(0)).unwrap());
        assert!(!present_result(DXGI_STATUS_OCCLUDED).unwrap());
        assert_eq!(
            present_result(DXGI_ERROR_DEVICE_REMOVED)
                .unwrap_err()
                .code(),
            DXGI_ERROR_DEVICE_REMOVED
        );
    }

    #[test]
    fn shadow_buffer_transitions_between_writes_and_reads() {
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let srv = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let index = D3D12_RESOURCE_STATE_INDEX_BUFFER;
        assert_eq!(shadow_transition(D3D12_RESOURCE_STATE_COMMON, uav), None);
        assert_eq!(shadow_transition(D3D12_RESOURCE_STATE_COMMON, srv), None);
        assert_eq!(shadow_transition(uav, srv), Some(srv));
        assert_eq!(shadow_transition(srv, index), Some(srv | index));
        assert_eq!(shadow_transition(srv | index, srv), None);
        assert_eq!(shadow_transition(srv | index, uav), Some(uav));
        assert_eq!(shadow_transition(uav, uav), None);
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: a live adapter the factory just returned.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }

    #[test]
    fn adapter_name_stops_at_nul() {
        let raw = [b'R' as u16, b'T' as u16, b'X' as u16, 0, b'X' as u16];
        assert_eq!(adapter_name(&raw), "RTX");
        assert_eq!(adapter_name(&[b'X' as u16, b'e' as u16]), "Xe");
    }

    #[test]
    fn adapter_filter_matches_case_insensitively() {
        assert!(adapter_matches("NVIDIA GeForce RTX 3070 Ti", "rtx 3070"));
        assert!(adapter_matches("Intel(R) Iris(R) Xe Graphics", "  INTEL "));
        assert!(!adapter_matches("Intel(R) Iris(R) Xe Graphics", "RTX"));
    }

    #[test]
    fn unknown_adapter_filter_fails_instead_of_falling_back() {
        let err = pick_hardware_adapter(Some("__victory_no_such_adapter__")).unwrap_err();
        assert_eq!(err.code(), DXGI_ERROR_NOT_FOUND);
    }

    #[test]
    fn default_adapter_is_hardware_when_present() {
        if let Ok((_, desc)) = pick_hardware_adapter(None) {
            assert_eq!(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32, 0);
        }
    }

    #[test]
    fn only_textures_made_for_rendering_take_render_target_views() {
        let Some(host) = host() else { return };
        let make = |flags: D3D12_RESOURCE_FLAGS| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 4,
                Height: 4,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: a full description and our out pointer.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut texture,
                    )
                    .unwrap();
            }
            texture.unwrap()
        };
        assert!(renders_to(&make(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET)));
        assert!(!renders_to(&make(D3D12_RESOURCE_FLAG_NONE)));
        // The view bound in such a texture's place is a legal one.
        host.rtv(None, Some(&null_target()));
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn retire_drops_immediately_when_no_epoch_is_active() {
        let _retire = isolated_retire_scope();
        let (counter, drops) = DropCounter::new();
        retire(counter);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn idle_list_does_not_block_retirement() {
        let _retire = isolated_retire_scope();
        let Some(host) = host() else {
            return;
        };
        let idle = host.new_list().expect("host list");
        let (counter, drops) = DropCounter::new();
        retire(counter);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(idle);
    }

    #[test]
    fn open_marked_list_pins_retired_objects_across_other_executions() {
        let _retire = isolated_retire_scope();
        let Some(host) = host() else {
            return;
        };
        let open = host.new_list().expect("open list");
        open.mark_work();

        let (c1, d1) = DropCounter::new();
        retire(c1);
        assert_eq!(d1.load(Ordering::SeqCst), 0);

        // Submitting and executing a DIFFERENT list must not drop objects
        // retired while `open` is still active.
        let other = host.new_list().expect("other list");
        other.mark_work();
        other.close();
        host.execute(&[&other]);
        other.reset();
        drop(other);
        assert_eq!(
            d1.load(Ordering::SeqCst),
            0,
            "executing another list must not release an open list's epoch"
        );

        // Closing and resetting the open list releases its epoch.
        open.close();
        open.reset();
        assert_eq!(d1.load(Ordering::SeqCst), 1);

        // Marking work again and dropping the list also releases its epoch.
        open.mark_work();
        let (c2, d2) = DropCounter::new();
        retire(c2);
        assert_eq!(d2.load(Ordering::SeqCst), 0);
        drop(open);
        assert_eq!(d2.load(Ordering::SeqCst), 1);

        // A closed list pinned via begin_submit stays active across execution
        // until end_submit.
        let submitted = host.new_list().expect("submitted list");
        submitted.mark_work();
        submitted.begin_submit();
        let (c3, d3) = DropCounter::new();
        retire(c3);
        submitted.close();
        host.execute(&[&submitted]);
        assert_eq!(d3.load(Ordering::SeqCst), 0);
        submitted.end_submit();
        assert_eq!(d3.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_execute_does_not_submit() {
        // D3D12 forbids ExecuteCommandLists with NumCommandLists == 0.
        let Some(host) = host() else {
            return;
        };
        let before = *host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        host.execute(&[]);
        let after = *host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(before, after, "empty execute must not bump the fence");
    }

    #[test]
    fn active_execution_epoch_pins_objects_retired_during_execute() {
        let _retire = isolated_retire_scope();
        let Some(host) = host() else {
            return;
        };
        let list = host.new_list().expect("host list");
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops_during = drops.clone();
        // Use an unmarked deferred step without `begin_submit()` so `list`
        // itself pins no epoch; only `Host::execute`'s `exec_epoch` keeps the
        // object alive until execution finishes.
        list.defer_unmarked(move || {
            let counter = DropCounter(drops_during.clone());
            retire(counter);
            assert_eq!(drops_during.load(Ordering::SeqCst), 0);
        });
        assert!(!list.has_epoch());
        list.close();
        host.execute(&[&list]);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn state_gen_bumps_only_on_new_or_changed_texture_states() {
        let Some(host) = host() else {
            return;
        };
        let list = host.new_list().expect("host list");
        assert_eq!(list.state_gen(), 0);
        let null = std::ptr::null();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let iid = ID3D12Resource::IID;
        let mut tex = null;
        assert_eq!(
            crate::resource::create_committed(
                null,
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                0,
                null.cast(),
                (&raw const iid).cast(),
                &mut tex,
            ),
            crate::object::S_OK
        );

        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        const COPY_SOURCE: u32 = 0x800;
        assert!(list.transition(tex, SHADER_RESOURCE).is_none());
        assert_eq!(list.state_gen(), 1);
        // Same state does not bump state_gen.
        assert!(list.transition(tex, SHADER_RESOURCE).is_none());
        assert_eq!(list.state_gen(), 1);
        // Changing state bumps state_gen and returns the prior state.
        assert!(list.transition(tex, COPY_SOURCE).is_some());
        assert_eq!(list.state_gen(), 2);

        list.close();
        list.reset();
        assert_eq!(list.state_gen(), 0);
        list.close();

        let release =
            crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(tex, 0x10);
        assert_eq!(release(tex), 0);
    }
}

#[cfg(test)]
mod batch {
    // Batch::texture_due lives here; `mod tests` is already taken above
    use super::Batch;
    use std::sync::Arc;

    // Each pass reads the same 4 MiB and declares a distinct 16-byte output.
    // The second workload adds a title-write boundary between passes.
    // Sample columns are title-write boundary, batch microseconds, bytes refreshed.
    #[test]
    #[ignore = "measures submission-time buffer refreshes"]
    fn benchmark_shader_write_refreshes() {
        assert!(super::host().is_some(), "benchmark needs a D3D12 adapter");
        let _scope = super::isolated_retire_scope();
        let source = vec![23u8; 4 << 20];
        let output = vec![35u8; 4096];
        let src = source.as_ptr() as usize;
        let dst = output.as_ptr() as usize;
        crate::shadow::register(src, src as u64, source.len() as u64);
        crate::shadow::register(dst, dst as u64, output.len() as u64);
        let read = crate::shadow::find(src as u64, source.len() as u64).unwrap();
        let write = crate::shadow::find(dst as u64, output.len() as u64).unwrap();
        for barrier in [false, true] {
            for sample in 0..18 {
                let mut batch = Batch::default();
                let before = super::PREPARING[1].load(super::Ordering::Relaxed);
                let clock = std::time::Instant::now();
                for pass in 0..12 {
                    batch.add(&read, src, source.len());
                    let at = dst + pass * 16;
                    batch.add(&write, at, 16);
                    batch.gpu_write(&write, at, 16);
                    if barrier {
                        batch.title_written();
                    }
                }
                batch.flush();
                for (copy, at, len) in batch.gpu_writes.drain(..) {
                    assert_eq!(copy.copy_back(at, len), 0);
                }
                let micros = clock.elapsed().as_micros();
                let bytes = super::PREPARING[1].load(super::Ordering::Relaxed) - before;
                assert_eq!(read.bytes(src as u64, 1).unwrap(), [23]);
                if sample >= 2 {
                    println!("refresh_sample {barrier} {micros} {bytes}");
                }
            }
        }
        crate::shadow::unregister(dst, dst as u64);
        crate::shadow::unregister(src, src as u64);
    }

    #[test]
    fn shader_write_updates_its_own_copy_before_the_snapshot() {
        if super::host().is_none() {
            return;
        }
        let _scope = super::isolated_retire_scope();
        let mut data = vec![31u8; 4096];
        let start = data.as_ptr() as usize;
        let inner = start + 128;
        crate::shadow::register(start, start as u64, data.len() as u64);
        crate::shadow::register(inner, inner as u64, 256);
        let wide = crate::shadow::find(start as u64, data.len() as u64).unwrap();
        let narrow = crate::shadow::find(inner as u64, 256).unwrap();
        assert!(!Arc::ptr_eq(&wide, &narrow));
        wide.update(start, data.len());
        narrow.update(inner, 256);
        data[128] = 42;

        let mut batch = Batch::default();
        batch.add(&wide, start, data.len());
        batch.add(&narrow, inner, 256);
        batch.gpu_write(&narrow, inner, 256);
        assert_eq!(narrow.bytes(inner as u64, 1).unwrap(), [42]);
        assert_eq!(wide.bytes(inner as u64, 1).unwrap(), [31]);

        // A title write first flushes the other copy, even when it aliases
        // the snapshotted buffer in title memory.
        batch.title_written();
        assert_eq!(wide.bytes(inner as u64, 1).unwrap(), [42]);
        data[128] = 53;
        batch.add(&wide, start, data.len());
        batch.add(&narrow, inner, 256);
        batch.flush();
        assert_eq!(wide.bytes(inner as u64, 1).unwrap(), [53]);
        assert_eq!(narrow.bytes(inner as u64, 1).unwrap(), [42]);
        assert_eq!(narrow.copy_back(inner, 256), 0);
        assert_eq!(data[128], 53);
        crate::shadow::unregister(inner, inner as u64);
        crate::shadow::unregister(start, start as u64);
    }

    #[test]
    fn exact_gpu_writes_are_declared_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new(0), Arc::new(0));
        assert!(batch.new_gpu_write(&a, 4096, 16));
        assert!(!batch.new_gpu_write(&a.clone(), 4096, 16));
        assert!(batch.new_gpu_write(&a, 4096, 32));
        assert!(batch.new_gpu_write(&a, 4104, 16));
        assert!(batch.new_gpu_write(&b, 4096, 16));
        // CPU copies can flush refreshes, but these GPU writes stay pending.
        batch.title_written();
        assert!(!batch.new_gpu_write(&a, 4096, 16));
        assert!(Batch::default().new_gpu_write(&a, 4096, 16));
    }

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod eager_gbv_tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn parse_eager_gbv_respects_off_current_and_guarded_values() {
        assert_eq!(parse_eager_gbv(None), None);
        assert_eq!(parse_eager_gbv(Some(OsStr::new(""))), None);
        assert_eq!(parse_eager_gbv(Some(OsStr::new("0"))), None);
        assert_eq!(
            parse_eager_gbv(Some(OsStr::new("1"))),
            Some(EagerGbv::Current)
        );
        assert_eq!(
            parse_eager_gbv(Some(OsStr::new("guarded"))),
            Some(EagerGbv::Guarded)
        );
    }

    #[test]
    fn front_load_flag_matches_patch_mode() {
        assert_eq!(
            front_load_flag_for(
                D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_GUARDED_VALIDATION
            ),
            Ok(
                D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_GUARDED_VALIDATION_SHADERS
            )
        );
        assert_eq!(
            front_load_flag_for(
                D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_UNGUARDED_VALIDATION
            ),
            Ok(
                D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_UNGUARDED_VALIDATION_SHADERS
            )
        );
        assert_eq!(
            front_load_flag_for(
                D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_STATE_TRACKING_ONLY
            ),
            Ok(
                D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_TRACKING_ONLY_SHADERS
            )
        );
    }

    #[test]
    #[ignore = "requires an isolated process with the D3D12 debug layer and a WARP software adapter"]
    fn eager_gbv_round_trips_and_preserves_unrelated_flags_on_warp() -> windows::core::Result<()> {
        // SAFETY: runs only in an isolated test process with debug layer and WARP.
        unsafe {
            let mut layer: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut layer)?;
            let layer = layer.ok_or_else(windows::core::Error::empty)?;
            layer.EnableDebugLayer();
            layer
                .cast::<ID3D12Debug1>()?
                .SetEnableGPUBasedValidation(true);
            let adapter = warp_adapter()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            let device = device.ok_or_else(windows::core::Error::empty)?;

            let debug = device.cast::<ID3D12DebugDevice1>()?;
            let kind = D3D12_DEBUG_DEVICE_PARAMETER_GPU_BASED_VALIDATION_SETTINGS;
            let size = size_of::<D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS>() as u32;
            let mut settings = D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS::default();
            debug.GetDebugParameter(kind, (&raw mut settings).cast(), size)?;
            // Turn on an unrelated create flag (`FRONT_LOAD_CREATE_TRACKING_ONLY_SHADERS`)
            // and verify eager configuration preserves it alongside the added front-load bit.
            let preserved =
                D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_TRACKING_ONLY_SHADERS;
            settings.PipelineStateCreateFlags.0 |= preserved.0;
            debug.SetDebugParameter(kind, (&raw const settings).cast(), size)?;

            configure_eager_gbv(&device, EagerGbv::Current)?;
            let mut after_current = D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS::default();
            debug.GetDebugParameter(kind, (&raw mut after_current).cast(), size)?;
            assert_ne!(after_current.PipelineStateCreateFlags.0 & preserved.0, 0);

            configure_eager_gbv(&device, EagerGbv::Guarded)?;
            let mut after_guarded = D3D12_DEBUG_DEVICE_GPU_BASED_VALIDATION_SETTINGS::default();
            debug.GetDebugParameter(kind, (&raw mut after_guarded).cast(), size)?;
            assert_eq!(
                after_guarded.DefaultShaderPatchMode,
                D3D12_GPU_BASED_VALIDATION_SHADER_PATCH_MODE_GUARDED_VALIDATION
            );
            assert_ne!(
                after_guarded.PipelineStateCreateFlags.0
                    & D3D12_GPU_BASED_VALIDATION_PIPELINE_STATE_CREATE_FLAG_FRONT_LOAD_CREATE_GUARDED_VALIDATION_SHADERS
                        .0,
                0
            );
            assert_ne!(after_guarded.PipelineStateCreateFlags.0 & preserved.0, 0);
            Ok(())
        }
    }
}

fn wave_widths(wave_ops: bool, min: u32, max: u32) -> victory_shader::emit::ComputeWaves {
    let range = min..=max;
    victory_shader::emit::ComputeWaves {
        wave32: wave_ops && range.contains(&32),
        narrow_quads: wave_ops && (4..64).contains(&max) && (4..=max).contains(&min),
    }
}

#[cfg(test)]
mod wave_tests {
    #[test]
    fn supported_widths_match_adapter_lane_range() {
        for (ops, min, max, expected) in [
            (true, 32, 32, (true, true)),
            (true, 32, 64, (true, false)),
            (true, 16, 32, (true, true)),
            (true, 8, 8, (false, true)),
            (true, 4, 128, (true, false)),
            (true, 64, 64, (false, false)),
            (false, 32, 32, (false, false)),
        ] {
            let widths = super::wave_widths(ops, min, max);
            assert_eq!((widths.wave32, widths.narrow_quads), expected);
        }
    }
}

#[cfg(test)]
mod title_memory_io {
    use super::{copy_mapped, write_mapped};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const PAGE: usize = 4096;

    /// Two committed pages followed by one reserved (not committed) page.
    fn region() -> *mut u8 {
        // SAFETY: a fresh reservation of ours, of which two pages are committed.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE, PAGE_READWRITE);
            assert!(!base.is_null());
            let committed = VirtualAlloc(base, 2 * PAGE, MEM_COMMIT, PAGE_READWRITE);
            assert!(!committed.is_null());
            base.cast()
        }
    }

    fn release(base: *mut u8) {
        // SAFETY: the reservation made by `region`.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }

    #[test]
    fn write_mapped_writes_mapped_pages_and_skips_the_rest() {
        let base = region();
        let data = vec![0xabu8; 3 * PAGE];
        // The third page is not committed: its piece is skipped, the others land.
        write_mapped(base, data.as_ptr(), 3 * PAGE);
        // SAFETY: the two committed pages of `region`.
        let written = unsafe { std::slice::from_raw_parts(base, 2 * PAGE) };
        assert!(written.iter().all(|&b| b == 0xab));
        release(base);
    }

    #[test]
    fn copy_into_title_memory_that_is_not_all_mapped_is_left_alone() {
        let base = region();
        let from = vec![0x5au8; 3 * PAGE];
        copy_mapped(base, from.as_ptr(), 3 * PAGE, true);
        // A whole copy into mapped memory works without asking the system first.
        copy_mapped(base, from.as_ptr(), 2 * PAGE, true);
        // SAFETY: the two committed pages of `region`.
        let now = unsafe { std::slice::from_raw_parts(base, 2 * PAGE) };
        assert!(now.iter().all(|&b| b == 0x5a));
        release(base);
    }
}

#[cfg(test)]
mod present_retry {
    use super::{PresentStep, Retry, retry_delay};
    use std::time::{Duration, Instant};

    #[test]
    fn the_backoff_starts_short_and_stops_at_a_second() {
        let schedule: Vec<u64> = (1..=7).map(|n| retry_delay(n).as_millis() as u64).collect();
        assert_eq!(schedule, [100, 200, 400, 800, 1_000, 1_000, 1_000]);
    }

    #[test]
    fn a_failed_attempt_waits_before_the_next_one() {
        let start = Instant::now();
        let mut retry = Retry::new();
        // Nothing has failed, so an attempt is due.
        assert!(retry.due(start));
        retry.failed(start);
        assert_eq!(retry.attempts, 1);
        assert!(!retry.due(start));
        assert!(!retry.due(start + Duration::from_millis(99)));
        assert!(retry.due(start + Duration::from_millis(100)));
        // A second failure doubles the wait and counts.
        let second = start + Duration::from_millis(100);
        retry.failed(second);
        assert_eq!(retry.attempts, 2);
        assert!(!retry.due(second + Duration::from_millis(199)));
        assert!(retry.due(second + Duration::from_millis(200)));
    }

    #[test]
    fn a_working_attempt_clears_the_state_and_reports_the_failures() {
        let start = Instant::now();
        let mut retry = Retry::new();
        for _ in 0..3 {
            retry.failed(start);
        }
        assert_eq!(retry.attempts, 3);
        assert_eq!(retry.reset(), 3);
        assert_eq!(retry.attempts, 0);
        assert!(retry.due(start));
        // A later failure starts the schedule over.
        retry.failed(start);
        assert!(!retry.due(start + Duration::from_millis(99)));
    }

    /// A minute behind a desktop that never appears: one attempt per backoff
    /// instead of one per presented frame.
    #[test]
    fn a_minute_without_a_desktop_asks_a_bounded_number_of_times() {
        let mut retry = Retry::new();
        let mut now = Instant::now();
        let mut attempts = 0u32;
        for _ in 0..1_200 {
            // 60 s at 20 fps.
            if retry.due(now) {
                attempts += 1;
                retry.failed(now);
            }
            now += Duration::from_millis(50);
        }
        assert!(
            (59..=64).contains(&attempts),
            "{attempts} attempts in a minute, one per backoff expected"
        );
    }

    #[test]
    fn every_step_of_a_presenter_has_a_name() {
        for step in [
            PresentStep::Window,
            PresentStep::Factory,
            PresentStep::SwapChain,
            PresentStep::List,
        ] {
            assert!(!step.name().is_empty());
        }
    }
}
