//! `RENO_BENCH=1`: the hooks `cargo xtask bench --app` reads. Without it none of this runs, and
//! the app behaves as it always did.
//!
//! Under it the app prints, on stdout, one line each:
//!
//! - `first-frame <ms>`: once, after the first frame is drawn, measured from the start of `main`.
//! - `frames <n>`: at most once a second, from the render that follows, with how many frames have
//!   been drawn so far. It is how the benchmark knows the window is on screen: a window that is
//!   covered is not drawn, and the count stops. There is no timer behind it, so an idle app
//!   prints nothing.
//! - `scroll-rows <n>` and `scroll-frames <ms>,<ms>,...`: with `RENO_OPEN=run_<id>`, the open
//!   run's transcript is scrolled from its end to its start and back by a fixed step each frame,
//!   and the time between frames is printed. A frame is slow when its time is past the display's
//!   refresh interval.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();
static FRAMES: AtomicU64 = AtomicU64::new(0);
static LAST_REPORT_MS: AtomicU64 = AtomicU64::new(0);

/// How far the scripted scroll moves the transcript each frame, in pixels.
pub(crate) const SCROLL_STEP_PX: f32 = 480.0;
/// A pass of the scripted scroll ends after this many frames at most.
pub(crate) const SCROLL_MAX_FRAMES: usize = 4_000;

/// Turns the hooks on, once, from `main`. `started` is when `main` began.
pub(crate) fn init(enabled: bool, started: Instant) {
    if enabled {
        let _ = START.set(started);
    }
}

pub(crate) fn enabled() -> bool {
    START.get().is_some()
}

/// Prints `first-frame` once the window's first frame has been drawn.
#[allow(clippy::print_stdout)]
pub(crate) fn first_frame(window: &gpui::Window) {
    let Some(start) = START.get().copied() else {
        return;
    };
    window.on_next_frame(move |_, _| {
        println!("first-frame {:.1}", ms(start.elapsed()));
    });
}

/// Called from a render: counts the frame, and reports the count once a second.
#[allow(clippy::print_stdout)]
pub(crate) fn frame() {
    let Some(start) = START.get() else {
        return;
    };
    let n = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    let now = start.elapsed().as_millis() as u64;
    if now.saturating_sub(LAST_REPORT_MS.load(Ordering::Relaxed)) >= 1_000 {
        LAST_REPORT_MS.store(now, Ordering::Relaxed);
        println!("frames {n}");
    }
}

/// Under `RENO_BENCH=1`: activates the app and orders its window to the front. GPUI draws no
/// frame for a window that is covered, and a bare binary started from a terminal opens behind the
/// terminal's own window. Without the hooks the window opens as it always did.
pub(crate) fn bring_to_front(window: &gpui::Window, cx: &mut gpui::App) {
    if enabled() {
        cx.activate(true);
        window.activate_window();
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Prints what the scripted scroll saw.
#[allow(clippy::print_stdout)]
pub(crate) fn report_scroll(rows: usize, intervals: &[Duration]) {
    println!("scroll-rows {rows}");
    let list: Vec<String> = intervals.iter().map(|d| format!("{:.2}", ms(*d))).collect();
    println!("scroll-frames {}", list.join(","));
}

/// Which way the scripted scroll is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    /// From the end of the transcript to its start.
    Up,
    /// And back to the end.
    Down,
    Done,
}

/// The state of the scripted scroll: pure, so the turning points are unit-tested.
#[derive(Debug)]
pub(crate) struct ScrollPlan {
    pass: Pass,
    frames_in_pass: usize,
    still: usize,
}

impl ScrollPlan {
    pub(crate) fn new() -> Self {
        Self {
            pass: Pass::Up,
            frames_in_pass: 0,
            still: 0,
        }
    }

    pub(crate) fn pass(&self) -> Pass {
        self.pass
    }

    /// Called after each step with whether the step moved the transcript. The list stops moving
    /// at its ends; after three frames without moving, or the frame cap, the pass is over.
    pub(crate) fn after_step(&mut self, moved: bool) {
        self.frames_in_pass += 1;
        self.still = if moved { 0 } else { self.still + 1 };
        if self.still >= 3 || self.frames_in_pass >= SCROLL_MAX_FRAMES {
            self.pass = match self.pass {
                Pass::Up => Pass::Down,
                Pass::Down | Pass::Done => Pass::Done,
            };
            self.frames_in_pass = 0;
            self.still = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_ends_when_the_list_stops_moving() {
        let mut plan = ScrollPlan::new();
        assert_eq!(plan.pass(), Pass::Up);
        for _ in 0..50 {
            plan.after_step(true);
        }
        assert_eq!(plan.pass(), Pass::Up);
        plan.after_step(false);
        plan.after_step(false);
        assert_eq!(plan.pass(), Pass::Up, "two still frames are not the end");
        plan.after_step(true);
        for _ in 0..3 {
            plan.after_step(false);
        }
        assert_eq!(plan.pass(), Pass::Down);
        for _ in 0..3 {
            plan.after_step(false);
        }
        assert_eq!(plan.pass(), Pass::Done);
    }

    #[test]
    fn a_pass_that_never_stops_is_cut_at_the_cap() {
        let mut plan = ScrollPlan::new();
        for _ in 0..SCROLL_MAX_FRAMES {
            plan.after_step(true);
        }
        assert_eq!(plan.pass(), Pass::Down);
    }
}
