//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! The library is free-threaded (`CreatePipelineLibrary`, "Thread Safety"),
//! so calls do not share one lock. Loads and stores of the same name are
//! serialised by one of `STRIPES` mutexes chosen by name; other names run in
//! parallel. Stores also hold `SERIALISING` shared and a save holds it
//! exclusively, so the size cannot change between `GetSerializedSize` and
//! `Serialize`; loads never touch it, so a save never blocks them.

use std::collections::HashSet;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock, TryLockError};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Vec<u8>,
    /// Pipelines stored since the last save.
    added: AtomicU64,
}

// SAFETY: the library is free-threaded; the only calls that must not
// overlap (two of the same name, a store and a serialise) are kept apart by
// `STRIPES` and `SERIALISING`.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

/// Number of name-keyed mutexes over loads and stores.
const STRIPES: usize = 64;
static STRIPE_LOCKS: [Mutex<()>; STRIPES] = [const { Mutex::new(()) }; STRIPES];
/// Held shared by stores, exclusively over serialising.
static SERIALISING: RwLock<()> = RwLock::new(());

/// The stripe of `name`: the same name always gives the same stripe.
fn stripe(name: &str) -> &'static Mutex<()> {
    let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(name);
    &STRIPE_LOCKS[(hash % STRIPES as u64) as usize]
}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR
                .get()
                .and_then(|d| std::fs::read(d.join(LIBRARY_FILE)).ok())
                .unwrap_or_default();
            let library = open(&device, blob);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Vec<u8>) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device
                .CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?
                .cast()
        }
    };
    if !blob.is_empty() {
        match make(&blob) {
            Ok(library) => {
                tracing::info!(bytes = blob.len(), "host pipeline library loaded");
                return Some(Library {
                    library,
                    _blob: blob,
                    added: AtomicU64::new(0),
                });
            }
            Err(e) => {
                tracing::info!(error = %e, "host pipeline library rejected; starting an empty one")
            }
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    // An empty library is asked for with no blob at all, which `make` cannot
    // say: an empty slice still has an address, and vkd3d-proton refuses an
    // address with no bytes behind it. That left it with no library at all.
    let mut raw = std::ptr::null_mut();
    // SAFETY: no blob is read; a success that wrote a pointer wrote an owned
    // ID3D12PipelineLibrary.
    let made = unsafe {
        (Interface::vtable(device).CreatePipelineLibrary)(
            Interface::as_raw(device),
            std::ptr::null(),
            0,
            &ID3D12PipelineLibrary::IID,
            &mut raw,
        )
        .ok()
        .and_then(|()| {
            if raw.is_null() {
                return Err(windows::Win32::Foundation::E_POINTER.into());
            }
            ID3D12PipelineLibrary::from_raw(raw).cast::<ID3D12PipelineLibrary1>()
        })
    };
    match made {
        Ok(library) => Some(Library {
            library,
            _blob: Vec::new(),
            added: AtomicU64::new(0),
        }),
        Err(e) => {
            tracing::info!(error = %e, "the host refused an empty pipeline library");
            None
        }
    }
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root]
        .into_iter()
        .chain(code.iter().copied())
        .chain([state.as_bytes()])
    {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// Exact library names from the host pipeline trace. Names describe both
/// translated stages and fixed-function state, so a shader shared with a
/// different pipeline can still be used there.
#[cfg(feature = "diagnostics")]
fn excluded_names(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter_map(|id| {
            if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(id.to_ascii_lowercase())
            } else {
                tracing::warn!(id, "VICTORY_SKIP_HOST_PSOS expects 32-digit pipeline names");
                None
            }
        })
        .collect()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    #[cfg(feature = "diagnostics")]
    {
        static EXCLUDED: OnceLock<HashSet<String>> = OnceLock::new();
        let excluded = EXCLUDED.get_or_init(|| {
            excluded_names(&std::env::var("VICTORY_SKIP_HOST_PSOS").unwrap_or_default())
        });
        if excluded.contains(name) {
            tracing::warn!(name, "skipping configured host pipeline");
            return Err(windows::Win32::Foundation::E_ABORT.into());
        }
        static TRACE: OnceLock<bool> = OnceLock::new();
        if *TRACE.get_or_init(|| crate::switch::on("VICTORY_TRACE_PIPELINES")) {
            // SAFETY: queries the calling thread without accessing caller memory.
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            tracing::info!(
                tid = format_args!("{tid:04x}"),
                name,
                "loading or compiling host pipeline"
            );
        }
    }
    let wide = HSTRING::from(name);
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    let stripe = stripe(name);
    let loaded = {
        let _same_name = stripe.lock().unwrap_or_else(|e| e.into_inner());
        load(&library.library, PCWSTR(wide.as_ptr()))
    };
    if let Ok(pso) = loaded {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the locks: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let _same_name = stripe.lock().unwrap_or_else(|e| e.into_inner());
    let _not_serialising = SERIALISING.read().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { library.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        library.added.fetch_add(1, Ordering::Relaxed);
    }
    Ok(pso)
}

/// When periodic saves happen: the first after 20 s, then 40, 80, and every
/// 160 s, and never sooner than 30 times the last save took.
fn save_interval(saves: u32, last_took: Duration) -> Duration {
    const BASE: [u64; 4] = [20, 40, 80, 160];
    let base = Duration::from_secs(BASE[(saves as usize).min(BASE.len() - 1)]);
    base.max(last_took.saturating_mul(30))
}

#[derive(Default)]
struct SaveSchedule {
    last: Option<Instant>,
    /// Saves that wrote files.
    saves: u32,
    last_took: Duration,
}

/// Writes the library and the early list if pipelines were added since the
/// last save and the save interval has passed (or `now`). A periodic save
/// that finds another save running is skipped.
pub(crate) fn save(now: bool) {
    // Held over serialisation and both writes, so overlapping saves cannot
    // replace files under each other.
    static SCHEDULE: Mutex<SaveSchedule> = Mutex::new(SaveSchedule {
        last: None,
        saves: 0,
        last_took: Duration::ZERO,
    });
    let (Some(dir), Some(library)) = (DIR.get(), library()) else {
        return;
    };
    let mut schedule = if now {
        SCHEDULE.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match SCHEDULE.try_lock() {
            Ok(schedule) => schedule,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        }
    };
    if !now
        && schedule
            .last
            .is_some_and(|t| t.elapsed() < save_interval(schedule.saves, schedule.last_took))
    {
        return;
    }
    let started = Instant::now();
    schedule.last = Some(started);
    let (blob, saved_additions) = {
        let _stores_wait = SERIALISING.write().unwrap_or_else(|e| e.into_inner());
        let added = library.added.load(Ordering::Relaxed);
        if added == 0 && !USED_CHANGED.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: a buffer of the size the library asks for; no store can
        // change it while the write lock is held.
        let bytes = unsafe {
            let mut bytes = vec![0u8; library.library.GetSerializedSize()];
            library.library.Serialize(&mut bytes).map(|()| bytes)
        };
        match bytes {
            Ok(bytes) => (bytes, added),
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return;
            }
        }
    };
    let used_changed = USED_CHANGED.swap(false, Ordering::Relaxed);
    let used: Vec<String> = USED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flatten()
        .cloned()
        .collect();
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| write_atomically(&dir.join(LIBRARY_FILE), &blob))
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => {
            // Keep pipelines stored while the files were being written; only
            // saves subtract, and they are serialised by `SCHEDULE`.
            library.added.fetch_sub(saved_additions, Ordering::Relaxed);
            schedule.saves = schedule.saves.saturating_add(1);
            schedule.last_took = started.elapsed();
            tracing::info!(
                bytes = blob.len(),
                early = used.len(),
                made = MADE.load(Ordering::Relaxed),
                loaded = LOADED.load(Ordering::Relaxed),
                started_early = EARLY.load(Ordering::Relaxed),
                "host pipeline library saved"
            );
        }
        Err(e) => {
            // At least one cache file was not published. Keep the counters
            // dirty for the next save.
            if used_changed {
                USED_CHANGED.store(true, Ordering::Relaxed);
            }
            tracing::warn!(error = %e, "host pipeline library not saved");
        }
    }
}

/// Take turns replacing the cache entry across processes. Proton can deny a
/// concurrent rename even when every writer has its own staging file.
fn publish_cache_file(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let lock_path = path.with_extension("lock");
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0);
    let started = Instant::now();
    loop {
        match options.open(&lock_path) {
            Ok(_exclusive_handle) => return std::fs::rename(tmp, path),
            Err(e)
                if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32)
                    && started.elapsed() < Duration::from_millis(250) =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Writes `bytes` to a unique temporary file beside `path`, then renames it
/// over `path`: a reader never sees a part, and overlapping saves do not
/// share a temporary file.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let extension = format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(extension);
    let written = std::fs::write(&tmp, bytes).and_then(|()| publish_cache_file(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn excluded_names_match_whole_host_descriptions() {
        let a = name("mesh", b"root", &[b"ms", b"ps"], "one");
        let b = name("mesh", b"root", &[b"ms", b"ps"], "two");
        let value = format!(" , {},{}, {}, x", a.to_uppercase(), &b[..16], a);
        assert_eq!(excluded_names(&value), HashSet::from([a]));
        assert!(excluded_names("").is_empty());
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn an_excluded_pipeline_skips_both_driver_calls() {
        // The process reads the setting once. Use a child so other tests can
        // exercise the normal path in the same suite.
        let key = name("mesh", b"root", &[b"ms", b"ps"], "test");
        if std::env::var("VICTORY_SKIP_HOST_PSOS").as_deref() != Ok(&key) {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipeline::library::tests::an_excluded_pipeline_skips_both_driver_calls",
                ])
                .env("VICTORY_SKIP_HOST_PSOS", &key)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            return;
        }
        let result = pipeline(
            &key,
            |_, _| panic!("library touched"),
            || panic!("driver called"),
        );
        assert_eq!(
            result.unwrap_err().code(),
            windows::Win32::Foundation::E_ABORT
        );
    }

    #[test]
    fn stripes_are_stable_and_spread_names() {
        let names: Vec<String> = (0..1024)
            .map(|i| name("graphics", b"root", &[&[i as u8, (i >> 8) as u8]], "state"))
            .collect();
        let mut used = HashSet::new();
        for n in &names {
            assert!(std::ptr::eq(stripe(n), stripe(n)));
            used.insert(stripe(n) as *const _ as usize);
        }
        assert_eq!(used.len(), STRIPES);
    }

    #[test]
    fn save_intervals_back_off_and_follow_slow_saves() {
        let secs = Duration::from_secs;
        let quick = Duration::from_millis(100);
        let intervals: Vec<_> = (0..6).map(|n| save_interval(n, quick)).collect();
        assert_eq!(
            intervals,
            [20, 40, 80, 160, 160, 160].map(secs),
            "20, 40, 80, then every 160 s"
        );
        assert_eq!(save_interval(0, secs(2)), secs(60));
        assert_eq!(save_interval(3, secs(2)), secs(160));
        assert_eq!(save_interval(3, secs(10)), secs(300));
        assert_eq!(save_interval(u32::MAX, Duration::MAX), Duration::MAX);
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok())
        else {
            return; // no host GPU here
        };
        let library =
            open(&device, b"not a pipeline library".repeat(16)).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
        assert_eq!(library.added.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_atomic_writes_publish_one_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for thread in 0..8 {
            let path = path.clone();
            let start = start.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let byte = thread as u8 + 1;
                for _ in 0..32 {
                    write_atomically(&path, &[byte; 64 * 1024]).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let published = std::fs::read(&path).unwrap();
        assert_eq!(published.len(), 64 * 1024);
        assert!(published.iter().all(|&byte| byte == published[0]));
        assert!((1..=8).contains(&published[0]));
        assert!(
            !std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        );
    }

    #[test]
    fn a_busy_publisher_leaves_the_previous_cache_for_the_next_save() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        write_atomically(&path, b"previous").unwrap();
        let other = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(path.with_extension("lock"))
            .unwrap();

        assert!(write_atomically(&path, b"busy").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        drop(other);

        write_atomically(&path, b"next").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"next");
    }

    #[test]
    fn failed_atomic_writes_remove_their_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        std::fs::create_dir(&path).unwrap();

        assert!(write_atomically(&path, b"not published").is_err());
        assert!(
            !std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        );
    }
}
