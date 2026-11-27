//! Donor swap-chain lifecycle, two-plane output and successful-present telemetry.

use super::*;

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

pub(super) struct Presenter {
    last_present_issue: Option<windows::core::HRESULT>,
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    /// With presents sent ahead ([`late_free_present`]): a list for each
    /// present in the air, and the submission it last flew as (it is used
    /// again once that one has finished).
    ahead: Vec<(HostList, u64)>,
    hwnd: HWND,
    meter: Meter,
    composer: Option<crate::compose::Composer>,
    panel: panel::Panel,
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

    /// Counts a presented frame. `VICTORY_PRESENT_LOG=1` records each
    /// present at info level, including in release builds.
    fn frame(&mut self, hwnd: HWND) {
        static PRESENT_LOG: OnceLock<bool> = OnceLock::new();
        if *PRESENT_LOG.get_or_init(|| crate::switch::on("VICTORY_PRESENT_LOG")) {
            tracing::info!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        } else {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
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
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        let refused = BUDGET_NOTES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        let biggest = BUDGET_NOTES[1].swap(0, Ordering::Relaxed);
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame; {refused:.2} indirect records a frame ran empty for want of their list's budget (2^27), the largest cost of one {biggest} (0: under 2^22)"
        );
        crate::shadow::exact_line(frames);
        #[cfg(feature = "diagnostics")]
        crate::watch::alone_line(frames);
        crate::descriptor::sync_line(frames);
        late_line(frames);
        crate::queue::behind_line(frames);
        card_clock_line(frames, ms);
        #[cfg(feature = "diagnostics")]
        spans::line(frames);
        crate::shadow::back_line(frames);
        #[cfg(feature = "diagnostics")]
        split_line(frames);
        {
            let [
                passes,
                pieces,
                compares,
                original_compares,
                original_pieces,
                compare_pieces,
            ] = [0, 1, 2, 3, 4, 5]
                .map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (VICTORY_CARD_REHASH_RUNS=1 joins them: now {}), {compares:.0} compare passes \
                     over {compare_pieces:.0} pieces (before coalescing: {original_compares:.0} passes over {original_pieces:.0} pieces)",
                    if rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let [own, asking, still, all] = [0, 1, 2, 3].map(|i| {
                crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
            });
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let deferred =
                crate::texture::DEFERRED_REMAPS.swap(0, Ordering::Relaxed) as f64 / frames;
            if deferred > 0.0 {
                tracing::info!(
                    "{deferred:.1} texture refreshes per frame kept the previous snapshot while title memory was remapped"
                );
            }
        }
        {
            let [copies, joined] =
                [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (VICTORY_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
        }
        // Passes over the heap's buffer views for shaders that pick them by index.
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        // Copies between depth textures and their colour twins, each way.
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        // Depth planes drawn from their twins.
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        // Copies in the card's memory (`crate::shadow::cards`).
        let card = crate::shadow::card_stats();
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::slow_asks();
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            crate::tiles::status();
            {
                let [queued, due, away, partly, oldest] = [0, 1, 2, 3, 4]
                    .map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
            }
            if crate::resource::held_only() {
                let [copies, bytes, grown, whole] =
                    [0, 1, 2, 3].map(|i| crate::resource::HELD[i].load(Ordering::Relaxed));
                let [flipped, missed] =
                    [0, 1].map(|i| crate::descriptor::host::FLIPPED[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures from the first level viewed (VICTORY_HELD_LEVELS=0: whole), since the start: {copies} host copies of the \
                     levels viewed alone ({} MB on the card, {} MB of title memory whole), {grown} replaced by one with more levels, \
                     {flipped} slots made from their new view after its upload had run ({missed} written again first); {} uploads left \
                     out as the host copy held those bytes (VICTORY_TEXTURE_SAME=0: made); {} MB of texture blocks free; \
                     {} looks for textures to let go, {:.2} ms each, the longest {:.1}",
                    bytes >> 20,
                    whole >> 20,
                    crate::draw::HEAP_TEXTURES_SAME.load(Ordering::Relaxed),
                    crate::pool::texture_room() >> 20,
                    crate::resource::SWEPT_US[0].load(Ordering::Relaxed),
                    crate::resource::SWEPT_US[1].load(Ordering::Relaxed) as f64
                        / 1e3
                        / crate::resource::SWEPT_US[0].load(Ordering::Relaxed).max(1) as f64,
                    crate::resource::SWEPT_US[2].load(Ordering::Relaxed) as f64 / 1e3
                );
            }
            {
                let mode = crate::watch::page_writes();
                let [writes, bytes, guarded, plain] =
                    [0, 1, 2, 3].map(|i| crate::watch::WRITTEN[i].load(Ordering::Relaxed));
                let [kept, dropped, slots, uploaded] = [0, 1, 2, 3]
                    .map(|i| crate::descriptor::host::PAGE_WRITES[i].load(Ordering::Relaxed));
                tracing::info!(
                    "writes behind the guards, since the start: {writes} DirectStorage writes into physical pages at addresses the                      title had mapped ({} MB); {guarded} guarded 64 KiB blocks of host copies lay under them ({}); {plain} reads to                      plain addresses finished ({}); {kept} ranges under texture views kept for the texture pass ({dropped} dropped);                      {slots} slots of placed textures held their bytes, {uploaded} of them uploaded before the write ({})                      (VICTORY_PAGE_WRITES: unset or guards opens the guarded blocks, 0 counts only, 1, slots, all)",
                    bytes >> 20,
                    if mode & 1 != 0 {
                        "opened, taken again"
                    } else {
                        "left as they were"
                    },
                    if mode & 4 != 0 {
                        "told as well"
                    } else {
                        "counted only"
                    },
                    if mode & 2 != 0 {
                        "made due again"
                    } else {
                        "left as they were"
                    }
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms)",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{} ({} copies made of views off their buffer's elements); depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::shadow::OWN_MADE.load(Ordering::Relaxed),
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            let (defrag_dispatches, defrag_groups, defrag_indirect) = crate::draw::defrag_counts();
            if defrag_dispatches > 0 || defrag_indirect > 0 {
                tracing::info!(
                    dispatches = defrag_dispatches,
                    groups = defrag_groups,
                    indirect = defrag_indirect,
                    sites = victory_shader::emit::defrag_meet_sites(),
                    lds_reads = victory_shader::emit::defrag_lds_sites().0,
                    lds_writes = victory_shader::emit::defrag_lds_sites().1,
                    "heap defragmentation since start"
                );
            }
            #[cfg(feature = "diagnostics")]
            {
                let put = [0, 1, 2, 3]
                    .map(|i| crate::shadow::PUT[i].swap(0, Ordering::Relaxed) as f64 / frames);
                tracing::info!(
                    "what shaders wrote put into title memory per frame (VICTORY_EXACT_BACK): {:.0} pieces in {:.0} writes, {:.1} MB; {:.0} pieces joined over gaps",
                    put[0],
                    put[1],
                    put[2] / 1e6,
                    put[3]
                );
                traffic::line(self.copy_stats_frames as f64);
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
    /// shown; else, in diagnostics builds, whether the frame was black.
    #[allow(dead_code)]
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        self.present_planes(texture, state, None, frame)
    }

    /// Shows `texture`, optionally alpha-compositing a second `overlay` plane
    /// over it (Series S two-plane presentation), and returns them to their states.
    pub(crate) fn present_planes(
        &self,
        texture: &ID3D12Resource,
        state: u32,
        overlay: Option<(&ID3D12Resource, u32)>,
        frame: u64,
    ) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let planes = overlay.and_then(|(over, over_state)| {
            // SAFETY: a live overlay resource.
            let over_desc = unsafe { over.GetDesc() };
            Some((
                over,
                D3D12_RESOURCE_STATES(over_state as i32),
                crate::compose::view(&desc)?,
                crate::compose::view(&over_desc)?,
            ))
        });
        let format = if planes.is_some() {
            DXGI_FORMAT_R8G8B8A8_UNORM
        } else {
            let Some(format) = swap_format(desc.Format) else {
                tracing::warn!(format = desc.Format.0, "cannot present this format");
                return None;
            };
            format
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let state_u32 = state;
        let p = presenter.as_mut()?;
        if planes.is_some() && p.composer.is_none() {
            match crate::compose::Composer::new(self, format) {
                Ok(c) => p.composer = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "two-plane composer not made; showing base plane only");
                    drop(presenter);
                    return self.present_planes(texture, state_u32, None, frame);
                }
            }
        }
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    if p.format != format {
                        p.composer = None;
                    }
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        if planes.is_some() && p.composer.is_none() {
            match crate::compose::Composer::new(self, format) {
                Ok(c) => p.composer = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "two-plane composer not made after resize; showing base plane only");
                    drop(presenter);
                    return self.present_planes(texture, state_u32, None, frame);
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state_u32 as i32);
        // SAFETY: our swap chain, list and live source texture(s).
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            p.panel.poll(p.hwnd);
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let capture_now = crate::capture::wanted(frame);
            #[cfg(feature = "diagnostics")]
            let read_now =
                capture_now || crate::capture::listed(frame) || crate::capture::show_active();
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead
                    .iter()
                    .position(|(_, flown)| *flown <= finished)
                    .or_else(|| {
                        let list = self.new_list().ok()?;
                        list.close();
                        p.ahead.push((list, 0));
                        Some(p.ahead.len() - 1)
                    })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            copy.keep(texture);
            let list = &copy.list;
            let composed = if let Some((over, over_state, base_fmt, over_fmt)) = planes {
                copy.keep(over);
                if let Some(composer) = p.composer.as_mut() {
                    let mut pre = Vec::with_capacity(3);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        pre.push(transition(
                            texture,
                            state,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        ));
                    }
                    if over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        pre.push(transition(
                            over,
                            over_state,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        ));
                    }
                    pre.push(transition(
                        &back,
                        D3D12_RESOURCE_STATE_PRESENT,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                    ));
                    list.ResourceBarrier(&pre);
                    let compose_slot = slot.map_or(0, |i| i + 1);
                    if let Err(err) = composer.record(
                        self,
                        compose_slot,
                        list,
                        [(texture, base_fmt), (over, over_fmt)],
                        &back,
                        (width, height),
                    ) {
                        copy.close();
                        drop(back);
                        drop(presenter);
                        tracing::warn!(%err, "could not record display composite; presenting the base plane");
                        return self.present_planes(texture, state_u32, None, frame);
                    }
                    static ONCE: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !ONCE.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            frame,
                            width,
                            height,
                            base_format = base_fmt.0,
                            overlay_format = over_fmt.0,
                            "display output: compositing two planes (overlay active)"
                        );
                    }
                    let mut post = Vec::with_capacity(3);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        post.push(transition(
                            texture,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                            state,
                        ));
                    }
                    if over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        post.push(transition(
                            over,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                            over_state,
                        ));
                    }
                    post.push(transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    ));
                    list.ResourceBarrier(&post);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !composed {
                let mut pre = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    pre.push(transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
                }
                pre.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ));
                list.ResourceBarrier(&pre);
                #[cfg(feature = "diagnostics")]
                let shown = crate::capture::show(list, &back, texture);
                #[cfg(not(feature = "diagnostics"))]
                let shown = false;
                if !shown {
                    list.CopyResource(&back, texture);
                }
            }
            #[cfg(feature = "diagnostics")]
            let read_output = || {
                list.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let res = crate::capture::read_back(&self.device, list, &back);
                list.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                res
            };
            #[cfg(feature = "diagnostics")]
            let capture_panel = capture_now && crate::capture::with_panel();
            #[cfg(feature = "diagnostics")]
            let capture = if capture_now && !capture_panel {
                if composed {
                    read_output()
                } else {
                    crate::capture::read_back(&self.device, list, texture)
                }
            } else {
                None
            };
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() || composed {
                None
            } else {
                crate::capture::probe(&self.device, list, texture)
            };
            #[cfg(feature = "diagnostics")]
            let watched = if composed {
                None
            } else {
                crate::popwatch::take(&self.device, list, texture)
            };
            let uploaded = p
                .panel
                .draw(self, copy, &back, (p.width, p.height), p.format);
            #[cfg(feature = "diagnostics")]
            let capture = if capture_panel {
                read_output()
            } else {
                capture
            };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            if composed {
                list.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                )]);
            } else {
                let mut post = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    post.push(transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
                }
                post.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ));
                list.ResourceBarrier(&post);
            }
            copy.close();
            let flown = match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                    flown
                }
                None => {
                    self.execute(&[copy]);
                    0
                }
            };
            if uploaded {
                p.panel.sent(flown);
            }
            #[cfg(feature = "diagnostics")]
            if let Some(taken) = watched {
                crate::popwatch::sent(taken, flown, frame);
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let mut showing = crate::timeline::span("the host's Present");
            let result = p.swap_chain.Present(0, DXGI_PRESENT(0));
            if result == windows::Win32::Foundation::DXGI_STATUS_OCCLUDED || result.is_err() {
                p.meter.frame_times.interrupted();
                if p.last_present_issue.replace(result) != Some(result) {
                    if result == windows::Win32::Foundation::DXGI_STATUS_OCCLUDED {
                        tracing::info!(frame, "DXGI reports the window is occluded");
                    } else {
                        tracing::warn!(frame, error = %windows::core::Error::from_hresult(result), "swap chain present failed");
                    }
                }
                return None;
            }
            if p.last_present_issue.take().is_some() {
                tracing::info!(frame, "DXGI resumed presenting frames");
            }
            p.panel.presented(true);
            showing.next("the frame meter");
            p.meter.frame(p.hwnd);
            showing.end();
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
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
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            // Leave SDR output unchanged; tag an HDR/scRGB swap chain only
            // if the actual output currently advertises the corresponding mode.
            if matches!(
                format,
                DXGI_FORMAT_R10G10B10A2_UNORM | DXGI_FORMAT_R16G16B16A16_FLOAT
            ) {
                let hdr = swap_chain
                    .GetContainingOutput()
                    .and_then(|output| output.cast::<IDXGIOutput6>())
                    .and_then(|output| output.GetDesc1())
                    .is_ok_and(|desc| {
                        desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                    });
                let space = desired_color_space(format, hdr);
                if swap_chain.CheckColorSpaceSupport(space).unwrap_or(0)
                    & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32
                    != 0
                    && let Err(error) = swap_chain.SetColorSpace1(space)
                {
                    tracing::warn!(%error, color_space = space.0, "SetColorSpace1 failed");
                }
            }
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                last_present_issue: None,
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
                composer: None,
                panel: panel::Panel::new(crate::switch::on("VICTORY_PANEL")),
            })
        }
    }
}
