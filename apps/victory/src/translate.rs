//! The offline batch: every shader found goes through the translator, and
//! what it cannot handle yet is tallied (decision 0003).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use victory_shader::emit;
#[cfg(windows)]
use victory_shader::emit::host::{self, Host};
#[cfg(windows)]
use victory_shader::emit::validator::Validator;
use victory_shader::structure::{self, Atom, LaneExit};
use victory_shader::{
    Container, FourCC, HwStage, NativeProgram, RootType, ShaderError, ir, native_programs,
};
use victory_shader::{io, resource};

use crate::shader::{MAX_FILE, Source, scan_entries, sources};

/// Per-stage program counts.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct StageCounts {
    vs: usize,
    ps: usize,
    cs: usize,
    ms: usize,
    ls: usize,
    hs: usize,
    es: usize,
    gs: usize,
}

impl StageCounts {
    fn record(&mut self, stage: HwStage) {
        match stage {
            HwStage::Vs => self.vs += 1,
            HwStage::Ps => self.ps += 1,
            HwStage::Cs => self.cs += 1,
            HwStage::Ms => self.ms += 1,
            HwStage::Ls => self.ls += 1,
            HwStage::Hs => self.hs += 1,
            HwStage::Es => self.es += 1,
            HwStage::Gs => self.gs += 1,
        }
    }

    fn merge(&mut self, other: &StageCounts) {
        self.vs += other.vs;
        self.ps += other.ps;
        self.cs += other.cs;
        self.ms += other.ms;
        self.ls += other.ls;
        self.hs += other.hs;
        self.es += other.es;
        self.gs += other.gs;
    }

    fn total(&self) -> usize {
        self.vs + self.ps + self.cs + self.ms + self.ls + self.hs + self.es + self.gs
    }

    /// Stage breakdown printed on each program-level reason line.
    fn format_reason_breakdown(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let mut out = format!(
            "vs {}, ps {}, cs {}, ms {}",
            self.vs, self.ps, self.cs, self.ms
        );
        for (name, count) in [
            ("ls", self.ls),
            ("hs", self.hs),
            ("es", self.es),
            ("gs", self.gs),
        ] {
            if count > 0 {
                out.push_str(&format!(", {name} {count}"));
            }
        }
        Some(out)
    }

    /// Breakdown of non-zero unemitted stages (`LS`, `HS`, `ES`, `GS`).
    fn format_unemitted(&self) -> String {
        let mut parts = Vec::new();
        for (name, count) in [
            ("LS", self.ls),
            ("HS", self.hs),
            ("ES", self.es),
            ("GS", self.gs),
        ] {
            if count > 0 {
                parts.push(format!("{name} {count}"));
            }
        }
        parts.join(", ")
    }
}

/// Programs stopped by one reason, split by stage, with the first example.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failed {
    total: usize,
    stages: StageCounts,
    example: String,
}

fn is_emitted_stage(stage: HwStage) -> bool {
    matches!(stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms)
}

#[derive(Default)]
struct Tally {
    containers: usize,
    duplicates: usize,
    programs: usize,
    translated: usize,
    instructions: usize,
    structured: usize,
    vector: usize,
    guarded: usize,
    /// Guards that read a mask the pass cannot see into.
    opaque: usize,
    lane_branches: usize,
    uniform_branches: usize,
    tracked: usize,
    loaded: usize,
    built: usize,
    constant: usize,
    /// Descriptors and scalar accesses with a computed address term.
    dynamic: usize,
    scalar: usize,
    used: usize,
    /// Used-resource entries whose root parameter no descriptor traces
    /// to, by resource and root type.
    untraced_used: BTreeMap<String, usize>,
    /// Root parameters descriptors trace to that no used entry names.
    unlisted: usize,
    /// Vertex and pixel programs, and those whose I/O was raised.
    io_programs: usize,
    io_raised: usize,
    interpolated: usize,
    weighted_steps: usize,
    vertex_values: usize,
    /// Exports by target kind.
    exports: BTreeMap<&'static str, usize>,
    /// Exports still packed as f16 pairs after the export patch.
    compressed: usize,
    /// Vertex and pixel programs every earlier stage handled, and how far
    /// emission got with them.
    emit_programs: usize,
    emitted: usize,
    validated: usize,
    pipelines: usize,
    /// The same for compute programs.
    cs_programs: usize,
    cs_emitted: usize,
    cs_validated: usize,
    cs_pipelines: usize,
    /// The same for mesh programs.
    ms_programs: usize,
    ms_emitted: usize,
    ms_validated: usize,
    ms_pipelines: usize,
    /// Programs in stages the emitter does not emit (`LS`, `HS`, `ES`, `GS`)
    /// that were not refused by an earlier pass.
    not_emitted: StageCounts,
    /// Distinct programs refused before or during emission.
    refused: usize,
    /// Emitted programs rejected by the DXIL validator (`--validate`).
    validator_rejected: usize,
    /// Reason → programs stopped by it, split by stage, and the first one.
    failed: BTreeMap<String, Failed>,
    /// With `--list`: per-program result rows, including runtime hashes.
    list: bool,
    lines: Vec<String>,
}

impl Tally {
    fn fail(&mut self, stage: Option<HwStage>, reason: String, example: impl FnOnce() -> String) {
        let entry = if self.list {
            let e = example();
            self.lines.push(format!("{e}  {reason}"));
            self.failed.entry(reason).or_insert(Failed {
                total: 0,
                stages: StageCounts::default(),
                example: e,
            })
        } else {
            self.failed.entry(reason).or_insert_with(|| Failed {
                total: 0,
                stages: StageCounts::default(),
                example: example(),
            })
        };
        entry.total += 1;
        if let Some(stage) = stage {
            entry.stages.record(stage);
        }
    }

    fn emitted_total(&self) -> usize {
        self.emitted + self.cs_emitted + self.ms_emitted
    }

    fn validated_total(&self) -> usize {
        self.validated + self.cs_validated + self.ms_validated
    }

    fn summary_lines(&self, validate: bool) -> Vec<String> {
        let not_emitted = self.not_emitted.total();
        let not_emitted_line = if not_emitted > 0 {
            format!(
                "             {not_emitted} programs in stages not emitted ({})",
                self.not_emitted.format_unemitted()
            )
        } else {
            "             0 programs in stages not emitted".to_owned()
        };
        let emitted = self.emitted_total();
        let outcome_line = if validate || self.validator_rejected > 0 {
            format!(
                "             {emitted} emitted ({} validated, {} rejected by validator), \
                 {} refused, {not_emitted} in stages not emitted ({} total)",
                self.validated_total(),
                self.validator_rejected,
                self.refused,
                self.programs
            )
        } else {
            format!(
                "             {emitted} emitted, {} refused, {not_emitted} in stages not emitted \
                 ({} total)",
                self.refused, self.programs
            )
        };
        vec![
            format!(
                "containers   {} ({} duplicates skipped)",
                self.containers, self.duplicates
            ),
            format!(
                "programs     {} translated of {} ({} IR instructions)",
                self.translated, self.programs, self.instructions
            ),
            format!(
                "structured   {} of {} ({} vector instructions, {} guarded, {} on opaque masks; \
                 branches {} per lane, {} uniform)",
                self.structured,
                self.translated,
                self.vector,
                self.guarded,
                self.opaque,
                self.lane_branches,
                self.uniform_branches
            ),
            format!(
                "resources    {} of {} tracked (descriptors {} loaded, {} built, {} constant; \
                 {} scalar accesses; {} with a computed term)",
                self.tracked,
                self.translated,
                self.loaded,
                self.built,
                self.constant,
                self.scalar,
                self.dynamic
            ),
            format!(
                "  used-resource entries {}: {} not traced {:?}; {} traced parameters not listed",
                self.used,
                self.untraced_used.values().sum::<usize>(),
                self.untraced_used,
                self.unlisted
            ),
            format!(
                "I/O          {} of {} vertex and pixel programs raised ({} interpolations, \
                 {} weighted steps, {} vertex values; exports {:?}, {} still f16 pairs)",
                self.io_raised,
                self.io_programs,
                self.interpolated,
                self.weighted_steps,
                self.vertex_values,
                self.exports,
                self.compressed
            ),
            format!(
                "DXIL         {} emitted of {} vertex and pixel programs; {} validated, {} pipelines",
                self.emitted, self.emit_programs, self.validated, self.pipelines
            ),
            format!(
                "             {} emitted of {} compute programs; {} validated, {} pipelines",
                self.cs_emitted, self.cs_programs, self.cs_validated, self.cs_pipelines
            ),
            format!(
                "             {} emitted of {} mesh programs; {} validated, {} pipelines",
                self.ms_emitted, self.ms_programs, self.ms_validated, self.ms_pipelines
            ),
            not_emitted_line,
            outcome_line,
        ]
    }

    fn failure_lines(&self) -> Vec<String> {
        let mut failed: Vec<_> = self.failed.iter().collect();
        failed.sort_by_key(|(_, f)| std::cmp::Reverse(f.total));
        failed
            .into_iter()
            .map(|(reason, f)| match f.stages.format_reason_breakdown() {
                Some(stages) => {
                    format!(
                        "  {:>7}  {reason}  [{stages}]  (e.g. {})",
                        f.total, f.example
                    )
                }
                None => format!("  {:>7}  {reason}  (e.g. {})", f.total, f.example),
            })
            .collect()
    }

    fn merge(&mut self, other: Tally) {
        self.containers += other.containers;
        self.duplicates += other.duplicates;
        self.programs += other.programs;
        self.translated += other.translated;
        self.instructions += other.instructions;
        self.structured += other.structured;
        self.vector += other.vector;
        self.guarded += other.guarded;
        self.opaque += other.opaque;
        self.lane_branches += other.lane_branches;
        self.uniform_branches += other.uniform_branches;
        self.tracked += other.tracked;
        self.loaded += other.loaded;
        self.built += other.built;
        self.constant += other.constant;
        self.dynamic += other.dynamic;
        self.scalar += other.scalar;
        self.used += other.used;
        for (k, n) in other.untraced_used {
            *self.untraced_used.entry(k).or_default() += n;
        }
        self.unlisted += other.unlisted;
        self.io_programs += other.io_programs;
        self.io_raised += other.io_raised;
        self.interpolated += other.interpolated;
        self.weighted_steps += other.weighted_steps;
        self.vertex_values += other.vertex_values;
        self.compressed += other.compressed;
        self.emit_programs += other.emit_programs;
        self.emitted += other.emitted;
        self.validated += other.validated;
        self.pipelines += other.pipelines;
        self.cs_programs += other.cs_programs;
        self.cs_emitted += other.cs_emitted;
        self.cs_validated += other.cs_validated;
        self.cs_pipelines += other.cs_pipelines;
        self.ms_programs += other.ms_programs;
        self.ms_emitted += other.ms_emitted;
        self.ms_validated += other.ms_validated;
        self.ms_pipelines += other.ms_pipelines;
        self.not_emitted.merge(&other.not_emitted);
        self.refused += other.refused;
        self.validator_rejected += other.validator_rejected;
        for (k, n) in other.exports {
            *self.exports.entry(k).or_default() += n;
        }
        self.lines.extend(other.lines);
        for (reason, f) in other.failed {
            let entry = self.failed.entry(reason).or_insert_with(|| Failed {
                total: 0,
                stages: StageCounts::default(),
                example: f.example,
            });
            entry.total += f.total;
            entry.stages.merge(&f.stages);
        }
    }
}

/// Emitted shaders by label, shared between threads.
type Kept = std::sync::Arc<Mutex<Vec<(String, emit::HostShader)>>>;

/// The optional checks after emission, one set per thread.
struct Checks {
    /// Shaders named in `VICTORY_PAIRS` (`vs+ps,...`, label substrings),
    /// kept to build those pipelines after the walk.
    keep: Option<Kept>,
    validator: Option<Validator>,
    host: Option<(Host, emit::HostShader)>,
}

pub fn run(
    paths: Vec<PathBuf>,
    validate: bool,
    pipelines: bool,
    list: bool,
    jobs: Option<usize>,
) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    let dll = validate.then(Validator::default_path).flatten();
    if validate {
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
        println!(
            "validator    {}",
            dll.as_ref()
                .map_or("dxil.dll".into(), |p| p.display().to_string())
        );
    }
    if pipelines {
        Host::new(false).map_err(anyhow::Error::msg)?;
    }
    let pairs: Vec<(String, String)> = std::env::var("VICTORY_PAIRS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.split_once('+'))
        .map(|(v, p)| (v.trim().to_owned(), p.trim().to_owned()))
        .collect();
    let kept = std::sync::Arc::new(Mutex::new(Vec::new()));
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let seen = Mutex::new(HashSet::new());
    let total = Mutex::new(Tally::default());
    let mut walk_errors = 0;
    for root in &paths {
        for entry in scan_entries(root, &mut walk_errors) {
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            let jobs = sources(&data);
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads.min(jobs.len()) {
                    s.spawn(|| {
                        let mut t = Tally {
                            list,
                            ..Tally::default()
                        };
                        let mut checks = Checks {
                            keep: (!pairs.is_empty()).then(|| kept.clone()),
                            validator: validate
                                .then(|| Validator::load(dll.as_deref()).ok())
                                .flatten(),
                            host: pipelines
                                .then(|| Host::new(false).ok().map(|h| (h, host::stub_ps())))
                                .flatten(),
                        };
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            translate(job, entry.path(), &seen, &mut t, &mut checks);
                        }
                        lock(&total).merge(t);
                    });
                }
            });
        }
    }
    let mut t = total.into_inner().unwrap_or_else(|e| e.into_inner());
    for line in t.summary_lines(validate) {
        println!("{line}");
    }
    if !pairs.is_empty() {
        let host = Host::new(false).map_err(anyhow::Error::msg)?;
        let kept = lock(&kept);
        let find = |name: &str| kept.iter().find(|(l, _)| l.contains(name));
        for (v, p) in &pairs {
            let made = match (find(v), find(p)) {
                (Some((_, vs)), Some((_, ps))) => if vs.stage == HwStage::Ms {
                    host.mesh_pipeline(vs, ps)
                } else {
                    host.pipeline(vs, ps)
                }
                .map(|_| "pipeline created".to_owned())
                .unwrap_or_else(|e| e),
                _ => "not emitted".to_owned(),
            };
            println!("pair {v} + {p}: {made}");
        }
    }
    t.lines.sort();
    for l in &t.lines {
        println!("{l}");
    }
    for line in t.failure_lines() {
        println!("{line}");
    }
    anyhow::ensure!(
        walk_errors == 0,
        "scan incomplete (traversal errors: {walk_errors})"
    );
    Ok(())
}

fn translate(
    source: &Source,
    path: &Path,
    seen: &Mutex<HashSet<blake3::Hash>>,
    t: &mut Tally,
    checks: &mut Checks,
) {
    let location = || format!("{}{}", path.display(), source.label());
    let Some(bytes) = source.bytes() else {
        return t.fail(None, "corrupt record".into(), location);
    };
    let list = t.list;
    let hash = list.then(|| blake3::hash(&bytes).to_hex().to_string());
    let example = || {
        let location = location();
        match &hash {
            Some(hash) => format!("{location} hash={}", &hash[..16]),
            None => location,
        }
    };
    t.containers += 1;
    let Ok(container) = Container::parse(&bytes) else {
        return t.fail(None, "no DXIL part (SM5 or older)".into(), example);
    };
    let Some(dxil) = container.part(FourCC::DXIL).map(|p| p.data) else {
        return t.fail(None, "no DXIL part (SM5 or older)".into(), example);
    };
    if !lock(seen).insert(blake3::hash(dxil)) && !list {
        t.duplicates += 1;
        return;
    }
    let programs = match native_programs(dxil) {
        Ok(p) => p,
        Err(e) => return t.fail(None, e.to_string(), example),
    };
    for p in programs {
        tally_program(&container, &p, &example, t, checks);
    }
}

fn tally_program(
    container: &Container<'_>,
    p: &NativeProgram,
    example: &impl Fn() -> String,
    t: &mut Tally,
    checks: &mut Checks,
) {
    t.programs += 1;
    match victory_shader::spill::build(p) {
        Ok(program) => {
            t.translated += 1;
            t.instructions += program.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
            // Per-lane descriptors are traced on the per-lane form; its
            // failure is listed after the resources' and I/O's.
            let structured = structure::structure(&program);
            let lanes = structured.as_ref().ok();
            let res = match resource::track(&program, &p.layout, p.stage, lanes) {
                Ok(r) => {
                    tally_resources(p, &r, t);
                    Some(r)
                }
                Err(ShaderError::Unsupported { offset, what }) => {
                    t.fail(Some(p.stage), format!("resources: {what}"), || {
                        format!("{} {:?} at {offset:#x}", example(), p.stage)
                    });
                    None
                }
                Err(e) => {
                    t.fail(Some(p.stage), e.to_string(), || {
                        format!("{} {:?}", example(), p.stage)
                    });
                    None
                }
            };
            let graphics = matches!(p.stage, HwStage::Vs | HwStage::Ps);
            let mut raised = None;
            if is_emitted_stage(p.stage) {
                t.io_programs += usize::from(graphics);
                match io::raise(&program, p) {
                    Ok(r) => {
                        if graphics {
                            tally_io(&r, t);
                        }
                        raised = Some(r);
                    }
                    Err(ShaderError::Unsupported { offset, what }) => {
                        t.fail(Some(p.stage), format!("I/O: {what}"), || {
                            format!("{} {:?} at {offset:#x}", example(), p.stage)
                        });
                    }
                    Err(e) => t.fail(Some(p.stage), e.to_string(), || {
                        format!("{} {:?}", example(), p.stage)
                    }),
                }
            }
            let structured = match structured {
                Ok(s) => {
                    tally_structured(&program, &s, t);
                    Some(s)
                }
                Err(e) => {
                    let reason = match e {
                        ShaderError::Unsupported { what, .. } => format!("structure: {what}"),
                        ShaderError::Isa { reason, .. } => format!("structure: {reason}"),
                        e => e.to_string(),
                    };
                    t.fail(Some(p.stage), reason, || {
                        format!("{} {:?}", example(), p.stage)
                    });
                    None
                }
            };
            if !is_emitted_stage(p.stage) {
                if res.is_some() && structured.is_some() {
                    t.not_emitted.record(p.stage);
                    if t.list {
                        t.lines
                            .push(format!("{} {:?}  stage not emitted", example(), p.stage));
                    }
                } else {
                    t.refused += 1;
                }
            } else if let (Some(r), Some(i), Some(s)) = (res, raised, structured) {
                let label = || format!("{} {:?}", example(), p.stage);
                *counter(t, p.stage, 0) += 1;
                match emit::emit(container, p, &program, &s, &i, &r) {
                    Ok(shader) => {
                        *counter(t, p.stage, 1) += 1;
                        check(shader, t, checks, label);
                    }
                    Err(ShaderError::Unsupported { offset, what }) => {
                        t.refused += 1;
                        t.fail(Some(p.stage), format!("emit: {what}"), || {
                            format!("{} at {offset:#x}", label())
                        });
                    }
                    Err(e) => {
                        t.refused += 1;
                        t.fail(Some(p.stage), format!("emit: {e}"), label);
                    }
                }
            } else {
                t.refused += 1;
            }
        }
        Err(e) => {
            t.refused += 1;
            let reason = match e {
                ShaderError::Unsupported { what, .. } => format!("unsupported: {what}"),
                e => e.to_string(),
            };
            t.fail(Some(p.stage), reason, || {
                format!("{} {:?}", example(), p.stage)
            });
        }
    }
}

/// Validates an emitted shader if asked to.
fn check(
    mut shader: emit::HostShader,
    t: &mut Tally,
    checks: &mut Checks,
    label: impl Fn() -> String,
) {
    if let Some(keep) = &checks.keep {
        lock(keep).push((label(), shader.clone()));
    }
    // `VICTORY_EMIT_DUMP` with `--list`: every emitted shader as a file.
    if t.list
        && let Ok(dir) = std::env::var("VICTORY_EMIT_DUMP")
    {
        let name: String = label()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let _ = std::fs::write(
            format!("{dir}/{}.dxil", &name[name.len().saturating_sub(100)..]),
            &shader.dxil,
        );
    }
    if let Some((h, stub_ps)) = &checks.host {
        let made = match shader.stage {
            HwStage::Vs => h.pipeline(&shader, stub_ps).map(drop),
            HwStage::Cs => h.compute_pipeline(&shader).map(drop),
            HwStage::Ms => h.mesh_pipeline(&shader, stub_ps).map(drop),
            _ => host::stub_vs(&shader).and_then(|vs| h.pipeline(&vs, &shader).map(drop)),
        };
        match made {
            Ok(()) => {
                *counter(t, shader.stage, 3) += 1;
                if t.list {
                    t.lines.push(format!("{}  pipeline created", label()));
                }
            }
            Err(e) => t.fail(Some(shader.stage), format!("pipeline: {e}"), &label),
        }
    }
    let Some(v) = checks.validator.as_mut() else {
        return;
    };
    match v.validate(&mut shader.dxil) {
        Ok(()) => {
            *counter(t, shader.stage, 2) += 1;
            if t.list {
                t.lines.push(format!("{}  validated", label()));
            }
        }
        Err(text) => {
            let first = text
                .lines()
                .find(|l| l.contains("error"))
                .unwrap_or(text.lines().next().unwrap_or(""))
                .to_owned();
            if let Ok(dir) = std::env::var("VICTORY_EMIT_DUMP") {
                let n = t
                    .failed
                    .get(&format!("validator: {first}"))
                    .map_or(0, |f| f.total);
                if n < 3 {
                    let name = format!(
                        "{dir}/{}_{n}",
                        first
                            .chars()
                            .filter(char::is_ascii_alphanumeric)
                            .take(40)
                            .collect::<String>()
                    );
                    let _ = std::fs::write(format!("{name}.dxil"), &shader.dxil);
                    let _ = std::fs::write(format!("{name}.txt"), format!("{}\n{text}", label()));
                }
            }
            t.validator_rejected += 1;
            t.fail(Some(shader.stage), format!("validator: {first}"), label);
        }
    }
}

/// Emission counts of `stage`'s kind: 0 programs, 1 emitted, 2 validated,
/// 3 pipelines.
fn counter(t: &mut Tally, stage: HwStage, k: usize) -> &mut usize {
    let [a, b, c, d] = match stage {
        HwStage::Cs => [
            &mut t.cs_programs,
            &mut t.cs_emitted,
            &mut t.cs_validated,
            &mut t.cs_pipelines,
        ],
        HwStage::Ms => [
            &mut t.ms_programs,
            &mut t.ms_emitted,
            &mut t.ms_validated,
            &mut t.ms_pipelines,
        ],
        _ => [
            &mut t.emit_programs,
            &mut t.emitted,
            &mut t.validated,
            &mut t.pipelines,
        ],
    };
    [a, b, c, d].into_iter().nth(k).expect("0 to 3")
}

fn tally_io(r: &io::Io, t: &mut Tally) {
    t.io_raised += 1;
    for (_, i) in &r.interps {
        match i {
            io::Interp::Interpolated { .. } => t.interpolated += 1,
            io::Interp::Weighted { .. } => t.weighted_steps += 1,
            io::Interp::Vertex { .. } => t.vertex_values += 1,
        }
    }
    for e in &r.exports {
        let kind = match e.target {
            io::Target::Color(_) => "color",
            io::Target::Depth => "depth",
            io::Target::Null => "null",
            io::Target::Position(_) => "position",
            io::Target::Param { .. } => "param",
            io::Target::Unread => "unread param",
            io::Target::Primitive => "primitive",
            io::Target::PrimitiveParam { .. } => "primitive param",
        };
        *t.exports.entry(kind).or_default() += 1;
        t.compressed += usize::from(e.compressed);
    }
}

fn tally_resources(p: &NativeProgram, r: &resource::Resources, t: &mut Tally) {
    t.tracked += 1;
    t.scalar += r.scalar.len();
    t.dynamic += r.scalar.iter().filter(|s| s.1.dynamic.is_some()).count();
    let address = |load| r.scalar.iter().find(|s| s.0 == load).map(|s| s.1);
    let mut traced = HashSet::new();
    for leaf in r.descriptors.iter().flat_map(|d| d.source.leaves()) {
        let a = match *leaf {
            resource::Source::Loaded { load, .. } | resource::Source::Fetched { load, .. } => {
                t.loaded += 1;
                address(load)
            }
            resource::Source::Built { address, .. } => {
                t.built += 1;
                t.dynamic += usize::from(address.dynamic.is_some());
                Some(address)
            }
            resource::Source::Constant(_) => {
                t.constant += 1;
                None
            }
            resource::Source::Phi { .. }
            | resource::Source::Select { .. }
            | resource::Source::Lanes { .. }
            | resource::Source::LanePhi { .. } => {
                unreachable!("leaves are no phis, selects or per-lane choices")
            }
        };
        // Heaps hold dynamic resources, which no used entry lists; code
        // tables are no root parameter.
        if let Some(a) = a
            && a.param != resource::CODE
            && !matches!(
                p.layout[a.param].root,
                RootType::ResourceHeap | RootType::SamplerHeap
            )
        {
            traced.insert(p.layout[a.param].user_data);
        }
    }
    // Root constants are listed as a CBV, read straight from the SGPRs.
    traced.extend(
        r.params
            .iter()
            .map(|&(_, param, _)| &p.layout[param])
            .filter(|l| l.root == RootType::Constants)
            .map(|l| l.user_data),
    );
    let listed: HashSet<u8> = p.used.iter().map(|u| u.user_data).collect();
    t.used += p.used.len();
    // Immediate samplers are literals in the code: the distinct sampler
    // values (decoded) are matched against the listed entries, which do not
    // say which value is whose.
    let mut values: Vec<emit::StaticSampler> = Vec::new();
    for d in r
        .descriptors
        .iter()
        .filter(|d| d.role == resource::Role::Sampler)
    {
        for leaf in d.source.leaves() {
            if let resource::Source::Constant(words) = leaf
                && let Ok(w) = <[u32; 4]>::try_from(&words[..words.len().min(4)])
            {
                let s = emit::StaticSampler::from_words(w);
                if !values.contains(&s) {
                    values.push(s);
                }
            }
        }
    }
    // Engines often declare equal samplers at several registers, so equal
    // values may stand for several entries, but never more values than
    // entries.
    let immediate = p
        .used
        .iter()
        .filter(|u| u.root == RootType::ImmediateSampler)
        .count();
    let samplers_match = !values.is_empty() && values.len() <= immediate;
    let matched = |u: &&victory_shader::UsedResource| {
        if u.root == RootType::ImmediateSampler {
            samplers_match
        } else {
            traced.contains(&u.user_data)
        }
    };
    for u in p.used.iter().filter(|u| !matched(u)) {
        *t.untraced_used
            .entry(format!("{:?} via {:?}", u.kind, u.root))
            .or_default() += 1;
    }
    t.unlisted += traced.difference(&listed).count();
}

fn tally_structured(program: &ir::Program, s: &structure::Structured, t: &mut Tally) {
    t.structured += 1;
    for (block, guards) in program.blocks.iter().zip(&s.guards) {
        t.vector += block
            .insts
            .iter()
            .filter(|i| i.read(ir::Reg::S(ir::EXEC)).is_some())
            .count();
        for &g in guards.iter().flatten() {
            t.guarded += 1;
            let support = s.bools.support(g);
            t.opaque += usize::from(support.iter().any(|a| matches!(a, Atom::Opaque(_))));
        }
    }
    for exit in &s.exits {
        if let LaneExit::Branch { when, .. } = *exit {
            if s.bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(_)))
            {
                t.uniform_branches += 1;
            } else {
                t.lane_branches += 1;
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(windows))]
use offline::{Host, Validator, host};

/// Off Windows there is no D3D12 host or DXIL validator: asking for either
/// fails, and the translation itself still runs.
#[cfg(not(windows))]
mod offline {
    use victory_shader::emit::HostShader;

    const NEEDS_WINDOWS: &str = "needs Windows (D3D12 and dxil.dll)";

    pub enum Validator {}

    impl Validator {
        pub fn default_path() -> Option<std::path::PathBuf> {
            None
        }

        pub fn load(_: Option<&std::path::Path>) -> Result<Self, String> {
            Err(NEEDS_WINDOWS.into())
        }

        pub fn validate(&mut self, _: &mut [u8]) -> Result<(), String> {
            match *self {}
        }
    }

    pub enum Host {}

    impl Host {
        pub fn new(_: bool) -> Result<Self, String> {
            Err(NEEDS_WINDOWS.into())
        }

        pub fn pipeline(&self, _: &HostShader, _: &HostShader) -> Result<(), String> {
            match *self {}
        }

        pub fn mesh_pipeline(&self, _: &HostShader, _: &HostShader) -> Result<(), String> {
            match *self {}
        }

        pub fn compute_pipeline(&self, _: &HostShader) -> Result<(), String> {
            match *self {}
        }
    }

    pub mod host {
        use victory_shader::emit::HostShader;

        /// Only asked for alongside a [`super::Host`], which never exists here.
        pub fn stub_ps() -> HostShader {
            unreachable!("no host GPU off Windows")
        }

        pub fn stub_vs(_: &HostShader) -> Result<HostShader, String> {
            Err(super::NEEDS_WINDOWS.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::native::{ComputeIo, StageIo};

    fn empty_dxbc() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(b"DXBC");
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&32u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    fn endpgm_program(stage: HwStage, io: StageIo) -> NativeProgram {
        // GFX10 `s_endpgm` (0xBF81_0000).
        NativeProgram {
            stage,
            wave64: false,
            vgprs: 4,
            layout: Vec::new(),
            used: Vec::new(),
            io,
            code: 0xBF81_0000u32.to_le_bytes().to_vec(),
        }
    }

    #[test]
    fn reason_lines_report_per_stage_counts_and_merge_across_workers() {
        let mut a = Tally::default();
        a.fail(Some(HwStage::Vs), "emit: unknown stride".into(), || {
            "a.bin Vs".into()
        });
        a.fail(Some(HwStage::Vs), "emit: unknown stride".into(), || {
            "a2.bin Vs".into()
        });
        a.fail(Some(HwStage::Ps), "emit: unknown stride".into(), || {
            "a3.bin Ps".into()
        });
        a.fail(Some(HwStage::Cs), "emit: unknown stride".into(), || {
            "a4.bin Cs".into()
        });
        a.fail(
            Some(HwStage::Ps),
            "I/O: computed barycentrics".into(),
            || "b.bin Ps".into(),
        );
        a.fail(None, "corrupt record".into(), || "bad.bin".into());

        let mut b = Tally::default();
        b.fail(Some(HwStage::Ps), "emit: unknown stride".into(), || {
            "c.bin Ps".into()
        });
        b.fail(Some(HwStage::Ms), "emit: unknown stride".into(), || {
            "d.bin Ms".into()
        });
        b.fail(Some(HwStage::Gs), "structure: irreducible".into(), || {
            "g.bin Gs".into()
        });

        a.merge(b);
        assert_eq!(
            a.failure_lines(),
            vec![
                "        6  emit: unknown stride  [vs 2, ps 2, cs 1, ms 1]  (e.g. a.bin Vs)",
                "        1  I/O: computed barycentrics  [vs 0, ps 1, cs 0, ms 0]  (e.g. b.bin Ps)",
                "        1  corrupt record  (e.g. bad.bin)",
                "        1  structure: irreducible  [vs 0, ps 0, cs 0, ms 0, gs 1]  (e.g. g.bin Gs)",
            ]
        );
    }

    #[test]
    fn list_mode_records_rows_and_preserves_stage_breakdown() {
        let mut t = Tally {
            list: true,
            ..Tally::default()
        };
        t.fail(Some(HwStage::Vs), "emit: test".into(), || {
            "x.bin hash=0000 Vs".into()
        });
        t.fail(Some(HwStage::Ms), "emit: test".into(), || {
            "y.bin hash=1111 Ms".into()
        });
        assert_eq!(
            t.lines,
            vec![
                "x.bin hash=0000 Vs  emit: test",
                "y.bin hash=1111 Ms  emit: test",
            ]
        );
        assert_eq!(
            t.failure_lines(),
            vec!["        2  emit: test  [vs 1, ps 0, cs 0, ms 1]  (e.g. x.bin hash=0000 Vs)"]
        );
    }

    #[test]
    fn summary_program_counts_add_up_without_subtraction() {
        let mut a = Tally {
            programs: 10,
            emit_programs: 5,
            emitted: 4,
            validated: 3,
            validator_rejected: 1,
            cs_programs: 2,
            cs_emitted: 1,
            cs_validated: 1,
            refused: 3,
            ..Tally::default()
        };
        a.not_emitted.record(HwStage::Ls);
        a.not_emitted.record(HwStage::Hs);

        let mut b = Tally {
            programs: 7,
            ms_programs: 3,
            ms_emitted: 2,
            ms_validated: 1,
            validator_rejected: 1,
            refused: 1,
            ..Tally::default()
        };
        b.not_emitted.record(HwStage::Hs);
        b.not_emitted.record(HwStage::Es);
        b.not_emitted.record(HwStage::Gs);
        b.not_emitted.record(HwStage::Gs);

        a.merge(b);
        assert_eq!(
            a.emitted_total() + a.refused + a.not_emitted.total(),
            a.programs
        );

        let without_validate = a.summary_lines(false);
        assert!(
            without_validate.contains(
                &"             6 programs in stages not emitted (LS 1, HS 2, ES 1, GS 2)"
                    .to_owned()
            ),
            "{without_validate:#?}"
        );
        // Because `validator_rejected > 0`, the breakdown stays visible even if `validate` is false.
        assert!(
            without_validate.contains(
                &"             7 emitted (5 validated, 2 rejected by validator), 4 refused, 6 in stages not emitted (17 total)".to_owned()
            ),
            "{without_validate:#?}"
        );

        let plain = Tally {
            programs: 3,
            cs_programs: 3,
            cs_emitted: 2,
            refused: 1,
            ..Tally::default()
        };
        let plain_lines = plain.summary_lines(false);
        assert!(
            plain_lines.contains(&"             0 programs in stages not emitted".to_owned()),
            "{plain_lines:#?}"
        );
        assert!(
            plain_lines.contains(
                &"             2 emitted, 1 refused, 0 in stages not emitted (3 total)".to_owned()
            ),
            "{plain_lines:#?}"
        );
    }

    #[test]
    fn tally_program_separates_emitted_refused_and_unemitted_stages() {
        let raw = empty_dxbc();
        let container = Container::parse(&raw).unwrap();
        let mut checks = Checks {
            keep: None,
            validator: None,
            host: None,
        };
        let mut t = Tally {
            list: true,
            ..Tally::default()
        };
        let example = || "synth.bin".to_owned();

        // Compute shader with `s_endpgm` emits cleanly.
        let cs = endpgm_program(
            HwStage::Cs,
            StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [1, 1, 1],
            }),
        );
        tally_program(&container, &cs, &example, &mut t, &mut checks);

        // Tessellation and geometry stages are counted in `not_emitted`, not refused.
        for stage in [HwStage::Ls, HwStage::Hs, HwStage::Es, HwStage::Gs] {
            let prog = endpgm_program(stage, StageIo::None);
            tally_program(&container, &prog, &example, &mut t, &mut checks);
        }

        // An unemitted stage that fails IR building is counted once as refused, not `not_emitted`.
        let mut bad_gs = endpgm_program(HwStage::Gs, StageIo::None);
        bad_gs.code = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        tally_program(&container, &bad_gs, &example, &mut t, &mut checks);

        // A vertex shader that fails IR building records `vs 1` on its refusal line.
        let mut bad_vs = endpgm_program(
            HwStage::Vs,
            StageIo::Vs {
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
        );
        bad_vs.code = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        tally_program(&container, &bad_vs, &example, &mut t, &mut checks);

        assert_eq!(t.programs, 7);
        assert_eq!(t.emitted_total(), 1);
        assert_eq!(t.cs_emitted, 1);
        assert_eq!(t.not_emitted.total(), 4);
        assert_eq!(t.not_emitted.ls, 1);
        assert_eq!(t.not_emitted.hs, 1);
        assert_eq!(t.not_emitted.es, 1);
        assert_eq!(t.not_emitted.gs, 1);
        assert_eq!(t.refused, 2);
        assert_eq!(
            t.emitted_total() + t.refused + t.not_emitted.total(),
            t.programs
        );

        let reasons = t.failure_lines();
        assert_eq!(reasons.len(), 1, "{reasons:#?}");
        assert!(
            reasons[0].contains("[vs 1, ps 0, cs 0, ms 0, gs 1]"),
            "{}",
            reasons[0]
        );
        assert!(
            t.lines
                .iter()
                .any(|l| l == "synth.bin Gs  stage not emitted"),
            "{:#?}",
            t.lines
        );
    }
}
