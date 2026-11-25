//! Translated shaders and translation refusals kept on disk (decision 0003):
//! under the configured cache folder, keyed by the container's content and by
//! the VIctory build that translated it (the BLAKE3 of our executable and
//! translation settings), so a rebuilt translator never serves an old
//! translation or an old refusal. Entries go through `victory-pipeline`'s
//! content-addressed cache, which writes them atomically; an entry that does
//! not read back is translated again.
//!
//! Each entry is our own encoding of `Result<HostShader, Refusal>`: a field
//! added to [`HostShader`] must be added to `encode` and `decode` too (entries
//! of older builds are ignored anyway).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Whether the active host adapter supports `WaveSize(32)` compute shaders.
fn allow_wave32() -> bool {
    #[cfg(windows)]
    {
        crate::host::host().is_none_or(|h| h.supports_wave32())
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// What identifies this build: the BLAKE3 of our executable file and of
/// the translation settings it runs with (the loop limit and the clean-up
/// passes, which a diagnostics build can change at run time, and whether
/// Wave32 lowering is enabled for the active host adapter).
fn build(wave32: bool) -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let exe = std::env::current_exe().and_then(std::fs::read);
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        match exe {
            Ok(mut bytes) => {
                bytes.extend_from_slice(&limit.to_le_bytes());
                bytes.extend_from_slice(&victory_shader::emit::shader_opt().to_le_bytes());
                bytes.push(u8::from(wave32));
                *blake3::hash(&bytes).as_bytes()
            }
            Err(e) => {
                // Without it no entry can be trusted: a fresh key per run.
                tracing::warn!(error = %e, "cannot read our executable; shader cache unused");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                *blake3::hash(&now.as_nanos().to_le_bytes()).as_bytes()
            }
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) reason: String,
    /// At least one lowering attempt asked for a required, statically
    /// addressable view stride. Only then can the four-byte probe change
    /// the translation path.
    pub(crate) required_stride: bool,
}

impl Refusal {
    pub(crate) fn fixed(reason: impl ToString) -> Self {
        Self {
            reason: reason.to_string(),
            required_stride: false,
        }
    }
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, Refusal> {
    let wave32 = allow_wave32();
    let translate = |c: &[u8]| {
        let required_stride = std::cell::Cell::new(false);
        victory_shader::emit::translate_with_view_strides(c, wave32, &|_, required| {
            if required {
                required_stride.set(true);
            }
            None
        })
        .map(|(shader, _)| shader)
        .map_err(|error| Refusal {
            reason: error.to_string(),
            required_stride: required_stride.get(),
        })
    };
    match CACHE.get() {
        Some(cache) => cached(cache, build(wave32), container, translate),
        None => translate(container),
    }
}

/// The cache entry's converter: the build and the container in, the encoded
/// translation or refusal out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
    /// The last translation outcome produced by `convert`, kept so a cache
    /// write failure returns it directly instead of re-running translation.
    result: Mutex<Option<Result<HostShader, Refusal>>>,
}

impl<F: Fn(&[u8]) -> Result<HostShader, Refusal> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        2
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let outcome = (self.translate)(&input[32..]);
        let bytes = encode(&outcome);
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        Ok(bytes)
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, Refusal> + Send + Sync,
) -> Result<HostShader, Refusal> {
    let input = [&build[..], container].concat();
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
        result: Mutex::new(None),
    };
    for _ in 0..2 {
        converter.ran.store(false, Ordering::Relaxed);
        *converter.result.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let path = match cache.get_or_convert(&converter, &input) {
            Ok(path) => path,
            // A full disk, a file in the way, a scanner holding the new
            // entry: the translation outcome is still valid, only not kept.
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "shader cache entry not written");
                break;
            }
        };
        if let Some(outcome) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let ran = converter.ran.load(Ordering::Relaxed);
            let counter = if ran { &MISSES } else { &HITS };
            let what = match (outcome.is_ok(), ran) {
                (true, true) => "translated and cached",
                (true, false) => "from the cache",
                (false, true) => "refusal cached",
                (false, false) => "refusal from the cache",
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return outcome;
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    converter
        .result
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_else(|| (converter.translate)(container))
}

const MAGIC: &[u8; 4] = b"VSH2";
const REFUSAL_MAGIC: &[u8; 4] = b"VSE2";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(outcome: &Result<HostShader, Refusal>) -> Vec<u8> {
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let s = match outcome {
        Ok(s) => s,
        Err(refusal) => {
            let mut out = REFUSAL_MAGIC.to_vec();
            out.push(u8::from(refusal.required_stride));
            word(&mut out, refusal.reason.len() as u32);
            out.extend_from_slice(refusal.reason.as_bytes());
            return out;
        }
    };
    let mut out = MAGIC.to_vec();
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [
            u.index,
            u32::from(u.slot),
            root_code(u.root),
            u.dword,
            u32::from(u.indexed_buffer),
        ] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Result<HostShader, Refusal>> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    let magic = r.take(4)?;
    if magic == REFUSAL_MAGIC || magic == b"VSE1" {
        // Older refusals carried only a message and always went through the probe.
        let required_stride = if magic == b"VSE1" {
            true
        } else {
            match r.take(1)? {
                [0] => false,
                [1] => true,
                _ => return None,
            }
        };
        let raw = r.list()?;
        let reason = String::from_utf8(raw).ok()?;
        return r.0.is_empty().then_some(Err(Refusal {
            reason,
            required_stride,
        }));
    }
    if magic != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
            indexed_buffer: match r.word()? {
                0 => false,
                1 => true,
                _ => return None,
            },
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    r.0.is_empty().then_some(Ok(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                indexed_buffer: true,
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH2 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn refusals_come_back_from_the_cache_until_the_build_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build_v1 = [7u8; 32];
        let build_v2 = [8u8; 32];
        let container = b"a refused console container".as_slice();
        let runs = AtomicU64::new(0);
        let refuse = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Err::<HostShader, _>(Refusal::fixed("unsupported: test refusal"))
        };

        assert_eq!(
            cached(&cache, &build_v1, container, refuse),
            Err(Refusal::fixed("unsupported: test refusal"))
        );
        assert_eq!(
            cached(&cache, &build_v1, container, refuse),
            Err(Refusal::fixed("unsupported: test refusal"))
        );
        assert_eq!(
            runs.load(Ordering::Relaxed),
            1,
            "the second refusal came from disk"
        );

        // A new translator build retries the container without clearing the cache.
        let fixed = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build_v2, container, fixed), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(cached(&cache, &build_v2, container, fixed), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged refusal entry is removed and re-translated.
        let damaged_dir = tempfile::tempdir().unwrap();
        let damaged_cache = Cache::open(damaged_dir.path()).unwrap();
        let damaged_runs = AtomicU64::new(0);
        let refuse_again = |_: &[u8]| {
            damaged_runs.fetch_add(1, Ordering::Relaxed);
            Err::<HostShader, _>(Refusal::fixed("unsupported: damaged"))
        };
        assert!(cached(&damaged_cache, &build_v1, container, refuse_again).is_err());
        assert_eq!(damaged_runs.load(Ordering::Relaxed), 1);
        for e in walk(damaged_dir.path()) {
            std::fs::write(e, b"VSE1 broken").unwrap();
        }
        assert_eq!(
            cached(&damaged_cache, &build_v1, container, refuse_again),
            Err(Refusal::fixed("unsupported: damaged"))
        );
        assert_eq!(damaged_runs.load(Ordering::Relaxed), 2);
        assert_eq!(
            cached(&damaged_cache, &build_v1, container, refuse_again),
            Err(Refusal::fixed("unsupported: damaged"))
        );
        assert_eq!(damaged_runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_cache_that_cannot_keep_an_entry_still_gives_the_translation() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        // A file where every entry's folder would go: nothing can be written.
        for shard in 0..=0xff_u8 {
            std::fs::write(dir.path().join(format!("{shard:02x}")), b"").unwrap();
        }
        let ok_runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            ok_runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let container = b"a console container".as_slice();
        assert_eq!(cached(&cache, &[7; 32], container, translate), Ok(shader()));
        assert_eq!(ok_runs.load(Ordering::Relaxed), 1);
        // A shader that does not translate still fails once, with its own error.
        let fail_runs = AtomicU64::new(0);
        let fail = |_: &[u8]| {
            fail_runs.fetch_add(1, Ordering::Relaxed);
            Err::<HostShader, _>(Refusal::fixed("unsupported"))
        };
        let error = cached(&cache, &[7; 32], b"other", fail).unwrap_err();
        assert_eq!(error, Refusal::fixed("unsupported"));
        assert_eq!(fail_runs.load(Ordering::Relaxed), 1);
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let ok = Ok(shader());
        let bytes = encode(&ok);
        assert_eq!(decode(&bytes), Some(ok));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);

        let err: Result<HostShader, Refusal> = Err(Refusal::fixed("unsupported: opcode"));
        let err_bytes = encode(&err);
        assert_eq!(decode(&err_bytes), Some(err));
        assert_eq!(decode(&err_bytes[..err_bytes.len() - 1]), None);
        assert_eq!(decode(&[err_bytes.as_slice(), &[0]].concat()), None);

        let retry = Err(Refusal {
            reason: "needs a view".into(),
            required_stride: true,
        });
        assert_eq!(decode(&encode(&retry)), Some(retry));
        let legacy = [b"VSE1".as_slice(), &2u32.to_le_bytes(), b"no"].concat();
        assert_eq!(
            decode(&legacy),
            Some(Err(Refusal {
                reason: "no".into(),
                required_stride: true
            }))
        );
        let mut invalid = err_bytes.clone();
        invalid[4] = 2;
        assert!(decode(&invalid).is_none());

        let empty_err: Result<HostShader, Refusal> = Err(Refusal::fixed(""));
        assert_eq!(decode(&encode(&empty_err)), Some(empty_err));

        // Invalid UTF-8 inside a refusal entry is treated as damaged.
        let mut bad_utf8 = REFUSAL_MAGIC.to_vec();
        bad_utf8.push(0);
        bad_utf8.extend(2u32.to_le_bytes());
        bad_utf8.extend([0xff, 0xfe]);
        assert_eq!(decode(&bad_utf8), None);
    }
}
