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

/// Whether the scripted scroll runs: `RENO_BENCH=1` and `RENO_BENCH_SCROLL=1`. The streaming
/// scenario opens a run too, and a script that redraws every frame there would be measured as the
/// app's own streaming cost.
pub(crate) fn scroll_script() -> bool {
    enabled() && std::env::var("RENO_BENCH_SCROLL").is_ok_and(|v| v == "1")
}

/// Under `RENO_BENCH=1`, `RENO_BENCH_OFF=a,b` switches the named redraw sources off, so a run's
/// frame count shows what each one costs. Sources: `pulse` (the shared animation clock's
/// redraws) and `store` (a daemon event's redraw of the root). Always false otherwise.
pub(crate) fn off(source: &str) -> bool {
    static OFF: OnceLock<Vec<String>> = OnceLock::new();
    enabled()
        && OFF
            .get_or_init(|| {
                std::env::var("RENO_BENCH_OFF")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_owned)
                    .collect()
            })
            .iter()
            .any(|name| name == source)
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

/// Starts measuring as soon as an on-screen Find strip has opened. The bench daemon waits for
/// its mock steer so that the burst cannot finish before the window starts drawing it.
#[allow(clippy::print_stdout)]
pub(crate) fn find_streaming(
    store: gpui::Entity<crate::store::Store>,
    run: reno_proto::RunId,
    window: &mut gpui::Window,
) {
    window.on_next_frame(move |window, _| {
        println!("find-ready");
        let started = Instant::now();
        capture_find_frames(store, run, window, started, started, Vec::new());
    });
    window.refresh();
}

fn capture_find_frames(
    store: gpui::Entity<crate::store::Store>,
    run: reno_proto::RunId,
    window: &mut gpui::Window,
    started: Instant,
    last: Instant,
    mut intervals: Vec<Duration>,
) {
    window.on_next_frame(move |window, cx| {
        let now = Instant::now();
        let transcript = store.read(cx).state().transcript(&run);
        if !intervals.is_empty()
            || transcript.is_some_and(|journal| {
                journal
                    .events
                    .iter()
                    .any(|(_, event)| matches!(event, reno_proto::ExecEvent::Text { .. }))
            })
        {
            intervals.push(now.saturating_duration_since(last));
        }
        if let Some(journal) = transcript
            && matches!(
                journal.events.last(),
                Some((_, reno_proto::ExecEvent::Done { .. }))
            )
        {
            let messages = journal
                .events
                .iter()
                .filter(|(_, event)| matches!(event, reno_proto::ExecEvent::Text { .. }))
                .count();
            report_scroll(messages, &intervals);
        } else if now.duration_since(started) < Duration::from_secs(30)
            && intervals.len() < SCROLL_MAX_FRAMES
        {
            capture_find_frames(store, run, window, started, now, intervals);
        } else {
            report_scroll(0, &[]);
        }
    });
    window.refresh();
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

#[cfg(test)]
#[gpui::test]
fn reno_bench_find_opens_the_strip_with_the_query(cx: &mut gpui::TestAppContext) {
    let options = crate::app::Options::parse(std::iter::empty(), |key| match key {
        "RENO_BENCH" => Some("1".to_owned()),
        "RENO_BENCH_FIND" => Some("looked at".to_owned()),
        _ => None,
    })
    .unwrap();
    let store = crate::tests::fake_store(cx);
    crate::tests::wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = crate::tests::window(cx, &store, crate::ui::theme::Appearance::Dark);
    root.update_in(vcx, |root, window, cx| {
        root.open_run(crate::tests::run_id("builder1"), window, cx)
    });
    let screen = root
        .read_with(vcx, |root, _| root.run_screen().cloned())
        .unwrap();
    screen.update_in(vcx, |screen, window, cx| {
        screen.open_bench_find(options.bench_find.as_deref().unwrap(), window, cx)
    });
    crate::tests::draw(vcx);
    assert!(crate::tests::painted(vcx, "find-query"));
    assert_eq!(
        screen
            .read_with(vcx, |screen, _| screen.bench_find_query())
            .as_deref(),
        Some("looked at")
    );
    let run = crate::tests::run_id("builder1");
    store.update(vcx, |store, cx| {
        let seq = store
            .state()
            .transcript(&run)
            .map_or(reno_proto::Seq(1), reno_client::Transcript::resume_from);
        store.apply(
            reno_client::Update::RunEvent {
                run,
                seq,
                event: reno_proto::ExecEvent::Text {
                    text: "I looked at the result.".into(),
                },
            },
            cx,
        );
    });
    crate::tests::draw(vcx);
    vcx.executor().advance_clock(Duration::from_millis(160));
    crate::tests::draw(vcx);
    assert!(
        screen
            .read_with(vcx, |screen, _| screen.find_count_label())
            .is_some_and(|count| count.ends_with("of 1"))
    );
}
