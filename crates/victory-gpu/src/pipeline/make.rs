//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{Shader, Translated};
use victory_shader::emit::{StructuredBinding, translate_for_adapter};
use victory_shader::{ResourceType, RootType, UsedResource};

type Strides = Vec<(StructuredBinding, u32)>;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Arc<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<Arc<HostPipeline>>>,
    bindings: OnceLock<Vec<StructuredBinding>>,
    strides: Strides,
    partial: bool,
    variants: Mutex<VecDeque<Arc<Pending>>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        Pending {
            shaders,
            graphics: graphics.map(Arc::from),
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            bindings: OnceLock::new(),
            strides: Vec::new(),
            partial: false,
            variants: Mutex::new(VecDeque::new()),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            static TRACE: OnceLock<bool> = OnceLock::new();
            if *TRACE.get_or_init(|| crate::switch::on("VICTORY_TRACE_PIPELINES")) {
                // SAFETY: GetCurrentThreadId queries the calling thread and takes no pointers.
                let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
                tracing::info!(tid = format_args!("{tid:04x}"), shaders = ?self.shaders, "starting host pipeline");
            }
            if self.strides.is_empty() && !self.partial {
                let made = HostPipeline::new(&self.shaders, self.graphics.as_deref(), &[]);
                if made.is_none()
                    && let Some(bindings) = self.find_stride_bindings()
                {
                    let _ = self.bindings.set(bindings);
                }
                made.map(Arc::new)
            } else {
                let shaders = self.specialize_shaders()?;
                HostPipeline::new(&shaders, self.graphics.as_deref(), &self.strides).map(Arc::new)
            }
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }

    fn find_stride_bindings(&self) -> Option<Vec<StructuredBinding>> {
        let host = crate::host::host()?;
        let bindings = std::cell::RefCell::new(BTreeSet::new());
        for shader in &self.shaders {
            if shader.translated().is_none() {
                // Probe lowering with a valid annotation. Only the binding
                // locations survive the probe; its DXIL never reaches D3D12.
                translate_for_adapter::<false>(
                    &shader.container,
                    host.wave_widths(),
                    Some(&|binding, required| {
                        required.then(|| {
                            bindings.borrow_mut().insert(binding);
                            4
                        })
                    }),
                )
                .ok()?;
            }
        }
        Some(bindings.into_inner().into_iter().collect())
    }

    fn specialize_shaders(&self) -> Option<Vec<Arc<Shader>>> {
        let host = crate::host::host()?;
        self.shaders
            .iter()
            .map(|shader| {
                if !self.partial && shader.translated.get().is_some_and(Option::is_some) {
                    return Some(shader.clone());
                }
                let accessed = std::cell::RefCell::new(BTreeSet::new());
                let translate = if self.partial {
                    translate_for_adapter::<true>
                } else {
                    translate_for_adapter::<false>
                };
                let (host, mut native) = translate(
                    &shader.container,
                    host.wave_widths(),
                    Some(&|binding, _| {
                        let i = self
                            .strides
                            .binary_search_by_key(&binding, |&(key, _)| key)
                            .ok()?;
                        let stride = self.strides[i].1;
                        accessed.borrow_mut().insert((binding, stride));
                        Some(stride)
                    }),
                )
                .map_err(|error| {
                    tracing::warn!(?shader, %error, partial_dispatch = self.partial, "specialized shader refused");
                })
                .ok()?;
                // Include exact slots absent from the console metadata so draw
                // binding snapshots and transitions the views that lowering reads.
                for (binding, stride) in accessed.into_inner() {
                    let kind = if binding.uav {
                        ResourceType::Uav
                    } else {
                        ResourceType::Srv
                    };
                    if let Some(entry) = native.used.iter_mut().find(|u| {
                        u.root == RootType::Table
                            && u.slot == binding.slot
                            && u.table_offset_dwords == binding.offset_dwords
                            && u.kind == kind
                    }) {
                        entry.stride = stride;
                    } else {
                        let user_data = host
                            .user_data
                            .iter()
                            .find(|u| u.root == RootType::Table && u.slot == binding.slot)?
                            .index;
                        native.used.push(UsedResource {
                            kind,
                            root: RootType::Table,
                            table_offset_dwords: binding.offset_dwords,
                            lower_bound: 0,
                            stride,
                            dwords: 4,
                            user_data: user_data.try_into().ok()?,
                            slot: binding.slot,
                        });
                    }
                }
                Some(Arc::new(Shader {
                    stage: shader.stage,
                    hash: shader.hash.clone(),
                    container: Vec::new(),
                    translated: OnceLock::from(Some(Translated::from_native(host, native))),
                    #[cfg(feature = "diagnostics")]
                    wave64: OnceLock::new(),
                }))
            })
            .collect()
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| {
                    let fspx = victory_shader::inspect_fspx(&s.container).is_some();
                    let note = if fspx { " (FSPX compute module)" } else { "" };
                    format!("{} {}{note}", s.stage, s.hash)
                })
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    let made = p.made.get()?.as_ref();
    if made.is_some() && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    made.map(Arc::as_ref)
}

/// Uses the existing worker queue and wait budget for stride variants. Old
/// variants can be evicted while commands keep their PSO and root signature.
pub(super) fn ready_with_strides(
    p: &Arc<Pending>,
    view_stride: impl Fn(StructuredBinding) -> Option<u32>,
) -> Option<Arc<HostPipeline>> {
    if let Some(made) = p.made.get()?.as_ref() {
        return Some(made.clone());
    }
    let bindings = p.bindings.get().filter(|bindings| !bindings.is_empty())?;
    let Some(strides) = bindings
        .iter()
        .map(|&binding| Some((binding, view_stride(binding)?)))
        .collect::<Option<Strides>>()
    else {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| {
            tracing::warn!("a draw or dispatch requires a matching structured buffer view")
        });
        return None;
    };
    ready_variant(p, &strides, false)
}

/// Compilation uses the same frame wait budget and bounded variant cache as
/// pipelines specialized for the currently bound structured-buffer strides.
pub(super) fn ready_variant(
    p: &Arc<Pending>,
    strides: &[(StructuredBinding, u32)],
    partial: bool,
) -> Option<Arc<HostPipeline>> {
    let variant = {
        let mut variants = p.variants.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = variants
            .iter()
            .position(|variant| variant.partial == partial && variant.strides == strides)
        {
            let variant = variants.remove(i).expect("a variant at this index");
            variants.push_back(variant.clone());
            variant
        } else {
            const MAX_VARIANTS: usize = 16;
            let variant = Arc::new(Pending {
                shaders: p.shaders.clone(),
                graphics: p.graphics.clone(),
                queue_state: AtomicU8::new(NOT_QUEUED),
                made: OnceLock::new(),
                bindings: OnceLock::new(),
                strides: strides.to_vec(),
                partial,
                variants: Mutex::new(VecDeque::new()),
                key: p.key.clone(),
                used: AtomicBool::new(false),
            });
            if variants.len() >= MAX_VARIANTS {
                variants.pop_front();
            }
            variants.push_back(variant.clone());
            variant
        }
    };
    ready(&variant)?;
    variant.made.get()?.clone()
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static WAITED_US: AtomicU64 = AtomicU64::new(0);
    if FRAME.swap(frame, Ordering::Relaxed) != frame {
        WAITED_US.store(0, Ordering::Relaxed);
    }
    let left = BUDGET.saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(
                WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Starts the workers (half the logical processors, 2 to 8) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(|| {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        next().make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indirect_draws_without_a_pipeline_leave_budget_for_a_later_record() {
        use crate::object::{self, S_OK};
        use crate::pipeline::{PIPELINE_IIDS, Pipeline, hand_out};
        use victory_shader::native::{ComputeIo, StageIo};
        use victory_shader::{HwStage, NativeProgram};

        let _scope = crate::host::isolated_retire_scope();
        let Some(host) = crate::host::host() else {
            return;
        };
        let create = |shaders| {
            let mut pipeline = std::ptr::null();
            assert_eq!(
                hand_out(shaders, None, &PIPELINE_IIDS[0], &mut pipeline),
                S_OK
            );
            // Resolve the synthetic PSOs before recording so this budget test
            // is independent of the async compiler's frame deadline.
            crate::pipeline::pipeline(pipeline).host.make();
            pipeline
        };
        let missing = create(vec![]); // no vertex or mesh stage
        let compute = create(vec![Shader::test_compute(NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 1,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [1, 1, 1],
            }),
            code: 0xBF81_0000u32.to_le_bytes().to_vec(), // s_endpgm
        })]);
        let list = host.new_list().unwrap();
        // Two skipped draws could consume the whole list budget. The last
        // record dispatches three groups through a working host pipeline.
        let records = [[1u32 << 24, 4, 0, 0], [1 << 24, 4, 0, 0], [3, 1, 1, 0]];
        let start = records.as_ptr() as u64;
        crate::shadow::register(start as usize, start, size_of_val(&records) as u64);
        let mut state = crate::draw::State::default();
        state.set_pipeline(missing);
        let mut dropped = "";
        let ran_draw = state.execute_indirect(&list, (0, 16, 2), start, None, &mut dropped);
        state.set_pipeline(compute);
        let dispatch = start + 32;
        let mut reason = "";
        let ran_dispatch = state.execute_indirect(&list, (2, 16, 1), dispatch, None, &mut reason);
        list.close();
        host.execute(&[&list]);
        let copy = crate::shadow::find(dispatch, 4).unwrap();
        let groups = u32::from_le_bytes(copy.bytes(dispatch, 4).unwrap().try_into().unwrap());
        crate::shadow::unregister(start as usize, start);
        object::release_boxed::<Pipeline>(missing);
        object::release_boxed::<Pipeline>(compute);
        assert!(!ran_draw);
        assert_eq!(dropped, "no host pipeline");
        assert!(ran_dispatch, "{reason}");
        assert_eq!(
            groups, 3,
            "skipped draws exhausted the following dispatch budget"
        );
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }
}
