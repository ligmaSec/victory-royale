//! Pipeline states. Both descriptions start as on PC: the root signature,
//! then the shader stages (five for graphics, one for compute); streams
//! carry the rest of the graphics state as subobjects of the standard
//! types.
//!
//! Host pipelines are made on a pipeline's first draw, not at creation:
//! The title creates some 60,000 pipelines in its first 20
//! seconds and draws with a few dozen (decision 0007, step 3). Shader
//! containers are shared by content hash and translated once, on first
//! use (decision 0003).
//!
//! Host pipelines cover vertex-plus-pixel (including a geometry stage that
//! expands rectangle lists), mesh-plus-pixel, and compute. shortcut:
//! tessellation stages and the plain graphics description keep their
//! containers only. The 32-byte packet after the header stays zero: titles
//! only copy it inline, which our null command-list limits keep from running.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
#[cfg(windows)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use crate::object::{
    self, E_FAIL, E_INVALIDARG, E_NOTIMPL, HRESULT, Iid, Object, PIPELINE, S_OK, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    /// Vertex, pixel, domain, hull, geometry.
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

/// A shader container a title passed, shared by every pipeline with the
/// same bytes.
pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    /// The first 8 bytes of its BLAKE3, as logged.
    pub(crate) hash: String,
    container: Vec<u8>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
    /// Whether its native program runs in 64-lane console waves
    /// (diagnostics: `VICTORY_SKIP_WAVE64`).
    #[cfg(all(windows, feature = "diagnostics"))]
    wave64: OnceLock<bool>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

/// Every container seen, by full content hash.
static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

/// The shared container for `bytes`.
fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(windows)]
                translated: OnceLock::new(),
                #[cfg(all(windows, feature = "diagnostics"))]
                wave64: OnceLock::new(),
            })
        })
        .clone()
}

/// A translated shader and the console resources its code reads.
#[cfg(windows)]
pub(crate) struct Translated {
    /// Console group dimensions, including when host quads use a flattened group.
    pub(crate) compute_threads: Option<[u32; 3]>,
    pub(crate) host: victory_shader::emit::HostShader,
    pub(crate) used: Vec<victory_shader::UsedResource>,
}

#[cfg(windows)]
impl Translated {
    fn from_native(
        host: victory_shader::emit::HostShader,
        native: victory_shader::NativeProgram,
    ) -> Self {
        Self {
            compute_threads: match native.io {
                victory_shader::native::StageIo::Cs(cs) => Some(cs.threads),
                _ => None,
            },
            host,
            used: native.used,
        }
    }
}

#[cfg(windows)]
impl Shader {
    #[cfg(test)]
    pub(crate) fn for_test(
        stage: &'static str,
        hw_stage: victory_shader::HwStage,
        used: Vec<victory_shader::UsedResource>,
    ) -> Arc<Self> {
        let translated = OnceLock::new();
        let _ = translated.set(Some(Translated {
            compute_threads: None,
            host: victory_shader::emit::HostShader {
                stage: hw_stage,
                dxil: Vec::new(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
            },
            used,
        }));
        Arc::new(Self {
            stage,
            hash: String::new(),
            container: Vec::new(),
            translated,
            #[cfg(feature = "diagnostics")]
            wave64: OnceLock::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_compute(native: victory_shader::NativeProgram) -> Arc<Self> {
        use victory_shader::{Container, emit, io, ir, resource, structure};
        let c = Container {
            digest: [0; 16],
            version: (1, 0),
            parts: Vec::new(),
        };
        let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let host = emit::emit(&c, &native, &p, &st, &io, &res).unwrap();
        Arc::new(Self {
            stage: "compute",
            hash: blake3::hash(&host.dxil).to_hex()[..16].to_string(),
            container: Vec::new(),
            #[cfg(feature = "diagnostics")]
            wave64: OnceLock::from(native.wave64),
            translated: OnceLock::from(Some(Translated::from_native(host, native))),
        })
    }

    /// The translation, made on first use; a shader that does not
    /// translate is logged once.
    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                if let Some(fspx) = FspxSummary::of(&self.container) {
                    tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        threads = ?fspx.threads,
                        wave = fspx.wave(),
                        code_size = fspx.code_size,
                        container_size = self.container.len(),
                        "FSPX compute module: it has no DXIL and its GFX10 code is not translated yet"
                    );
                }
                let done = translate(&self.container);
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    Err(e) => tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        error = %e,
                        "shader does not translate; its pipelines' draws are dropped"
                    ),
                }
                done.ok()
            })
            .as_ref()
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    use victory_shader::{Container, FourCC, HwStage};
    let host = crate::shader_cache::host_shader(container)?;
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let dxil = c.part(FourCC::DXIL).ok_or("no DXIL part")?;
    let native = victory_shader::native_programs(dxil.data)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
            )
        })
        .ok_or("no translated shader stage")?;
    Ok(Translated::from_native(host, native))
}

#[repr(C)]
struct Pipeline {
    object: Object,
    packet: [u32; 8],
    /// Each used stage's container.
    shaders: Vec<Arc<Shader>>,
    /// Whether a list has bound it yet (its shaders are logged then).
    bound: std::sync::atomic::AtomicBool,
    /// What its host pipeline is made from, and the host pipeline once made
    /// (by a worker: the title may release the pipeline meanwhile).
    #[cfg(windows)]
    host: Arc<make::Pending>,
    #[cfg(windows)]
    variants: Mutex<HashMap<[i32; 8], Arc<make::Pending>>>,
}
const _: () = assert!(offset_of!(Pipeline, packet) == 0x10);

/// Stage names, in description order.
const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

fn pipeline<'a>(this: This) -> &'a Pipeline {
    // SAFETY: the title passes one of our pipelines.
    unsafe { &*this.cast::<Pipeline>() }
}

/// List method: logs each pipeline's shaders the first time a list binds
/// it, so a run lists the shaders its frames need first.
pub(crate) fn bound(this: This) {
    if this.is_null() {
        return;
    }
    let p = pipeline(this);
    if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    for shader in &p.shaders {
        tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container.len(), hash = %shader.hash, "first bind");
    }
}

#[cfg(windows)]
impl Pipeline {
    fn variant(&self, widened: Option<&[u32; 8]>) -> Option<Arc<make::Pending>> {
        let graphics = self.host.graphics.as_deref()?;
        let formats = graphics.formats_for_targets(widened?)?;
        let mut variants = self
            .variants
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Some(
            variants
                .entry(formats)
                .or_insert_with(|| Arc::new(self.host.with_formats(graphics, formats)))
                .clone(),
        )
    }
}

/// Host pipelines queued and not made yet.
#[cfg(windows)]
pub(crate) fn making() -> u64 {
    make::waiting()
}

/// Draws and dispatches dropped since the last [`take_drops`], by reason
/// (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Shader hashes as printed in the pipeline diagnostics.
#[cfg(all(windows, feature = "diagnostics"))]
fn parse_skip_hashes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .filter_map(|hash| {
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                tracing::warn!(
                    hash,
                    "VICTORY_SKIP_PIPELINES expects 16-digit shader hashes"
                );
                None
            }
        })
        .collect()
}

/// The stages `VICTORY_SKIP_WAVE64` names (`mesh`, `compute`, `vertex`,
/// `pixel`).
#[cfg(all(windows, feature = "diagnostics"))]
fn parse_skip_stages(value: &str) -> Vec<&'static str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let stage = ["mesh", "compute", "vertex", "pixel"]
                .into_iter()
                .find(|&k| k.eq_ignore_ascii_case(s));
            if stage.is_none() {
                tracing::warn!(
                    stage = s,
                    "VICTORY_SKIP_WAVE64 expects mesh, compute, vertex or pixel"
                );
            }
            stage
        })
        .collect()
}

#[cfg(all(windows, feature = "diagnostics"))]
impl Shader {
    /// Whether the container's native program runs in 64-lane console waves.
    fn wave64(&self) -> bool {
        *self.wave64.get_or_init(|| {
            victory_shader::Container::parse(&self.container)
                .ok()
                .and_then(|c| {
                    c.part(victory_shader::FourCC::DXIL)
                        .map(|p| p.data.to_vec())
                })
                .and_then(|d| victory_shader::native::native_programs(&d).ok())
                .is_some_and(|ps| ps.iter().any(|p| p.wave64))
        })
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
fn skipped_shader(p: &Pipeline) -> Option<&str> {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    static WAVE64: OnceLock<Vec<&'static str>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let value = std::env::var("VICTORY_SKIP_PIPELINES").unwrap_or_default();
        parse_skip_hashes(&value)
    });
    let wave64 = WAVE64.get_or_init(|| {
        let value = std::env::var("VICTORY_SKIP_WAVE64").unwrap_or_default();
        parse_skip_stages(&value)
    });
    if hashes.is_empty() && wave64.is_empty() {
        return None;
    }
    p.shaders
        .iter()
        .find(|shader| {
            hashes.contains(&shader.hash) || (wave64.contains(&shader.stage) && shader.wave64())
        })
        .map(|shader| shader.hash.as_str())
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn is_skipped(this: This) -> bool {
    !this.is_null() && skipped_shader(pipeline(this)).is_some()
}

/// Why a draw or dispatch of `this` is dropped, in words (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn drop_reason(this: This) -> String {
    if this.is_null() {
        return "no pipeline set".to_owned();
    }
    let p = pipeline(this);
    skipped_shader(p)
        .map(|hash| format!("skipped shader {hash}"))
        .or_else(|| make::missing(&p.host))
        .unwrap_or_else(|| {
            "has a host pipeline (prepare or bind failed, or another kind of pipeline)".to_owned()
        })
}

/// Counts a draw or dispatch of `this` intentionally skipped or dropped
/// for want of a host pipeline (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_drop(this: This, widened: Option<&[u32; 8]>) {
    let why = if this.is_null() {
        Some("no pipeline set".to_owned())
    } else {
        let p = pipeline(this);
        skipped_shader(p)
            .map(|hash| format!("skipped shader {hash}"))
            .or_else(|| {
                let variant = p.variant(widened);
                make::missing(variant.as_ref().unwrap_or(&p.host))
            })
    };
    if let Some(why) = why {
        note_why(why);
    }
}

/// Counts a draw or dispatch dropped for `why` (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_why(why: String) {
    let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
    *drops
        .get_or_insert_with(HashMap::new)
        .entry(why)
        .or_default() += 1;
}

/// The drops counted since the last call, most first (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn take_drops() -> Vec<(String, u64)> {
    let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
    // Frame reports only show the first 40 reasons. Keep ties ordered so
    // a HashMap iteration order change cannot change which reasons appear.
    drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    drops
}

/// The stage's container, if it has one.
fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(shader(stage, unsafe {
        std::slice::from_raw_parts(code.code, code.size)
    }))
}

fn hand_out(
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::trace!(?shaders, "pipeline state");
    #[cfg(windows)]
    let host = {
        let mut graphics = graphics;
        if let Some(state) = graphics.as_deref_mut() {
            state.choose_default_formats();
        }
        Arc::new(make::Pending::new(shaders.clone(), graphics))
    };
    #[cfg(windows)]
    make::early(&host);
    let pipeline = Box::new(Pipeline {
        object: Object::new(&FUNCTIONS),
        packet: [0; 8],
        #[cfg(windows)]
        host,
        #[cfg(windows)]
        variants: Mutex::new(HashMap::new()),
        shaders,
        bound: std::sync::atomic::AtomicBool::new(false),
    });
    object::hand_out(PIPELINE, pipeline, iid, &PIPELINE_IIDS, out)
}

/// Device method.
pub(crate) extern "system" fn create_graphics(
    _: This,
    desc: *const GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
    let shaders = stages.filter_map(|(s, c)| copy(s, c)).collect();
    hand_out(
        shaders,
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method.
pub(crate) extern "system" fn create_compute(
    _: This,
    desc: *const ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    hand_out(
        copy("compute", &desc.stage).into_iter().collect(),
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// What an FSPX compute module says about itself, for the logs.
#[derive(Debug, PartialEq, Eq)]
struct FspxSummary {
    threads: [u32; 3],
    wave32: bool,
    /// Bytes of GFX10 code (kind 6).
    code_size: usize,
}

impl FspxSummary {
    /// The summary if `bytes` is an FSPX module, bare or in a DXBC container. Its thread group
    /// is zero and its wave size 64 if its dispatch state is missing or short.
    fn of(bytes: &[u8]) -> Option<Self> {
        let fspx = victory_shader::inspect_fspx(bytes)?;
        let info = fspx.dispatch_info();
        Some(Self {
            threads: info.map_or([0; 3], |i| i.threads),
            wave32: info.is_some_and(|i| i.wave32),
            code_size: fspx.program.map_or(0, <[u8]>::len),
        })
    }

    fn wave(&self) -> &'static str {
        if self.wave32 { "wave32" } else { "wave64" }
    }
}

/// Device method 0x1f0 (slot 62): the plain compute description plus console
/// extensions `(desc, extension count, extensions, iid, out)`.
/// shortcut: the extensions (console shader settings) are ignored, as is
/// the rest of the description.
///
/// An FSPX compute module in the description is logged. Diagnostics:
/// `VICTORY_FSPX_REJECT_SLOT62=1` fails it with `E_FAIL` (and no pipeline), to see how
/// the title reacts to a pipeline that is not made.
pub(crate) extern "system" fn create_compute_x(
    this: This,
    desc: *const ComputeHead,
    _extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let stage = unsafe { desc.as_ref() }.and_then(|d| bytecode(&d.stage));
    if let Some(fspx) = stage.and_then(FspxSummary::of) {
        let reject = fspx_reject_slot62();
        tracing::info!(
            threads = ?fspx.threads,
            wave = fspx.wave(),
            code_size = fspx.code_size,
            reject,
            "CreateComputePipelineStateX (slot 62) called with an FSPX compute module"
        );
        if reject {
            return fail(E_FAIL, out);
        }
    }
    create_compute(this, desc, iid, out)
}

/// The code of `code`, if it has any. The title owns the bytes, so they last as long as the
/// call that passed them.
fn bytecode<'a>(code: &Bytecode) -> Option<&'a [u8]> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(unsafe { std::slice::from_raw_parts(code.code, code.size) })
}

/// `hr` for a pipeline that was not made: the title's pointer is cleared so it holds no
/// object.
fn fail(hr: HRESULT, out: *mut This) -> HRESULT {
    if !out.is_null() {
        // SAFETY: the title passes room for the pipeline's pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Whether slot 62 refuses FSPX modules (diagnostics: `VICTORY_FSPX_REJECT_SLOT62`).
fn fspx_reject_slot62() -> bool {
    cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_FSPX_REJECT_SLOT62")
}

/// The HRESULT slot 67 returns: `E_FAIL`, or what `VICTORY_SLOT67_HR` names in the
/// diagnostics build.
fn slot67_result() -> HRESULT {
    #[cfg(feature = "diagnostics")]
    {
        static RESULT: std::sync::OnceLock<HRESULT> = std::sync::OnceLock::new();
        *RESULT.get_or_init(|| {
            let value = std::env::var("VICTORY_SLOT67_HR").ok();
            parse_slot67_hr(value.as_deref()).unwrap_or_else(|| {
                tracing::warn!(
                    "VICTORY_SLOT67_HR expects E_FAIL, E_INVALIDARG, E_NOTIMPL or S_OK; using E_FAIL"
                );
                E_FAIL
            })
        })
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        E_FAIL
    }
}

/// The HRESULT a `VICTORY_SLOT67_HR` value names (`E_FAIL` if unset), `None` if unknown.
#[cfg_attr(not(any(feature = "diagnostics", test)), allow(dead_code))]
fn parse_slot67_hr(value: Option<&str>) -> Option<HRESULT> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(E_FAIL);
    };
    [
        ("E_FAIL", E_FAIL),
        ("E_INVALIDARG", E_INVALIDARG),
        ("E_NOTIMPL", E_NOTIMPL),
        ("S_OK", S_OK),
    ]
    .into_iter()
    .find_map(|(name, hr)| value.eq_ignore_ascii_case(name).then_some(hr))
}

/// The description of device method 0x218 (slot 67), `DeserializeComputePipelineStateX`:
/// the root signature, then the pieces of a precompiled compute module as pointer and size
/// pairs, by FSPX kind 0, 1, 6 and 7.
#[repr(C)]
pub(crate) struct DeserializeHead {
    root: This,
    kind0: Bytecode,
    kind1: Bytecode,
    kind6: Bytecode,
    kind7: Bytecode,
}

/// Device method 0x218 (slot 67): a compute pipeline from precompiled pieces
/// `(desc, iid, out)`, which the AMD upscaler uses with GFX10 programs we do not translate.
///
/// The description is logged and the call fails with `E_FAIL` and no pipeline, so the title
/// takes its own fallback. Diagnostics: `VICTORY_SLOT67_HR` (`E_FAIL`, `E_INVALIDARG`,
/// `E_NOTIMPL` or `S_OK`) sets what it returns. `S_OK` hands out a pipeline holding the
/// pieces as an FSPX module, whose dispatches are dropped.
pub(crate) extern "system" fn deserialize_compute_x(
    _: This,
    desc: *const DeserializeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return fail(E_INVALIDARG, out);
    };
    let piece = |code: &Bytecode| bytecode(code).unwrap_or_default();
    let (kind0, kind1, kind6, kind7) = (
        piece(&desc.kind0),
        piece(&desc.kind1),
        piece(&desc.kind6),
        piece(&desc.kind7),
    );
    let info = victory_shader::dxbc::dispatch_info(kind0);
    let hr = slot67_result();
    tracing::info!(
        threads = ?info.map(|i| i.threads),
        wave = info.map(|i| if i.wave32 { "wave32" } else { "wave64" }),
        kind0_size = kind0.len(),
        kind1_size = kind1.len(),
        code_size = kind6.len(),
        kind7_size = kind7.len(),
        hr = format_args!("{hr:#010x}"),
        "DeserializeComputePipelineStateX (slot 67) called"
    );
    if hr != S_OK {
        return fail(hr, out);
    }
    let module = victory_shader::Fspx::assemble(kind0, kind1, kind6, kind7);
    hand_out(
        vec![shader("compute", &module)],
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// A pipeline described as a stream: its size and the subobjects, each a
/// type then its data, padded to 8 bytes.
#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

/// A subobject type's size (type included), and the stage it is if it is a
/// shader. The console numbers amplification and mesh shaders 23 and 24
/// (PC: 24 and 25); its own types from 25 on are not sized yet.
fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let stage = |name| Some((24, Some(name)));
    match kind {
        1 => stage("vertex"),
        2 => stage("pixel"),
        3 => stage("domain"),
        4 => stage("hull"),
        5 => stage("geometry"),
        6 => stage("compute"),
        23 => stage("amplification"),
        24 => stage("mesh"),
        0 => Some((16, None)),
        12 | 19 => Some((24, None)),
        7 | 15 => Some((40, None)),
        8 => Some((336, None)),
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

/// Device method: a pipeline from a stream; the shaders are kept, and the
/// graphics state for the host pipeline.
pub(crate) extern "system" fn create_stream(
    _: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.stream.is_null() {
        return E_INVALIDARG;
    }
    let mut shaders = Vec::new();
    #[cfg(windows)]
    let mut graphics = host::GraphicsState::default();
    let mut at = 0;
    while at + 8 <= desc.size {
        // SAFETY: inside the stream the title passes.
        let kind = unsafe { desc.stream.add(at).cast::<u32>().read_unaligned() };
        let Some((size, stage)) = subobject(kind) else {
            tracing::warn!(
                kind,
                at,
                "pipeline subobject of unknown size; the rest is ignored"
            );
            break;
        };
        if let Some(stage) = stage {
            // SAFETY: a shader subobject has its bytecode at +8.
            let code = unsafe { &*desc.stream.add(at + 8).cast::<Bytecode>() };
            shaders.extend(copy(stage, code));
        }
        #[cfg(windows)]
        // SAFETY: a whole subobject of this kind lies at `at`.
        unsafe {
            graphics.read(kind, desc.stream.add(at))
        };
        at += size;
    }
    hand_out(
        shaders,
        #[cfg(windows)]
        Some(Box::new(graphics)),
        iid,
        out,
    )
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &PIPELINE_IIDS).then_some(this);
    object::answer(PIPELINE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Pipeline>(this)
}

/// IUnknown 3, object 4, device child 1, pipeline 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
mod library;
#[cfg(windows)]
mod make;

/// Keeps compiled host pipelines in `dir/adapters/<key>/` across runs.
/// Adapters share the list of title pipelines to start early in `dir`.
#[cfg(windows)]
pub fn use_pipeline_cache(dir: std::path::PathBuf) {
    library::use_pipeline_cache(dir);
}

/// Saves the pipeline cache now if anything was added (at the end of a run).
#[cfg(windows)]
pub fn save_pipeline_cache() {
    library::save(true);
}

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_stages_names_known_stages_only() {
        assert_eq!(
            super::parse_skip_stages(" mesh,Compute ,,"),
            ["mesh", "compute"]
        );
        assert_eq!(super::parse_skip_stages("hull,pixel"), ["pixel"]);
        assert!(super::parse_skip_stages("").is_empty());
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::parse_skip_hashes(" , ").is_empty());
        assert_eq!(
            super::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn flattened_compute_pipeline_schedules_console_groups() {
        use victory_shader::{
            HwStage, NativeProgram,
            native::{ComputeIo, StageIo},
        };

        // Pair a real flattened host shader with the shape from the console
        // container, as happens when loading an 8x8 translation from cache.
        let io = |threads| StageIo::Cs(ComputeIo { rsrc2: 0, threads });
        let mut native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 1,
            layout: Vec::new(),
            used: Vec::new(),
            io: io([64, 1, 1]),
            code: 0xbf81_0000u32.to_le_bytes().to_vec(), // s_endpgm
        };
        let mut shader = Arc::try_unwrap(Shader::test_compute(native.clone())).unwrap();
        let flat = shader.translated.take().unwrap().unwrap().host;
        assert_eq!(flat.compute_threads(), Some([64, 1, 1]));
        native.io = io([8, 8, 1]);
        let translated = Translated::from_native(flat, native);
        assert_eq!(translated.compute_threads, Some([8, 8, 1]));
        shader.translated = OnceLock::from(Some(translated));

        if crate::host::host().is_some() {
            let pipeline = host::HostPipeline::new(&[Arc::new(shader)], None, &[]).unwrap();
            let size = pipeline.compute_threads.unwrap();
            assert_eq!(
                crate::list::thread_groups([8, 8, 1], size),
                Some(([1, 1, 1], false))
            );
            assert_eq!(
                crate::list::thread_groups([12, 8, 1], size),
                Some(([2, 1, 1], true))
            );
        }
    }

    #[test]
    fn slot67_result_names_parse() {
        assert_eq!(parse_slot67_hr(None), Some(E_FAIL));
        assert_eq!(parse_slot67_hr(Some("  ")), Some(E_FAIL));
        assert_eq!(parse_slot67_hr(Some("e_invalidarg")), Some(E_INVALIDARG));
        assert_eq!(parse_slot67_hr(Some(" E_NOTIMPL ")), Some(E_NOTIMPL));
        assert_eq!(parse_slot67_hr(Some("S_OK")), Some(S_OK));
        assert_eq!(parse_slot67_hr(Some("E_NOPE")), None);
    }

    fn fspx_module(code: &[u8]) -> Vec<u8> {
        let mut k0 = [0u8; 28];
        k0[12..16].copy_from_slice(&(8u32 | 16 << 16).to_le_bytes());
        k0[16..20].copy_from_slice(&(1u32 | 1 << 20).to_le_bytes());
        victory_shader::Fspx::assemble(&k0, &[1; 20], code, &[0x77; 16])
    }

    #[test]
    fn fspx_summary_reads_the_dispatch_state() {
        let module = fspx_module(&[0x55; 64]);
        let summary = FspxSummary::of(&module).unwrap();
        assert_eq!(summary.threads, [8, 16, 1]);
        assert!(summary.wave32);
        assert_eq!(summary.code_size, 64);
        assert_eq!(summary.wave(), "wave32");
        assert!(FspxSummary::of(b"DXBC").is_none());
        assert!(FspxSummary::of(&[]).is_none());
        let bare = victory_shader::Fspx::assemble(&[], &[], &[], &[]);
        let summary = FspxSummary::of(&bare).unwrap();
        assert_eq!(
            (summary.threads, summary.wave(), summary.code_size),
            ([0; 3], "wave64", 0)
        );
    }

    fn bytecode_of(bytes: &[u8]) -> Bytecode {
        Bytecode {
            code: bytes.as_ptr(),
            size: bytes.len(),
        }
    }

    #[test]
    fn slot67_fails_without_a_pipeline_by_default() {
        let module = fspx_module(&[0x55; 64]);
        let fspx = victory_shader::inspect_fspx(&module).unwrap();
        let desc = DeserializeHead {
            root: std::ptr::null(),
            kind0: bytecode_of(fspx.dispatch_state.unwrap()),
            kind1: bytecode_of(fspx.kind1.unwrap()),
            kind6: bytecode_of(fspx.program.unwrap()),
            kind7: bytecode_of(fspx.records.unwrap()),
        };
        // A pointer the call must clear.
        let mut p: This = &desc as *const DeserializeHead as This;
        let hr = deserialize_compute_x(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, slot67_result());
        assert_eq!(hr, E_FAIL);
        assert!(p.is_null());
    }

    #[test]
    fn slot67_refuses_no_description_and_tolerates_a_null_out() {
        let mut p: This = std::ptr::null();
        let hr = deserialize_compute_x(
            std::ptr::null(),
            std::ptr::null(),
            &PIPELINE_IIDS[0],
            &mut p,
        );
        assert_eq!(hr, E_INVALIDARG);
        let empty = DeserializeHead {
            root: std::ptr::null(),
            kind0: Bytecode {
                code: std::ptr::null(),
                size: 0,
            },
            kind1: Bytecode {
                code: std::ptr::null(),
                size: 0,
            },
            kind6: Bytecode {
                code: std::ptr::null(),
                size: 0,
            },
            kind7: Bytecode {
                code: std::ptr::null(),
                size: 0,
            },
        };
        let hr = deserialize_compute_x(
            std::ptr::null(),
            &empty,
            &PIPELINE_IIDS[0],
            std::ptr::null_mut(),
        );
        assert_eq!(hr, E_FAIL);
    }

    #[test]
    fn slot67_s_ok_path_hands_out_an_fspx_pipeline() {
        // The pipeline a successful slot 67 would give: its container is the assembled module.
        let module = fspx_module(&[0x55; 64]);
        let fspx = victory_shader::inspect_fspx(&module).unwrap();
        let reassembled = victory_shader::Fspx::assemble(
            fspx.dispatch_state.unwrap(),
            fspx.kind1.unwrap(),
            fspx.program.unwrap(),
            fspx.records.unwrap(),
        );
        assert_eq!(reassembled, module);
        let mut p = std::ptr::null();
        let hr = hand_out(
            vec![shader("compute", &reassembled)],
            #[cfg(windows)]
            None,
            &PIPELINE_IIDS[0],
            &mut p,
        );
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("compute", module)]);
        object::release_boxed::<Pipeline>(p);
    }

    #[test]
    fn slot62_passes_fspx_through_unless_rejected() {
        let module = fspx_module(&[0x55; 8]);
        let desc = ComputeHead {
            root: std::ptr::null(),
            stage: bytecode_of(&module),
        };
        let mut p = std::ptr::null();
        let hr = create_compute_x(
            std::ptr::null(),
            &desc,
            0,
            std::ptr::null(),
            &PIPELINE_IIDS[0],
            &mut p,
        );
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("compute", module)]);
        object::release_boxed::<Pipeline>(p);
    }

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        pipeline(p)
            .shaders
            .iter()
            .map(|s| (s.stage, s.container.clone()))
            .collect()
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        // A second pipeline with the same pixel shader shares its container.
        let mut q = std::ptr::null();
        create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &pipeline(p).shaders[1],
            &pipeline(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    /// The stream layout the title builds (title+0x86f090): root
    /// signature, five shader slots (the first a mesh shader), blend, sample
    /// mask, rasterizer, depth-stencil 1, sample description, input layout,
    /// topology, render target and depth formats: 0x2b0 bytes.
    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        // Topology type 3 (triangles), one render target of format 28.
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let g = pipeline(p).host.graphics.as_deref().unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn packed_rtv_pipeline_switches_physical_formats() {
        use crate::resource::{packed_test, target_format};
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B5G5R5A1_UNORM;
        use windows::core::{PCSTR, s};

        const SOURCE: &str = r#"
float4 vs(uint vertex : SV_VertexID) : SV_Position {
    return float4(vertex == 1 ? 3.0 : -1.0, vertex == 2 ? -3.0 : 1.0, 0.5, 1.0);
}
float4 ps() : SV_Target0 {
    return float4(1.0, 0.0, 0.0, 1.0);
}
"#;

        let Some(host) = crate::host::host() else {
            return;
        };
        let make_shader = |stage, hw_stage, entry: PCSTR, profile: PCSTR| {
            let mut blob: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            // SAFETY: the HLSL source, C strings, and output locations live through compilation.
            let result = unsafe {
                D3DCompile(
                    SOURCE.as_ptr().cast(),
                    SOURCE.len(),
                    s!("packed-rtv-variant"),
                    None,
                    None,
                    entry,
                    profile,
                    0,
                    0,
                    &mut blob,
                    Some(&mut errors),
                )
            };
            if let Err(error) = result {
                let detail = errors.as_ref().map(|blob| {
                    // SAFETY: the diagnostic blob owns this range.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            blob.GetBufferPointer().cast::<u8>(),
                            blob.GetBufferSize(),
                        )
                    };
                    String::from_utf8_lossy(bytes)
                });
                panic!("{stage} compilation failed: {error:?} {detail:?}");
            }
            let blob = blob.unwrap();
            // SAFETY: the compiled blob owns this range until copied.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                )
            };
            Arc::new(Shader {
                stage,
                hash: blake3::hash(bytes).to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(feature = "diagnostics")]
                wave64: false.into(),
                translated: OnceLock::from(Some(Translated {
                    compute_threads: None,
                    host: victory_shader::emit::HostShader {
                        stage: hw_stage,
                        dxil: bytes.to_vec(),
                        user_data: Vec::new(),
                        samplers: Vec::new(),
                        static_samplers: Vec::new(),
                        root_cbvs: Vec::new(),
                        root_srvs: Vec::new(),
                        root_uavs: Vec::new(),
                    },
                    used: Vec::new(),
                })),
            })
        };
        let vs = make_shader(
            "vertex",
            victory_shader::HwStage::Vs,
            s!("vs"),
            s!("vs_5_1"),
        );
        let ps = make_shader("pixel", victory_shader::HwStage::Ps, s!("ps"), s!("ps_5_1"));

        #[repr(C)]
        struct Subobject<T> {
            kind: u32,
            desc: T,
        }
        let face = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        let depth = Subobject {
            kind: 11,
            desc: D3D12_DEPTH_STENCIL_DESC {
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                FrontFace: face,
                BackFace: face,
                ..Default::default()
            },
        };
        let raster = Subobject {
            kind: 10,
            desc: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
        };
        let mut targets = Subobject {
            kind: 15,
            desc: D3D12_RT_FORMAT_ARRAY {
                NumRenderTargets: 1,
                ..Default::default()
            },
        };
        targets.desc.RTFormats[0] = DXGI_FORMAT_B5G5R5A1_UNORM;
        let mut graphics = host::GraphicsState::default();
        graphics.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        // SAFETY: stack values contain complete stream subobjects in the expected layout.
        unsafe {
            graphics.read(depth.kind, (&raw const depth).cast());
            graphics.read(raster.kind, (&raw const raster).cast());
            graphics.read(targets.kind, (&raw const targets).cast());
        }
        let mut guest_pipeline = std::ptr::null();
        assert_eq!(
            hand_out(
                vec![vs, ps],
                Some(Box::new(graphics)),
                &PIPELINE_IIDS[0],
                &mut guest_pipeline
            ),
            S_OK
        );

        let mut cmd = packed_test::GuestCmd::new();
        let rt = cmd.make_texture(1, 1, 86, 1, 1, 1, 1);
        let rt_uav = cmd.make_texture(1, 1, 86, 5, 1, 1, 1);
        let resources = [rt, rt_uav];
        let formats = [
            packed_test::physical_format(rt),
            packed_test::physical_format(rt_uav),
        ];
        assert_eq!(formats[0], target_format(86, 1, (1, 0)));
        assert_eq!(formats[1], target_format(86, 5, (1, 0)));
        eprintln!(
            "adapter: {}, f86 flags=1 -> f{}, flags=5 -> f{}",
            packed_test::adapter_name(host),
            formats[0],
            formats[1]
        );

        let make_heap = |ty| -> ID3D12DescriptorHeap {
            // SAFETY: valid shader-visible heap descriptor on a live device.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: ty,
                        NumDescriptors: 1,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        ..Default::default()
                    })
            }
            .unwrap()
        };
        let heaps = [
            Some(make_heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)),
            Some(make_heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)),
        ];

        for (order, explicit) in [([0usize, 1], true), ([1, 0], false)] {
            let list = host.new_list().unwrap();
            let mut state = crate::draw::State::default();
            let mut records = [[0u64; 14]; 2];
            // SAFETY: the list is open and the shader-visible heaps outlive submission.
            unsafe {
                list.list.SetDescriptorHeaps(&heaps);
                list.list.RSSetViewports(&[D3D12_VIEWPORT {
                    Width: 1.0,
                    Height: 1.0,
                    MaxDepth: 1.0,
                    ..Default::default()
                }]);
                list.list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                }]);
            }
            state.set_topology(&list, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST.0 as u32);
            state.set_pipeline(guest_pipeline);
            for index in order {
                let guest = resources[index];
                let image = crate::resource::host_resource(guest).unwrap();
                crate::list::ensure_state(
                    &list,
                    guest,
                    D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
                );
                let clear_view = host.rtv(Some(image), None);
                // SAFETY: texture is in RENDER_TARGET state and clear_view is live.
                unsafe {
                    list.list
                        .ClearRenderTargetView(clear_view, &[0.0, 1.0, 0.0, 1.0], None);
                }
                let desc = [
                    DXGI_FORMAT_B5G5R5A1_UNORM.0 as u32,
                    D3D12_RTV_DIMENSION_TEXTURE2D.0 as u32,
                    0,
                    0,
                    0,
                    0,
                ];
                let handle = records[index].as_mut_ptr() as u64;
                crate::descriptor::create_rtv(
                    std::ptr::null(),
                    guest,
                    if explicit {
                        desc.as_ptr().cast()
                    } else {
                        std::ptr::null()
                    },
                    handle,
                );
                state.set_targets(&list, &[handle], 0);
                state.draw(&list, 3, 1, 0, 0);
            }
            packed_test::assert_no_d3d12_errors(host);
            list.close();
            host.execute(&[&list]);

            for (i, &guest) in resources.iter().enumerate() {
                let bytes =
                    packed_test::readback_subresource(host, guest, 0, formats[i], 1, 1, 256);
                packed_test::assert_texels(86, formats[i], &bytes, 256, 1, 1, |_, _| 0xfc00);
            }
            packed_test::assert_no_d3d12_errors(host);
        }

        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(guest_pipeline, 0x10)(guest_pipeline),
            0
        );
    }
}
