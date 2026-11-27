//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);
static INTERVAL_OVERRIDE: OnceLock<Duration> = OnceLock::new();

/// Overrides the title's frame interval for this launch. The launcher must
/// validate the title profile and call this before starting title threads.
/// A zero interval makes every wait immediate.
pub fn set_frame_interval_override(interval: Duration) {
    let _ = INTERVAL_OVERRIDE.set(interval);
}

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let title_interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    let interval = INTERVAL_OVERRIDE.get().copied().unwrap_or(title_interval);
    tracing::info!(?interval, ?title_interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Advances time in GPU subsystem tests that do not run a title loop.
#[cfg(all(windows, test))]
pub(crate) fn test_advance() {
    FRAME.fetch_add(1, Ordering::Relaxed);
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` took
/// (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it. A wait
/// that comes within `limit` takes the tick here, under the clock's lock,
/// so two threads waiting for one kind never take the same tick.
fn next_tick(kind: u32, limit: Duration) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: INTERVAL_OVERRIDE.get().copied().unwrap_or(SIXTIETH),
        first: 0,
    });
    if c.interval.is_zero() {
        return (
            last(kind).fetch_add(1, Ordering::Relaxed) + 1,
            Duration::ZERO,
        );
    }
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let last_tick = last(kind).load(Ordering::Relaxed);
    // The tick most recently passed. A flip (or vblank) wait that begins
    // after a tick nobody has taken yet is late: the frame took longer than
    // an interval. It ends at once on that tick instead of waiting for the
    // following one, so a frame a little over budget costs that much more,
    // not a whole extra interval (30 fps falling straight to 15).
    let passed = c.first + (elapsed / interval) as u64;
    if kind != ORIGIN && passed > last_tick && passed > c.first {
        // Take it here like any other tick, so the next wait cannot end on it.
        last(kind).fetch_max(passed, Ordering::Relaxed);
        return (passed, Duration::ZERO);
    }
    let tick = (c.first + (elapsed / interval + 1) as u64).max(last_tick + 1);
    let at = u128::from(tick - c.first) * interval;
    let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
    if wait <= limit {
        last(kind).fetch_max(tick, Ordering::Relaxed);
    }
    (tick, wait)
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let limit = Duration::from_millis(u64::from(timeout_ms));
    let (_tick, wait) = next_tick(kind, limit);
    let _called = crate::timeline::span("the title's frame wait");
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        S_OK
    };
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick = _tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        // Loose on purpose: a 2 ms tick can take over 50 ms on a loaded
        // scheduler. 500 ms still fails well before the 1 s time-out.
        assert!(before.elapsed() <= Duration::from_millis(500));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip ends on a tick of its own.
        const FLIP: u32 = 2;
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let mut token = 0;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                LAST[2].load(Ordering::Relaxed)
            })
            .collect();
        // Strictly rising: a late wake may skip ticks, but never repeats one.
        assert!(flips.windows(2).all(|w| w[1] > w[0]), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP, Duration::ZERO);
        assert!(tick > flips[3]);

        // A frame over budget: its flip wait begins after a tick nobody
        // took and ends at once on it, not on the tick after.
        assert_eq!(set_interval(null, null, 20_000, 1, 0), S_OK);
        assert_eq!(
            wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
            S_OK
        );
        let flipped = LAST[2].load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(25));
        let late = Instant::now();
        assert_eq!(
            wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
            S_OK
        );
        assert!(late.elapsed() < Duration::from_millis(10));
        assert_eq!(LAST[2].load(Ordering::Relaxed), flipped + 1);
    }

    /// Two waits for one kind that begin together (two threads) take two
    /// ticks: the first takes its tick before it sleeps. A time-out takes
    /// none. The old code marked the tick after the sleep, so both got it.
    #[test]
    fn waits_begun_together_take_different_ticks() {
        const VBLANK: u32 = 1;
        let long = Duration::from_secs(1);
        let (first, _) = next_tick(VBLANK, long);
        let (second, _) = next_tick(VBLANK, long);
        assert!(second > first, "{second} after {first}");
        next_tick(VBLANK, Duration::ZERO);
        assert_eq!(last(VBLANK).load(Ordering::Relaxed), second);
    }

    #[test]
    fn a_validated_zero_interval_gives_distinct_immediate_ticks() {
        // The launcher sets the interval once per title process.
        const CHILD: &str = "VICTORY_TEST_ZERO_INTERVAL";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "frame::tests::a_validated_zero_interval_gives_distinct_immediate_ticks",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        set_frame_interval_override(Duration::ZERO);
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 16_667, 1, 0), S_OK);
        let mut token = 0;
        for expected in 1..=3 {
            assert_eq!(wait(null, ORIGIN, 0, null.cast(), 0, &mut token), S_OK);
            assert_eq!(token, expected);
            assert_eq!(last(ORIGIN).load(Ordering::Relaxed), expected);
        }
    }
}
