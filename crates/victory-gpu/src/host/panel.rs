//! An optional overlay on the swapchain. F3 toggles it while the title
//! window has focus; `VICTORY_PANEL=1` starts it visible. The card reuses
//! the last painted box between updates. Only successful presents are timed.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F3};
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::{FINISHED, Host, HostList, transition};

const EVERY: Duration = Duration::from_secs(1);
const SAMPLES: usize = 128;

/// A character's dots, and with the space beside and below it.
const GLYPH: (u32, u32) = (5, 7);
const CELL: (u32, u32) = (6, 9);
/// Characters in a line, lines, and the box's rim in dots.
const COLUMNS: u32 = 36;
const LINES: u32 = 6;
const RIM: u32 = 3;

/// Seven rows of five dots, the leftmost the highest bit.
#[rustfmt::skip]
const fn glyph(c: u8) -> [u8; 7] {
    match c {
        b'0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        b'1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        b'3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        b'4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        b'5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        b'6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        b'7' => [0b11111, 0b00001, 0b00010, 0b00010, 0b00100, 0b00100, 0b00100],
        b'8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        b'9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        b'A' => [0b00100, 0b01010, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001],
        b'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        b'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        b'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        b'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        b'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        b'G' => [0b01111, 0b10000, 0b10000, 0b10011, 0b10001, 0b10001, 0b01111],
        b'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        b'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        b'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        b'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        b'N' => [0b10001, 0b11001, 0b11001, 0b10101, 0b10011, 0b10011, 0b10001],
        b'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        b'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        b'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        b'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        b'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        b'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        b'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        b'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        b'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        b'.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        b'/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        b'-' => [0, 0, 0, 0b11111, 0, 0, 0],
        b' ' => [0; 7],
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
    }
}

/// The box's size in pixels, a dot being `scale` of them each way.
const fn size(scale: u32) -> (u32, u32) {
    (
        (COLUMNS * CELL.0 + 2 * RIM) * scale,
        (LINES * CELL.1 + 2 * RIM) * scale,
    )
}

/// Pixels to a dot for a picture that high: a 720-line screenshot of it
/// shows one and a third.
fn scale(height: u32) -> u32 {
    (height / 540).max(1)
}

/// Whether a dot of the picture of `lines` is white.
fn lit(lines: &[impl AsRef<[u8]>], x: u32, y: u32) -> bool {
    let (Some(x), Some(y)) = (x.checked_sub(RIM), y.checked_sub(RIM)) else {
        return false;
    };
    if x % CELL.0 >= GLYPH.0 || y % CELL.1 >= GLYPH.1 || x / CELL.0 >= COLUMNS {
        return false;
    }
    let Some(&c) = lines
        .get((y / CELL.1) as usize)
        .and_then(|l| l.as_ref().get((x / CELL.0) as usize))
    else {
        return false;
    };
    (glyph(c.to_ascii_uppercase())[(y % CELL.1) as usize] >> (GLYPH.0 - 1 - x % CELL.0)) & 1 != 0
}

/// The bytes of a black and a white pixel of a swap chain's format.
fn colours(format: DXGI_FORMAT) -> Option<[&'static [u8]; 2]> {
    Some(match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM => [&[0, 0, 0, 0xFF], &[0xFF; 4]],
        DXGI_FORMAT_R10G10B10A2_UNORM => [&[0, 0, 0, 0xC0], &[0xFF; 4]],
        // Halves of 0 and 1.
        DXGI_FORMAT_R16G16B16A16_FLOAT => [
            &[0, 0, 0, 0, 0, 0, 0, 0x3C],
            &[0, 0x3C, 0, 0x3C, 0, 0x3C, 0, 0x3C],
        ],
        _ => return None,
    })
}

/// Paint one row at a time in cached memory; the mapped upload is write-only.
fn paint(
    lines: &[impl AsRef<[u8]>],
    scale: u32,
    colours: [&[u8]; 2],
    row: &mut [u8],
    mut write_row: impl FnMut(u32, &[u8]),
) {
    let bytes = colours[0].len();
    for y in 0..size(scale).1 / scale {
        for (x, pixels) in row.chunks_exact_mut(bytes * scale as usize).enumerate() {
            let colour = colours[usize::from(lit(lines, x as u32, y))];
            for pixel in pixels.chunks_exact_mut(bytes) {
                pixel.copy_from_slice(colour);
            }
        }
        for dy in 0..scale {
            write_row(y * scale + dy, row);
        }
    }
}

struct Timing {
    avg_ms: f64,
    p95: Duration,
    max: Duration,
}

struct Pacing {
    last: Option<Instant>,
    intervals: [Duration; SAMPLES],
    len: usize,
    elapsed: Duration,
}

impl Pacing {
    fn new() -> Self {
        Self {
            last: None,
            intervals: [Duration::ZERO; SAMPLES],
            len: 0,
            elapsed: Duration::ZERO,
        }
    }

    fn clear(&mut self) {
        self.last = None;
        self.len = 0;
        self.elapsed = Duration::ZERO;
    }

    fn record(&mut self, now: Instant) -> Option<Timing> {
        let interval = now - self.last.replace(now)?;
        self.intervals[self.len] = interval;
        self.len += 1;
        self.elapsed += interval;
        if self.elapsed < EVERY && self.len < SAMPLES {
            return None;
        }
        let len = std::mem::take(&mut self.len);
        let elapsed = std::mem::take(&mut self.elapsed);
        if elapsed.is_zero() {
            return None;
        }
        let intervals = &mut self.intervals[..len];
        let (upper, p95) = {
            let (_, p95, upper) = intervals.select_nth_unstable((len * 95).div_ceil(100) - 1);
            (upper, *p95)
        };
        let max = upper.iter().copied().max().unwrap_or(p95);
        Some(Timing {
            avg_ms: elapsed.as_secs_f64() * 1e3 / len as f64,
            p95,
            max,
        })
    }
}

type Text = [[u8; COLUMNS as usize]; LINES as usize];

fn text(timing: Option<&Timing>, (width, height): (u32, u32), host: &Host) -> Text {
    let mut lines = [[b' '; COLUMNS as usize]; LINES as usize];
    let _ = write!(&mut lines[0][..], "VICTORY / D3D12          F3 HIDE");
    if let Some(t) = timing {
        let _ = write!(
            &mut lines[1][..],
            "{:.1} FPS   {:.1} MS AVG",
            1e3 / t.avg_ms,
            t.avg_ms
        );
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let _ = write!(
            &mut lines[2][..],
            "P95 {:.1} MS   MAX {:.1} MS",
            ms(t.p95),
            ms(t.max)
        );
    } else {
        let _ = write!(&mut lines[1][..], "MEASURING PRESENTS");
    }
    let _ = write!(&mut lines[3][..], "OUTPUT {width} X {height}");
    if let Some([budget, used, _, _]) = host.video_memory() {
        let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        let _ = write!(
            &mut lines[4][..],
            "VRAM {:.1} / {:.1} GIB",
            gib(used),
            gib(budget)
        );
    } else {
        let _ = write!(&mut lines[4][..], "VRAM UNAVAILABLE");
    }
    if victory_shader::emit::rays() {
        let _ = write!(
            &mut lines[5][..],
            "RAYS OPT-IN / BVH {}",
            crate::pipeline::builder_status()
        );
    } else {
        let _ = write!(&mut lines[5][..], "RAYS OFF");
    }
    lines
}

/// The buffers of a box that size and format.
struct Buffers {
    scale: u32,
    colours: [&'static [u8]; 2],
    pitch: u32,
    row: Vec<u8>,
    upload: ID3D12Resource,
    mapped: *mut u8,
    upload_fence: u64,
    card: ID3D12Resource,
}

// SAFETY: free-threaded resources, and memory only written under the presenter's lock.
unsafe impl Send for Buffers {}

impl Buffers {
    fn new(host: &Host, format: DXGI_FORMAT, scale: u32) -> Option<Buffers> {
        let (width, height) = size(scale);
        let colours = colours(format)?;
        let row_bytes = width * colours[0].len() as u32;
        let pitch = row_bytes.next_multiple_of(256);
        let bytes = u64::from(pitch) * u64::from(height);
        let (upload, mapped) = host.upload_buffer(bytes)?;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
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
        let mut card: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut card,
                )
                .ok()?;
        }
        Some(Buffers {
            scale,
            colours,
            pitch,
            row: vec![0; row_bytes as usize],
            upload,
            mapped,
            upload_fence: 0,
            card: card?,
        })
    }

    fn write(&mut self, lines: &Text) {
        let mapped = self.mapped;
        let pitch = self.pitch as usize;
        paint(lines, self.scale, self.colours, &mut self.row, |y, row| {
            // SAFETY: the upload is mapped, and its last GPU read completed.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    row.as_ptr(),
                    mapped.add(y as usize * pitch),
                    row.len(),
                )
            };
        });
    }
}

/// The panel of a presenter.
pub(super) struct Panel {
    visible: bool,
    key_down: bool,
    pacing: Pacing,
    timing: Option<Timing>,
    dirty: bool,
    output: (u32, u32),
    layout: Option<(DXGI_FORMAT, u32)>,
    buffers: Option<Buffers>,
}

impl Panel {
    pub(super) fn new(visible: bool) -> Self {
        Self {
            visible,
            key_down: false,
            pacing: Pacing::new(),
            timing: None,
            dirty: true,
            output: (0, 0),
            layout: None,
            buffers: None,
        }
    }

    fn key(&mut self, down: bool, focused: impl FnOnce() -> bool) {
        let was_down = std::mem::replace(&mut self.key_down, down);
        if down && !was_down && focused() {
            self.visible = !self.visible;
            self.pacing.clear();
            self.timing = None;
            self.dirty = true;
            if !self.visible {
                self.layout = None;
                self.buffers = None;
            }
        }
    }

    pub(super) fn poll(&mut self, hwnd: HWND) {
        // SAFETY: a key-state query with a valid virtual key.
        let down = unsafe { GetAsyncKeyState(i32::from(VK_F3)) } < 0;
        self.key(down, || {
            // SAFETY: a query with no input pointers; the returned handle is only compared.
            unsafe { GetForegroundWindow() == hwnd.0 }
        });
    }

    pub(super) fn presented(&mut self, success: bool) {
        if !self.visible {
            return;
        }
        if !success {
            self.pacing.clear();
            self.dirty |= self.timing.take().is_some();
        } else if let Some(timing) = self.pacing.record(Instant::now()) {
            self.timing = Some(timing);
            self.dirty = true;
        }
    }

    /// Draws onto a swapchain buffer in COPY_DEST. Returns whether this
    /// submission reads the upload; the caller supplies its fence via `sent`.
    pub(super) fn draw(
        &mut self,
        host: &Host,
        copy: &HostList,
        back: &ID3D12Resource,
        output: (u32, u32),
        format: DXGI_FORMAT,
    ) -> bool {
        if !self.visible {
            return false;
        }
        let (width, height) = output;
        let scale = scale(height);
        if self.layout != Some((format, scale)) {
            self.layout = Some((format, scale));
            self.buffers = Buffers::new(host, format, scale);
            if self.buffers.is_none() {
                tracing::warn!(?format, scale, "could not allocate the diagnostics overlay");
            }
            self.dirty = true;
        }
        let Some(buffers) = &mut self.buffers else {
            return false;
        };
        self.dirty |= self.output != output;
        self.output = output;
        let refreshed = self.dirty && buffers.upload_fence <= FINISHED.load(Ordering::Acquire);
        copy.keep(&buffers.card);
        if refreshed {
            buffers.write(&text(self.timing.as_ref(), output, host));
            self.dirty = false;
            copy.keep(&buffers.upload);
        }
        let (w, h) = size(scale);
        let list = &copy.list;
        // SAFETY: an open list and live resources; the box lies within both.
        unsafe {
            if refreshed {
                // Buffers decay to COMMON after each submission. This copy
                // promotes the card buffer to COPY_DEST, then we read it below.
                list.CopyResource(&buffers.card, &buffers.upload);
                list.ResourceBarrier(&[transition(
                    &buffers.card,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
            }
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffers.card),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: format,
                            Width: w,
                            Height: h,
                            Depth: 1,
                            RowPitch: buffers.pitch,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let part = D3D12_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w.min(width),
                bottom: h.min(height),
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&part));
        }
        refreshed
    }

    pub(super) fn sent(&mut self, flown: u64) {
        if let Some(buffers) = &mut self.buffers {
            buffers.upload_fence = flown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character the lines use has its own picture, and no two look
    /// alike.
    #[test]
    fn characters_differ() {
        let used: Vec<u8> = (b'0'..=b'9').chain(b'A'..=b'Z').chain(*b"./- ").collect();
        for (i, a) in used.iter().enumerate() {
            assert_ne!(glyph(*a), glyph(b'#'), "{} has a picture", *a as char);
            assert!(glyph(*a).iter().all(|row| row >> GLYPH.0 == 0));
            for b in &used[i + 1..] {
                assert_ne!(glyph(*a), glyph(*b), "{} {}", *a as char, *b as char);
            }
        }
    }

    #[test]
    fn a_box_is_painted() {
        let lines = ["I", " i"];
        for scale in [1, 2, 4] {
            let (width, height) = size(scale);
            let pitch = (width * 4).next_multiple_of(256) as usize;
            let colours = colours(DXGI_FORMAT_R10G10B10A2_UNORM).unwrap();
            let mut picture = vec![0; pitch * height as usize];
            let mut row = vec![0; width as usize * 4];
            paint(&lines, scale, colours, &mut row, |y, row| {
                let start = y as usize * pitch;
                picture[start..start + row.len()].copy_from_slice(row);
            });
            let at = |x: u32, y: u32| &picture[y as usize * pitch + x as usize * 4..][..4];
            // The rim is black, the first letter's top row lit in its middle.
            assert_eq!(at(0, 0), colours[0]);
            assert_eq!(at((RIM + 2) * scale, RIM * scale), colours[1]);
            assert_eq!(
                at((RIM + 2) * scale + scale - 1, RIM * scale + scale - 1),
                colours[1]
            );
            assert_eq!(at(RIM * scale, RIM * scale), colours[0]);
            // Small letters are shown as capitals, a line and a cell on.
            assert_eq!(
                at((RIM + CELL.0 + 2) * scale, (RIM + CELL.1) * scale),
                colours[1]
            );
            // Nothing past a line's end.
            assert_eq!(
                at((RIM + CELL.0 * 2 + 2) * scale, (RIM + CELL.1) * scale),
                colours[0]
            );
        }
    }

    #[test]
    fn pacing_counts_intervals_and_separates_p95_from_spikes() {
        let mut pacing = Pacing::new();
        let mut now = Instant::now();
        assert!(pacing.record(now).is_none());
        for ms in [40; 19].into_iter().chain([70]) {
            now += Duration::from_millis(ms);
            assert!(pacing.record(now).is_none());
        }
        let t = pacing.record(now + Duration::from_millis(170)).unwrap();
        assert!((1e3 / t.avg_ms - 21.0).abs() < 1e-9);
        assert_eq!(
            (t.p95, t.max),
            (Duration::from_millis(70), Duration::from_millis(170))
        );

        pacing.clear();
        assert!(pacing.record(now + Duration::from_secs(20)).is_none());
        let t = pacing.record(now + Duration::from_secs(21)).unwrap();
        assert_eq!((t.avg_ms, t.max), (1e3, Duration::from_secs(1)));
    }

    #[test]
    fn f3_toggles_only_on_a_focused_press() {
        let mut panel = Panel::new(false);
        panel.key(true, || false);
        panel.key(true, || true);
        assert!(
            !panel.visible,
            "focusing while F3 is held must not turn it on"
        );
        panel.key(false, || true);
        panel.key(true, || true);
        panel.key(true, || true);
        assert!(panel.visible, "holding F3 must not toggle repeatedly");
        panel.key(false, || true);
        panel.key(true, || true);
        assert!(!panel.visible);
    }
}
