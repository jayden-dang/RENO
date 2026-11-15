//! Animation kit: easing curves, a timing catalog and reusable element helpers over GPUI's
//! [`Animation`]/[`AnimationExt`], plus a throttled pulse clock for repeating loaders.
//!
//! Custom easing is a closure over GPUI's `Fn(f32) -> f32` shape; CSS `cubic-bezier()` is
//! evaluated exactly by [`CubicBezier`].
//!
//! Reduced motion: GPUI's `App::reduce_motion` flag is honoured automatically by every
//! `with_animation` element (oneshots snap to their end state, repeating ones to their start
//! state, no frames are scheduled). [`set_reduced_motion`] and [`reduced_motion`] wrap that
//! switch.
//!
//! translateY is a relative-position `top` inset: Taffy applies relative insets after layout, so
//! like a CSS transform, siblings never move. Upstream GPUI has no scale transform for `div`s,
//! so the scale part of menu and dialog entrances is approximated with fade plus translate.
//!
//! Left out of the source module by choice or for a real blocker: the OS reduced-motion probe
//! (`refresh_system`; it needs `unsafe` Objective-C on macOS, which `unsafe_code = "deny"`
//! forbids, plus Win32 and portal dependencies; callers report the value through
//! [`set_system`]), the settings persistence of the preference, the Windows high-resolution tick,
//! the logo-mark cells, and tests that drive the unported loader widgets.

// A catalog: entries are used as screens adopt them, so some are unreached today.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationElement, App, ElementId, EntityId, Global, Hsla, IntoElement, Rgba,
    SharedString, Styled, Window, px,
};

pub(crate) use gpui::AnimationExt;

// ---------------------------------------------------------------------------
// Pulse clock: throttled drive for the repeating loaders
// ---------------------------------------------------------------------------

/// Repeat-tick interval for the pulse loaders (~30fps). A `with_animation` repeat requests a
/// redraw every display frame while mounted; a shared 30fps clock is visually equivalent for
/// chunky pulses at a quarter of the redraws, and a window with no pulse mounted schedules
/// nothing.
const PULSE_TICK: Duration = Duration::from_millis(33);

/// How long a view stays on the tick list after its last pulse paint. One lease outlives a few
/// missed frames; an unmounted pulse stops renewing and the view drops off.
const PULSE_LEASE: Duration = Duration::from_millis(300);

struct PulseClock {
    epoch: Instant,
    leases: HashMap<EntityId, PulseLease>,
    tick: u64,
    running: bool,
}

struct PulseLease {
    until: Instant,
    stride: u64,
}

impl PulseLease {
    fn renew(&mut self, now: Instant, stride: u64) {
        self.until = now + PULSE_LEASE;
        self.stride = self.stride.min(stride);
    }

    fn take_tick(&mut self, tick: u64) -> bool {
        if !tick.is_multiple_of(self.stride) {
            return false;
        }
        // Each paint re-establishes the fastest mounted animation. A retired 30Hz animation must
        // not permanently pull a 15Hz loader up to 30Hz.
        self.stride = u64::MAX;
        true
    }
}

impl Global for PulseClock {}

impl Default for PulseClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            leases: HashMap::new(),
            tick: 0,
            running: false,
        }
    }
}

/// Current phase `[0,1)` of a repeating spec, plus a lease that keeps the calling view
/// re-rendering at [`PULSE_TICK`] while its pulse stays mounted. All views share one epoch, so
/// instances stay phase-locked. Reduced motion returns a static 0 and schedules nothing.
pub(crate) fn pulse_delta(spec: &MotionSpec, view: EntityId, cx: &mut App) -> f32 {
    pulse_delta_every(spec, view, 1, cx)
}

/// Coarser loaders need only 15Hz; their wall-clock period and phase stay identical.
pub(crate) fn pulse_delta_slow(spec: &MotionSpec, view: EntityId, cx: &mut App) -> f32 {
    pulse_delta_every(spec, view, 2, cx)
}

fn pulse_delta_every(spec: &MotionSpec, view: EntityId, stride: u64, cx: &mut App) -> f32 {
    if cx.reduce_motion() {
        return 0.0;
    }
    schedule_pulse_every(view, stride, cx);
    pulse_phase(spec, cx)
}

fn pulse_phase(spec: &MotionSpec, cx: &mut App) -> f32 {
    let clock = cx.default_global::<PulseClock>();
    (clock.epoch.elapsed().as_secs_f32() / spec.total().as_secs_f32()).fract()
}

/// Where a spinner is in its turn (0..1, one turn per `period`), keeping `view` repainting on
/// the pulse clock while it shows. `None` when motion is reduced: the spinner rests in place.
pub(crate) fn spin_phase(view: EntityId, period: Duration, cx: &mut App) -> Option<f32> {
    if cx.reduce_motion() {
        return None;
    }
    schedule_pulse_every(view, 1, cx);
    let clock = cx.default_global::<PulseClock>();
    Some((clock.epoch.elapsed().as_secs_f32() / period.as_secs_f32()).fract())
}

/// Activity feedback uses a slow, uniform brightness pulse when the system
/// reduces motion. Explicit On and background pause still stop all animation.
/// Decorative animation continues to use the reduced-motion-gated helpers.
#[derive(Clone, Copy)]
pub(crate) struct ActivityPulse {
    phase: f32,
    subtle: bool,
}

impl ActivityPulse {
    pub(crate) fn opacity(&self, cell_phase: f32, dim: f32) -> f32 {
        if self.subtle {
            // No travelling chase or size change, just a gentle 2.4s fade.
            0.6 + 0.2 * pulse_wave(self.phase)
        } else {
            gspin_opacity(self.phase + cell_phase, dim)
        }
    }
}

pub(crate) fn activity_pulse(view: EntityId, cx: &mut App) -> ActivityPulse {
    activity_pulse_every(view, 1, cx)
}

pub(crate) fn activity_pulse_slow(view: EntityId, cx: &mut App) -> ActivityPulse {
    activity_pulse_every(view, 2, cx)
}

fn activity_pulse_every(view: EntityId, stride: u64, cx: &mut App) -> ActivityPulse {
    let subtle = cx.reduce_motion();
    let animate = !subtle
        || cx.try_global::<MotionState>().is_some_and(|state| {
            state.preference == ReduceMotion::System
                && state.system
                && !(state.pause_in_background && !state.active)
        });
    let phase = if animate {
        schedule_pulse_every(view, if subtle { 2 } else { stride }, cx);
        pulse_phase(if subtle { &PULSE } else { &GRADIENT_SPIN }, cx)
    } else {
        0.0
    };
    ActivityPulse { phase, subtle }
}

fn schedule_pulse_every(view: EntityId, stride: u64, cx: &mut App) {
    let clock = cx.default_global::<PulseClock>();
    let now = Instant::now();
    clock
        .leases
        .entry(view)
        .or_insert(PulseLease {
            until: now + PULSE_LEASE,
            stride,
        })
        .renew(now, stride);
    if !clock.running {
        clock.running = true;
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(PULSE_TICK).await;
                let parked = cx.update(|cx| {
                    let clock = cx.default_global::<PulseClock>();
                    let now = Instant::now();
                    clock.leases.retain(|_, lease| lease.until > now);
                    if clock.leases.is_empty() {
                        clock.running = false;
                        return true;
                    }
                    clock.tick = clock.tick.wrapping_add(1);
                    let tick = clock.tick;
                    let views: Vec<EntityId> = clock
                        .leases
                        .iter_mut()
                        .filter_map(|(view, lease)| lease.take_tick(tick).then_some(*view))
                        .collect();
                    for view in views {
                        cx.notify(view);
                    }
                    false
                });
                if parked {
                    break;
                }
            }
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Cubic bezier
// ---------------------------------------------------------------------------

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function (endpoints fixed at (0,0) and (1,1)).
/// Evaluation solves x(t) = input by Newton iteration with a bisection fallback, the standard
/// UnitBezier approach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CubicBezier {
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
}

impl CubicBezier {
    pub(crate) const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        let aa = 1.0 - c - bb;
        (aa, bb, c)
    }

    fn sample_x(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        ((a * t + b) * t + c) * t
    }

    fn sample_y(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.y1, self.y2);
        ((a * t + b) * t + c) * t
    }

    fn sample_x_derivative(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        (3.0 * a * t + 2.0 * b) * t + c
    }

    /// Curve parameter `t` for a given progress `x` (both 0..1).
    fn solve_t_for_x(&self, x: f32) -> f32 {
        // Newton-Raphson.
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = self.sample_x_derivative(t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        // Bisection fallback (x(t) is monotonic for valid CSS beziers).
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if self.sample_x(mid) < x {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }

    /// Eased output for input progress `x` in `[0,1]` (clamped).
    pub(crate) fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        // f32 rounding can push sample_y a hair past 1.0 (observed 1.000000119 near the end of
        // menu animations); GPUI's animation element asserts `delta` in [0,1] and aborts, so
        // clamp the output hard.
        self.sample_y(self.solve_t_for_x(x)).clamp(0.0, 1.0)
    }

    /// This curve as a GPUI easing closure.
    pub(crate) fn easing(self) -> impl Fn(f32) -> f32 + 'static {
        move |x| self.eval(x)
    }
}

/// Signature entrance curve: CSS `cubic-bezier(0.16, 1, 0.3, 1)`.
pub(crate) const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-out`: width/height transitions.
pub(crate) const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// CSS `ease`: quick fades, menu and dialog pops.
pub(crate) const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// `easeOutQuint`: CSS `cubic-bezier(0.22, 1, 0.36, 1)`.
pub(crate) const EASE_OUT_QUINT: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// List resort glide.
pub(crate) const EASE_RESORT: CubicBezier = EASE_OUT_QUINT;
/// CSS `ease-in-out`.
pub(crate) const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
/// Tailwind's default transition curve: CSS `cubic-bezier(0.4, 0, 0.2, 1)`.
pub(crate) const EASE_TAILWIND: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);
/// CSS `cubic-bezier(0.2, 0, 0, 1)`: fast start, long soft landing.
pub(crate) const EASE_OUT_SOFT: CubicBezier = CubicBezier::new(0.2, 0.0, 0.0, 1.0);

// ---------------------------------------------------------------------------
// Motion specs (the catalog)
// ---------------------------------------------------------------------------

/// One catalog entry: duration, optional delay and curve. The delay is folded into the GPUI
/// animation timeline (GPUI `Animation` has no native delay): the animation runs for
/// `delay + duration` and [`progress`](Self::progress) holds 0 until the delay has elapsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MotionSpec {
    pub(crate) duration_ms: u64,
    pub(crate) delay_ms: u64,
    pub(crate) curve: CubicBezier,
}

impl MotionSpec {
    pub(crate) const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self {
            duration_ms,
            delay_ms: 0,
            curve,
        }
    }

    pub(crate) const fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// Wall-clock span of the whole timeline (delay plus duration).
    pub(crate) fn total(&self) -> Duration {
        Duration::from_millis(self.delay_ms + self.duration_ms)
    }

    /// Eased progress (0..1) for a raw timeline delta (0..1 across [`total`](Self::total)).
    /// Pure, so it is unit-testable without a window.
    pub(crate) fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }

    /// A oneshot GPUI [`Animation`] for this spec (delay folded in). The wall-clock span honours
    /// [`speed_scale`].
    pub(crate) fn animation(&self) -> Animation {
        let spec = *self;
        Animation::new(spec.total().mul_f32(speed_scale())).with_easing(move |d| spec.progress(d))
    }

    /// A repeating GPUI [`Animation`] with linear easing over the raw period, for pulse loaders
    /// whose per-cell easing happens in the animator.
    pub(crate) fn repeating(&self) -> Animation {
        Animation::new(self.total()).repeat()
    }
}

/// Entrances: 0.5s expo-out fade plus 4px rise.
pub(crate) const FADE_IN: MotionSpec = MotionSpec::new(500, EASE_OUT_EXPO);
/// Quick fade: 0.15s.
pub(crate) const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE);
/// Popover in: 0.14s.
pub(crate) const MENU_IN: MotionSpec = MotionSpec::new(140, EASE);
/// Popover out: 0.1s, quicker than the entrance so exits get out of the way.
pub(crate) const MENU_OUT: MotionSpec = MotionSpec::new(100, EASE);
/// Dialog in: 0.18s.
pub(crate) const DIALOG_IN: MotionSpec = MotionSpec::new(180, EASE);
/// Splash exit: 0.5s fade and 6px lift after a 0.15s hold.
pub(crate) const SPLASH_OUT: MotionSpec = MotionSpec::new(500, EASE).with_delay(150);
/// Sidebar and pane width/height transitions: 200ms ease-out.
pub(crate) const RESIZE: MotionSpec = MotionSpec::new(200, EASE_OUT);
/// Tab drag-reorder slide: 150ms.
pub(crate) const TAB_SLIDE: MotionSpec = MotionSpec::new(150, EASE_OUT);
/// Per-file collapse height: 180ms.
pub(crate) const COLLAPSE: MotionSpec = MotionSpec::new(180, EASE_OUT);
/// Chevron rotate: 200ms (approximated as a crossfade; `div`s have no rotation transform).
pub(crate) const CHEVRON: MotionSpec = MotionSpec::new(200, EASE);
/// CSS `transition-colors` default: 150ms over [`EASE_TAILWIND`].
pub(crate) const HOVER_FADE: MotionSpec = MotionSpec::new(150, EASE_TAILWIND);
/// Glyph state morph: 280ms over [`EASE_OUT_QUINT`].
pub(crate) const GLYPH_STATE: MotionSpec = MotionSpec::new(280, EASE_OUT_QUINT);
/// Selection indicator glide between the segments of a pill nav: 260ms.
pub(crate) const INDICATOR_SLIDE: MotionSpec = MotionSpec::new(260, EASE_OUT_SOFT);
/// Loader pulse period: 2.4s.
pub(crate) const PULSE: MotionSpec = MotionSpec::new(2400, EASE);

/// Wallpaper replacement: immediate attack with a short, soft landing.
pub(crate) const WALLPAPER_CROSSFADE: MotionSpec =
    MotionSpec::new(180, CubicBezier::new(1.0 / 3.0, 1.0, 2.0 / 3.0, 1.0));
/// Panel columns share the dialog entrance's gentle acceleration and soft
/// landing. Wide moves stretch this base to at most 240ms in the shell.
pub(crate) const PANEL_RESIZE: MotionSpec = DIALOG_IN;
/// Reversible new-thread ↔ session handoff. The shared composer moves and
/// morphs on a fast-starting, soft-landing curve while the canvas/transcript
/// crossfade is staged around it. Slightly longer than a utility transition,
/// but still short enough to acknowledge a send immediately.
pub(crate) const NEW_THREAD_TRANSITION: MotionSpec = MotionSpec::new(420, EASE_RESORT);
/// Rail-tick / scroll-to-row glide: 500ms ease-in-out over the whole distance
/// (Electron parity — the original rail rode the browser's native smooth
/// scroll, a fixed-duration gentle ease, never percent-of-remaining).
pub(crate) const SCROLL_GLIDE: MotionSpec = MotionSpec::new(500, EASE_IN_OUT);
/// Gradient matrix spinner wave period: 750ms.
pub(crate) const GRADIENT_SPIN: MotionSpec = MotionSpec::new(750, EASE);

/// Gradient-matrix spinner wave: intensity (0..1) of cell `wave_index` out of
/// `wave_count` diagonals, at raw delta `raw_delta` of the 750ms period. The wave
/// front travels across diagonals once per period.
pub(crate) fn matrix_wave(raw_delta: f32, wave_index: usize, wave_count: usize) -> f32 {
    let count = wave_count.max(1) as f32;
    pulse_wave(staggered_phase(raw_delta, wave_index, 1.0 / count))
}

// ---------------------------------------------------------------------------
// Element helpers (paint-layer entrances)
// ---------------------------------------------------------------------------

/// Standard entrance: opacity 0 to 1 plus translateY 4 to 0 over [`FADE_IN`].
pub(crate) fn fade_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_IN.animation(), |el, t| {
        el.relative().opacity(t).top(px(4.0 * (1.0 - t)))
    })
}

/// Quick opacity-only fade over [`FADE_QUICK`].
pub(crate) fn fade_quick<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_QUICK.animation(), |el, t| el.opacity(t))
}

/// Popover entrance: fade plus translateY -2 to 0 over [`MENU_IN`].
pub(crate) fn menu_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    menu_in_from(id, -2.0, element)
}

/// [`menu_in`] travelling from the trigger's side: `from` is the signed starting offset
/// (negative above the resting place for dropdowns, positive below it for upward menus).
pub(crate) fn menu_in_from<E>(
    id: impl Into<ElementId>,
    from: f32,
    element: E,
) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_IN.animation(), move |el, t| {
        el.relative()
            .opacity(0.3 + 0.7 * t)
            .top(px(from * (1.0 - t)))
    })
}

/// [`menu_out`] retreating toward the trigger by half the entrance travel.
pub(crate) fn menu_out_toward<E>(
    id: impl Into<ElementId>,
    toward: f32,
    t: f32,
    element: E,
) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_OUT.animation(), move |el, _| {
        el.relative().opacity(1.0 - t).top(px(toward * 0.5 * t))
    })
}

/// Popover exit: the reverse of [`menu_in`] — fade to 0 + translateY 0→−2 over
/// [`MENU_OUT`]. Unlike the entrances, the eased progress `t` comes from the
/// caller (computed off the popup's closing instant at render
/// time): `with_animation`'s element-id-keyed clock replays from 0 on remount
/// (the hover-blend comment's warning), and a replay mid-exit is a full-opacity
/// flash. The wall-clock progress is monotonic by construction; the animation
/// wrapper here only pumps frames for the exit's span, its own delta unused.
pub(crate) fn menu_out<E>(id: impl Into<ElementId>, t: f32, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_OUT.animation(), move |el, _| {
        el.relative().opacity(1.0 - t).top(px(-2.0 * t))
    })
}

/// New-thread composition entrance: opacity 0→1 while settling 10px down into
/// place over [`FADE_IN`]. Keeping the logo, target selectors, composer, and
/// checkout row under one animation makes the blank canvas arrive as a single
/// object instead of four independently moving pieces.
pub(crate) fn settle_down<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_IN.animation(), |el, t| {
        el.relative().opacity(t).top(px(-10.0 * (1.0 - t)))
    })
}

/// Dialog entrance over [`DIALOG_IN`] (scale approximated with fade plus a 2px rise).
pub(crate) fn dialog_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, DIALOG_IN.animation(), |el, t| {
        el.relative().opacity(t).top(px(2.0 * (1.0 - t)))
    })
}

/// Splash exit: hold 150ms, then fade out and lift 6px over 500ms.
pub(crate) fn splash_out<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, SPLASH_OUT.animation(), |el, t| {
        el.opacity(1.0 - t).top(px(-6.0 * t))
    })
}

// ---------------------------------------------------------------------------
// Loader math (pure)
// ---------------------------------------------------------------------------

/// Pulse floor opacity.
pub(crate) const PULSE_MIN_OPACITY: f32 = 0.08;
/// Pulse floor scale.
pub(crate) const PULSE_MIN_SCALE: f32 = 0.9;
/// Phase lag between neighbouring cells of a multi-cell pulse.
pub(crate) const PULSE_STAGGER: f32 = 0.15 / 2.4;

/// A cell's phase, given the loader's raw phase and the cell's index.
pub(crate) fn staggered_phase(raw_delta: f32, index: usize, stagger: f32) -> f32 {
    (raw_delta - index as f32 * stagger).rem_euclid(1.0)
}

/// Cosine pulse: 0 at phase 0, 1 at phase 0.5, back to 0 at phase 1.
pub(crate) fn pulse_wave(phase: f32) -> f32 {
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

/// Cell opacity for a phase: 0.08 to 1 to 0.08.
pub(crate) fn pulse_opacity(phase: f32) -> f32 {
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * pulse_wave(phase)
}

/// Cell scale for a phase: 0.9 to 1 to 0.9.
pub(crate) fn pulse_scale(phase: f32) -> f32 {
    PULSE_MIN_SCALE + (1.0 - PULSE_MIN_SCALE) * pulse_wave(phase)
}

/// Gradient-spin cell opacity for a local phase `t` (0..1 of the period),
/// ported from the `gradient-spin-pulse` keyframes: full at the cycle
/// start, easing down to `dim` by 45%, resting at `dim` until 92%, then rising
/// back to full — the per-cell phase offset sweeps this pulse across the grid.
pub(crate) fn gspin_opacity(t: f32, dim: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < 0.45 {
        lerp(1.0, dim, t / 0.45)
    } else if t < 0.92 {
        dim
    } else {
        lerp(dim, 1.0, (t - 0.92) / 0.08)
    }
}

/// Linear interpolation.
pub(crate) fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Blend two colours by `t` the way the browser transitions them: component interpolation in
/// sRGB with premultiplied alpha, so a wash fading in from transparent brightens without passing
/// through grey.
pub(crate) fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let (f, g) = (Rgba::from(from), Rgba::from(to));
    let a = lerp(f.a, g.a, t);
    if a <= f32::EPSILON {
        // Both endpoints (effectively) transparent: carry the target's hue.
        return Hsla::from(Rgba { a: 0.0, ..g });
    }
    Hsla::from(Rgba {
        r: lerp(f.r * f.a, g.r * g.a, t) / a,
        g: lerp(f.g * f.a, g.g * g.a, t) / a,
        b: lerp(f.b * f.a, g.b * g.a, t) / a,
        a,
    })
}

// ---------------------------------------------------------------------------
// Resize-edge feedback
// ---------------------------------------------------------------------------

/// Pane resize limits acknowledge a held pointer without persisting an
/// out-of-range size. The small displacement is shared by the shell panes and
/// nested surface splits so every seam has the same physical response.
pub(crate) const RESIZE_EDGE_NUDGE: f32 = 5.0;
pub(crate) const RESIZE_EDGE_BOUNCE_MS: u64 = 220;
pub(crate) const RESIZE_EDGE_BOUNCE_OUT_FRACTION: f32 = 0.32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeEdge {
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResizeDragSample {
    pub(crate) width: f32,
    pub(crate) edge: Option<ResizeEdge>,
    pub(crate) starts_bounce: bool,
}

/// Clamp a resize sample while latching its constrained edge. A held pointer
/// starts one bounce rather than restarting it for every drag event.
pub(crate) fn resize_drag_sample(
    requested: f32,
    min: f32,
    max: f32,
    latched_edge: Option<ResizeEdge>,
    reduced_motion: bool,
) -> ResizeDragSample {
    debug_assert!(min <= max);
    let edge = if requested <= min {
        Some(ResizeEdge::Min)
    } else if requested >= max {
        Some(ResizeEdge::Max)
    } else {
        None
    };
    ResizeDragSample {
        width: requested.clamp(min, max),
        starts_bounce: !reduced_motion && edge.is_some() && edge != latched_edge,
        edge,
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResizeEdgeBounce {
    pub(crate) edge: ResizeEdge,
    pub(crate) started: Instant,
}

impl ResizeEdgeBounce {
    pub(crate) fn new(edge: ResizeEdge) -> Self {
        Self {
            edge,
            started: Instant::now(),
        }
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rounded two-phase pulse: ease out to the overshoot, then take a little
/// longer to ease home. Both joins have zero velocity.
pub(crate) fn resize_bounce_offset(edge: ResizeEdge, raw: f32) -> f32 {
    let raw = raw.clamp(0.0, 1.0);
    let magnitude = if raw < RESIZE_EDGE_BOUNCE_OUT_FRACTION {
        smoothstep(raw / RESIZE_EDGE_BOUNCE_OUT_FRACTION)
    } else {
        1.0 - smoothstep(
            (raw - RESIZE_EDGE_BOUNCE_OUT_FRACTION) / (1.0 - RESIZE_EDGE_BOUNCE_OUT_FRACTION),
        )
    } * RESIZE_EDGE_NUDGE;
    match edge {
        ResizeEdge::Min => -magnitude,
        ResizeEdge::Max => magnitude,
    }
}

// ---------------------------------------------------------------------------
// Interruptible tweens, hover fades and state morphs
// ---------------------------------------------------------------------------
//
// Manual-drive tweens (never `with_animation`, whose element-id-keyed clock replays on
// remount): progress advances from wall time on each evaluation. The stores are main-thread
// `thread_local`s so free-function element builders can blend colours without threading `cx`
// through every signature. All access happens on the UI thread.

/// One element's hover fade: progress runs `origin → target` over
/// [`HOVER_FADE`], re-anchored at `origin` whenever the pointer flips
/// direction mid-flight so the blend is continuous.
#[derive(Debug, Clone, Copy)]
struct FadeEntry {
    origin: f32,
    target: f32,
    started: Instant,
    /// Frame counter at the last read (liveness stamp — see module notes).
    seen: u64,
}

impl FadeEntry {
    fn value(&self, now: Instant, duration: Duration) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        if duration.is_zero() || elapsed >= duration {
            return self.target;
        }
        let raw = elapsed.as_secs_f32() / duration.as_secs_f32();
        lerp(self.origin, self.target, HOVER_FADE.curve.eval(raw))
    }

    fn settled(&self, now: Instant, duration: Duration) -> bool {
        self.origin == self.target || now.saturating_duration_since(self.started) >= duration
    }
}

/// Interruptible height tween for the shared collapsible sections.
/// The rendered element owns the frame clock; this state preserves the current
/// interpolated height when a second click reverses an in-flight transition.
#[derive(Clone, Copy)]
pub(crate) struct DisclosureMotion {
    pub(crate) epoch: u64,
    pub(crate) from: f32,
    pub(crate) to: f32,
    pub(crate) started: std::time::Instant,
}

impl DisclosureMotion {
    pub(crate) fn new(epoch: u64, from: f32, to: f32) -> Self {
        Self {
            epoch,
            from,
            to,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn current(self) -> f32 {
        let total = COLLAPSE.total().as_secs_f32();
        let raw = if total > 0.0 {
            self.started.elapsed().as_secs_f32() / total
        } else {
            1.0
        };
        lerp(self.from, self.to, COLLAPSE.progress(raw))
    }

    pub(crate) fn animating(self) -> bool {
        self.started.elapsed() < COLLAPSE.total() + Duration::from_millis(120)
    }
}

/// Interruptible slide of a selection indicator between segment slots.
/// `position` is measured in segments (`1.5` sits halfway between the second
/// and third), so a retarget mid-flight continues from the drawn position
/// instead of jumping. Pure core — callers pass `now`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IndicatorSlide {
    from: f32,
    to: f32,
    started: Instant,
}

impl IndicatorSlide {
    pub(crate) fn at(position: f32, now: Instant) -> Self {
        Self {
            from: position,
            to: position,
            started: now,
        }
    }

    fn duration() -> Duration {
        INDICATOR_SLIDE.total().mul_f32(speed_scale())
    }

    fn progress(&self, now: Instant) -> f32 {
        let duration = Self::duration();
        if duration.is_zero() {
            return 1.0;
        }
        let raw =
            now.saturating_duration_since(self.started).as_secs_f32() / duration.as_secs_f32();
        INDICATOR_SLIDE.progress(raw)
    }

    pub(crate) fn value_at(&self, now: Instant) -> f32 {
        lerp(self.from, self.to, self.progress(now))
    }

    /// Aim at a new slot. Reduced motion snaps.
    pub(crate) fn retarget(&mut self, to: f32, reduced: bool, now: Instant) {
        let from = if reduced { to } else { self.value_at(now) };
        *self = Self {
            from,
            to,
            started: now,
        };
    }

    pub(crate) fn animating_at(&self, now: Instant) -> bool {
        self.from != self.to && now.saturating_duration_since(self.started) < Self::duration()
    }
}

/// Per-key hover progress store. Pure core (explicit `now`) — unit-testable;
/// the thread-local wrappers below feed it wall time.
#[derive(Default)]
pub(crate) struct HoverFades {
    entries: HashMap<String, FadeEntry>,
    frame: u64,
}

impl HoverFades {
    fn duration() -> Duration {
        HOVER_FADE.total().mul_f32(speed_scale())
    }

    /// Pointer entered (`hovered`) or left the element behind `key`. Reduced
    /// motion snaps straight to the endpoint.
    pub(crate) fn set_at(&mut self, key: &str, hovered: bool, reduced: bool, now: Instant) {
        let target = if hovered { 1.0 } else { 0.0 };
        let duration = Self::duration();
        let current = self
            .entries
            .get(key)
            .map(|e| e.value(now, duration))
            .unwrap_or(0.0);
        if target == 0.0 && !self.entries.contains_key(key) {
            return; // never-hovered element reporting a leave — nothing to do
        }
        let origin = if reduced { target } else { current };
        let seen = self.frame;
        self.entries.insert(
            key.to_string(),
            FadeEntry {
                origin,
                target,
                started: now,
                seen,
            },
        );
    }

    /// Hover progress (0..1) for `key` at `now`; stamps liveness.
    pub(crate) fn value_at(&mut self, key: &str, now: Instant) -> f32 {
        let frame = self.frame;
        match self.entries.get_mut(key) {
            Some(entry) => {
                entry.seen = frame;
                entry.value(now, Self::duration())
            }
            None => 0.0,
        }
    }

    /// Once-per-frame bookkeeping: advance the frame counter, prune entries
    /// that settled back to rest or went a full frame unread (unmounted), and
    /// report whether any fade is still mid-flight (→ keep frames coming).
    pub(crate) fn tick_at(&mut self, now: Instant) -> bool {
        self.frame += 1;
        let frame = self.frame;
        let duration = Self::duration();
        let mut active = false;
        self.entries.retain(|_, entry| {
            // Unread through the whole previous frame: the element unmounted
            // (its leave event will never come) — drop the entry.
            if entry.seen + 1 < frame {
                return false;
            }
            let settled = entry.settled(now, duration);
            if !settled {
                active = true;
            }
            // Settled at rest — steady state, indistinguishable from absent.
            !(settled && entry.target == 0.0)
        });
        active
    }
}

thread_local! {
    static HOVER_FADES: RefCell<HoverFades> = RefCell::new(HoverFades::default());
}

/// Hover progress (0..1) for `key` this frame.
pub(crate) fn hover_t(key: &str) -> f32 {
    HOVER_FADES.with(|fades| fades.borrow_mut().value_at(key, Instant::now()))
}

/// Record a hover flip for `key` (reduced motion snaps).
pub(crate) fn set_hover(key: &str, hovered: bool, reduced: bool) {
    HOVER_FADES.with(|fades| {
        fades
            .borrow_mut()
            .set_at(key, hovered, reduced, Instant::now())
    });
}

/// An `.on_hover` listener driving the fade for `key` — pair with
/// [`hover_t`]/[`hover_blend`] reads of the same key in the same element.
pub(crate) fn hover_listener(
    key: impl Into<SharedString>,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let key = key.into();
    move |hovered, window, cx| {
        set_hover(&key, *hovered, reduced_motion(cx));
        // Event-dispatch context: `request_animation_frame` is draw-phase-only
        // (it resolves the current view) — `refresh` marks the whole window
        // dirty, the root render re-evaluates the blend and keeps frames
        // coming via its tail while the fade is mid-flight.
        window.refresh();
    }
}

/// Frame-drive hook: call ONCE per window frame (the shell render tail); true
/// while any hover fade is mid-flight and frames must keep coming.
pub(crate) fn hover_fades_active() -> bool {
    HOVER_FADES.with(|fades| fades.borrow_mut().tick_at(Instant::now()))
}

/// The standard hover blend: rest → hover color at `key`'s current progress.
pub(crate) fn hover_blend(key: &str, rest: Hsla, hover: Hsla) -> Hsla {
    mix(rest, hover, hover_t(key))
}

// ---------------------------------------------------------------------------
// Two-state glyph morphs (CSS `transition` on a `data-state` flip)
// ---------------------------------------------------------------------------
//
// Icons morph between two states (the sidebar glyph's panel narrows when
// the sidebar closes). Same manual clock and liveness rules as the hover
// fades, but keyed by the element's *state* rather than pointer events: the
// first read of a key snaps to its state (mounting never replays a morph), a
// flip re-anchors at the current value so a mid-flight toggle reverses
// smoothly, and an entry unread for a full frame is pruned.

#[derive(Debug, Clone, Copy)]
struct StateEntry {
    origin: f32,
    target: f32,
    started: Instant,
    spec: MotionSpec,
    seen: u64,
}

impl StateEntry {
    fn duration(&self) -> Duration {
        self.spec.total().mul_f32(speed_scale())
    }

    fn value(&self, now: Instant) -> f32 {
        let duration = self.duration();
        let elapsed = now.saturating_duration_since(self.started);
        if duration.is_zero() || elapsed >= duration {
            return self.target;
        }
        let raw = elapsed.as_secs_f32() / duration.as_secs_f32();
        lerp(self.origin, self.target, self.spec.progress(raw))
    }

    fn settled(&self, now: Instant) -> bool {
        self.origin == self.target || now.saturating_duration_since(self.started) >= self.duration()
    }
}

/// Per-key state-morph store. Pure core (explicit `now`) — unit-testable.
#[derive(Default)]
pub(crate) struct StateMorphs {
    entries: HashMap<String, StateEntry>,
    frame: u64,
}

impl StateMorphs {
    /// Progress (0 = off, 1 = on) of `key` toward `on` at `now`; stamps
    /// liveness. Reduced motion snaps a flip straight to its endpoint.
    pub(crate) fn value_at(
        &mut self,
        key: &str,
        on: bool,
        spec: MotionSpec,
        reduced: bool,
        now: Instant,
    ) -> f32 {
        let target = if on { 1.0 } else { 0.0 };
        let frame = self.frame;
        let Some(entry) = self.entries.get_mut(key) else {
            self.entries.insert(
                key.to_string(),
                StateEntry {
                    origin: target,
                    target,
                    started: now,
                    spec,
                    seen: frame,
                },
            );
            return target;
        };
        if entry.target != target {
            let current = entry.value(now);
            *entry = StateEntry {
                origin: if reduced { target } else { current },
                target,
                started: now,
                spec,
                seen: frame,
            };
        }
        entry.seen = frame;
        entry.value(now)
    }

    /// Once-per-frame bookkeeping: prune unmounted entries and report whether
    /// any morph is still mid-flight (→ keep frames coming).
    pub(crate) fn tick_at(&mut self, now: Instant) -> bool {
        self.frame += 1;
        let frame = self.frame;
        let mut active = false;
        self.entries.retain(|_, entry| {
            if entry.seen + 1 < frame {
                return false;
            }
            active |= !entry.settled(now);
            true
        });
        active
    }
}

thread_local! {
    static STATE_MORPHS: RefCell<StateMorphs> = RefCell::new(StateMorphs::default());
}

/// Morph progress (0..1) of `key` toward `on` this frame.
pub(crate) fn state_t(key: &str, on: bool, spec: MotionSpec, reduced: bool) -> f32 {
    STATE_MORPHS.with(|morphs| {
        morphs
            .borrow_mut()
            .value_at(key, on, spec, reduced, Instant::now())
    })
}

/// Frame-drive hook, beside [`hover_fades_active`]: call ONCE per window frame.
pub(crate) fn state_morphs_active() -> bool {
    STATE_MORPHS.with(|morphs| morphs.borrow_mut().tick_at(Instant::now()))
}

// ---------------------------------------------------------------------------
// Speed and reduced motion
// ---------------------------------------------------------------------------

/// Dev knob (`RENO_MOTION_SCALE`, default 1): stretches every catalog timeline by this factor,
/// so `RENO_MOTION_SCALE=10` slows a 200ms tween to 2s for frame sampling. Read once.
pub(crate) fn speed_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("RENO_MOTION_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|s| s.is_finite())
            .map_or(1.0, |s| s.clamp(0.01, 100.0))
    })
}

/// Global reduced-motion flag. GPUI snaps every `with_animation` element when set.
pub(crate) fn set_reduced_motion(cx: &mut App, reduced: bool) {
    cx.set_reduce_motion(reduced);
}

/// Read the global reduced-motion flag.
pub(crate) fn reduced_motion(cx: &App) -> bool {
    cx.reduce_motion()
}

/// The user's reduced-motion preference. Persisting it is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReduceMotion {
    /// Follow the OS accessibility setting.
    #[default]
    System,
    On,
    Off,
}

impl ReduceMotion {
    pub(crate) const ALL: [Self; 3] = [Self::System, Self::On, Self::Off];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::On => "On",
            Self::Off => "Off",
        }
    }
}

/// Inputs to the global flag: the chosen preference, plus what the OS and the main window last
/// reported. Kept apart so a system change while the user has pinned On or Off still lands, and
/// takes effect when they return to System.
struct MotionState {
    preference: ReduceMotion,
    pause_in_background: bool,
    system: bool,
    active: bool,
}

impl Global for MotionState {}

fn state_mut(cx: &mut App) -> Option<&mut MotionState> {
    cx.has_global::<MotionState>()
        .then(|| cx.global_mut::<MotionState>())
}

/// Combine the preference with the OS setting and window focus.
pub(crate) fn resolve(
    preference: ReduceMotion,
    system: bool,
    pause_in_background: bool,
    active: bool,
) -> bool {
    let reduced = match preference {
        ReduceMotion::System => system,
        ReduceMotion::On => true,
        ReduceMotion::Off => false,
    };
    reduced || (pause_in_background && !active)
}

/// Install the motion state and set the flag before the first window opens, so boot animations
/// already honour it. The OS value starts false until the caller reports it with [`set_system`].
pub(crate) fn init(preference: ReduceMotion, pause_in_background: bool, cx: &mut App) {
    cx.set_global(MotionState {
        preference,
        pause_in_background,
        system: false,
        active: true,
    });
    apply(cx);
}

pub(crate) fn preference(cx: &App) -> ReduceMotion {
    cx.try_global::<MotionState>()
        .map(|state| state.preference)
        .unwrap_or_default()
}

pub(crate) fn pause_in_background(cx: &App) -> bool {
    cx.try_global::<MotionState>()
        .is_some_and(|state| state.pause_in_background)
}

/// The OS setting as last reported.
pub(crate) fn system_reduces_motion(cx: &App) -> bool {
    cx.try_global::<MotionState>()
        .is_some_and(|state| state.system)
}

pub(crate) fn set_preference(preference: ReduceMotion, cx: &mut App) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    if state.preference == preference {
        return;
    }
    state.preference = preference;
    apply(cx);
}

pub(crate) fn set_pause_in_background(enabled: bool, cx: &mut App) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    if state.pause_in_background == enabled {
        return;
    }
    state.pause_in_background = enabled;
    apply(cx);
}

/// Main-window focus moved. Only the app installs motion state; windows without it ignore this.
pub(crate) fn window_activation_changed(active: bool, cx: &mut App) {
    set_window_active(active, cx);
}

fn set_window_active(active: bool, cx: &mut App) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    state.active = active;
    apply(cx);
}

/// Report the OS reduced-motion setting. Reading it needs platform calls (`unsafe` Objective-C on
/// macOS, Win32 on Windows, the desktop portal on Linux), which this crate does not make: the
/// caller that owns a platform layer reads it and calls this.
pub(crate) fn set_system(system: bool, cx: &mut App) {
    let Some(state) = state_mut(cx) else {
        return;
    };
    state.system = system;
    apply(cx);
}

fn apply(cx: &mut App) {
    let Some(state) = cx.try_global::<MotionState>() else {
        return;
    };
    let reduced = resolve(
        state.preference,
        state.system,
        state.pause_in_background,
        state.active,
    );
    // GPUI refreshes every window only when the flag actually flips.
    set_reduced_motion(cx, reduced);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVES: [CubicBezier; 5] = [EASE_OUT_EXPO, EASE_OUT, EASE, EASE_RESORT, EASE_IN_OUT];

    fn assert_close(actual: f32, expected: f32, tol: f32, ctx: &str) {
        assert!(
            (actual - expected).abs() <= tol,
            "{ctx}: got {actual}, expected {expected} +-{tol}"
        );
    }

    #[test]
    fn pulse_stride_reestablishes_after_each_paint() {
        let now = Instant::now();
        let mut lease = PulseLease {
            until: now + PULSE_LEASE,
            stride: 2,
        };
        assert!(!lease.take_tick(1), "slow loader skips odd ticks");
        assert!(lease.take_tick(2));
        lease.renew(now, 2);
        lease.renew(now, 1);
        assert!(lease.take_tick(3), "fast animation wins while mounted");
        lease.renew(now, 2);
        assert!(lease.take_tick(4));
        lease.renew(now, 2);
        assert!(
            !lease.take_tick(5),
            "retired fast animation leaves no sticky stride"
        );
        assert!(lease.take_tick(6));
        assert!(!lease.take_tick(7), "unpainted view cannot renew itself");
        assert!(lease.until <= now + PULSE_LEASE);
    }

    #[test]
    fn eval_never_escapes_unit_interval_dense_sweep() {
        // Regression: f32 rounding produced 1.000000119 near the tail of EASE_OUT_EXPO, tripping
        // GPUI's `delta` in [0,1] assert.
        for curve in CURVES {
            for i in 0..=100_000u32 {
                let x = i as f32 / 100_000.0;
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
            for x in [0.999_999f32, 0.999_999_9, 1.0 - f32::EPSILON] {
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
        }
    }

    #[test]
    fn bezier_linear_is_identity() {
        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for x in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            assert_close(linear.eval(x), x, 1e-4, "linear");
        }
    }

    #[test]
    fn bezier_known_values() {
        // References computed independently with 80-step bisection.
        let cases: [(&str, CubicBezier, [f32; 5]); 3] = [
            (
                "expo",
                EASE_OUT_EXPO,
                [0.494391, 0.825622, 0.971779, 0.997677, 0.999878],
            ),
            (
                "ease-out",
                EASE_OUT,
                [0.160572, 0.378138, 0.684643, 0.906535, 0.982973],
            ),
            (
                "ease",
                EASE,
                [0.094796, 0.408511, 0.802403, 0.960459, 0.994316],
            ),
        ];
        for (name, curve, expected) in cases {
            for (x, want) in [0.1, 0.25, 0.5, 0.75, 0.9].into_iter().zip(expected) {
                assert_close(curve.eval(x), want, 1e-3, name);
            }
        }
    }

    #[test]
    fn bezier_endpoints_and_clamping() {
        for curve in CURVES {
            assert_eq!(curve.eval(0.0), 0.0);
            assert_eq!(curve.eval(1.0), 1.0);
            assert_eq!(curve.eval(-0.5), 0.0);
            assert_eq!(curve.eval(1.5), 1.0);
        }
    }

    #[test]
    fn bezier_is_monotonic_for_catalog_curves() {
        for curve in CURVES {
            let mut last = 0.0;
            for i in 0..=100 {
                let y = curve.eval(i as f32 / 100.0);
                assert!(y >= last - 1e-4, "monotonicity violated at {i}");
                last = y;
            }
        }
    }

    #[test]
    fn spec_delay_holds_then_runs() {
        // SPLASH_OUT: 150ms delay plus 500ms run = 650ms total.
        assert_eq!(SPLASH_OUT.total(), Duration::from_millis(650));
        assert_eq!(SPLASH_OUT.progress(0.0), 0.0);
        // Still inside the delay window at raw 0.2 (130ms < 150ms).
        assert_eq!(SPLASH_OUT.progress(0.2), 0.0);
        assert_eq!(SPLASH_OUT.progress(1.0), 1.0);
        assert_eq!(SPLASH_OUT.progress(2.0), 1.0);
        let mid = SPLASH_OUT.progress(0.65);
        assert!(mid > 0.0 && mid < 1.0);
        assert_close(
            FADE_IN.progress(0.5),
            EASE_OUT_EXPO.eval(0.5),
            1e-6,
            "no-delay",
        );
    }

    #[test]
    fn catalog_timings() {
        assert_eq!(FADE_IN.duration_ms, 500);
        assert_eq!(FADE_QUICK.duration_ms, 150);
        assert_eq!(MENU_IN.duration_ms, 140);
        assert_eq!(MENU_OUT.duration_ms, 100);
        assert_eq!(DIALOG_IN.duration_ms, 180);
        assert_eq!((SPLASH_OUT.duration_ms, SPLASH_OUT.delay_ms), (500, 150));
        assert_eq!(RESIZE.duration_ms, 200);
        assert_eq!(TAB_SLIDE.duration_ms, 150);
        assert_eq!(COLLAPSE.duration_ms, 180);
        assert_eq!(CHEVRON.duration_ms, 200);
        assert_eq!(PULSE.duration_ms, 2400);
        assert_eq!(
            (HOVER_FADE.duration_ms, HOVER_FADE.curve),
            (150, EASE_TAILWIND)
        );
        assert_eq!(EASE_OUT_EXPO, CubicBezier::new(0.16, 1.0, 0.3, 1.0));
        assert_eq!(EASE_OUT_QUINT, CubicBezier::new(0.22, 1.0, 0.36, 1.0));
    }

    #[test]
    fn pulse_wave_endpoints() {
        assert_close(pulse_wave(0.0), 0.0, 1e-6, "wave start");
        assert_close(pulse_wave(0.5), 1.0, 1e-6, "wave peak");
        assert_close(pulse_wave(1.0), 0.0, 1e-6, "wave end");
        assert_close(pulse_opacity(0.0), 0.08, 1e-6, "opacity floor");
        assert_close(pulse_opacity(0.5), 1.0, 1e-6, "opacity peak");
        assert_close(pulse_scale(0.0), 0.9, 1e-6, "scale floor");
        assert_close(pulse_scale(0.5), 1.0, 1e-6, "scale peak");
    }

    #[test]
    fn stagger_wraps_and_orders_cells() {
        assert_close(staggered_phase(0.0, 0, PULSE_STAGGER), 0.0, 1e-6, "cell 0");
        assert_close(
            staggered_phase(0.0, 1, PULSE_STAGGER),
            1.0 - PULSE_STAGGER,
            1e-5,
            "cell 1 wraps",
        );
        assert_close(
            staggered_phase(0.3, 2, PULSE_STAGGER),
            staggered_phase(0.3 + 1.0, 2, PULSE_STAGGER),
            2e-6,
            "periodic",
        );
    }

    #[test]
    fn lerp_basics() {
        assert_eq!(lerp(208.0, 400.0, 0.0), 208.0);
        assert_eq!(lerp(208.0, 400.0, 1.0), 400.0);
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
    }

    #[test]
    fn mix_endpoints_and_transparent_blend() {
        let rest = gpui::hsla(0.0, 0.0, 0.235, 1.0);
        let hover = gpui::hsla(0.0, 0.0, 0.29, 1.0);
        assert_eq!(mix(rest, hover, 0.0), rest);
        assert_eq!(mix(rest, hover, 1.0), hover);
        assert_eq!(mix(rest, hover, -1.0), rest, "t clamps low");
        assert_eq!(mix(rest, hover, 2.0), hover, "t clamps high");

        let mid = mix(rest, hover, 0.5);
        assert!(mid.l > rest.l && mid.l < hover.l, "mid lightness {}", mid.l);

        // Transparent to wash: alpha ramps, hue stays the wash's (premultiplied, so never a
        // darkened grey mid-fade).
        let wash = gpui::hsla(0.0, 0.0, 1.0, 0.06);
        let half = mix(gpui::transparent_black(), wash, 0.5);
        assert!((half.a - 0.03).abs() < 1e-4, "alpha midpoint {}", half.a);
        let half_rgba = Rgba::from(half);
        assert!(
            half_rgba.r > 0.99 && half_rgba.g > 0.99 && half_rgba.b > 0.99,
            "white wash keeps its hue: {half_rgba:?}"
        );
    }

    #[gpui::test]
    fn reduced_motion_flag_round_trips_and_stops_the_pulse(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        cx.update(|cx| {
            let view = cx.new(|_| ());
            set_reduced_motion(cx, true);
            assert!(reduced_motion(cx));
            assert_eq!(pulse_delta(&PULSE, view.entity_id(), cx), 0.0);
            assert!(
                !cx.try_global::<PulseClock>()
                    .is_some_and(|clock| clock.leases.contains_key(&view.entity_id())),
                "reduced motion schedules nothing"
            );
            set_reduced_motion(cx, false);
            assert!(!reduced_motion(cx));
        });
    }

    #[test]
    fn indicator_slide_is_interruptible_and_snaps_when_reduced() {
        let t0 = Instant::now();
        let mut slide = IndicatorSlide::at(0.0, t0);
        assert!(!slide.animating_at(t0));
        slide.retarget(2.0, false, t0);
        assert_eq!(slide.value_at(t0), 0.0);
        assert!(slide.animating_at(t0 + Duration::from_millis(50)));
        let mid = t0 + Duration::from_millis(80);
        let drawn = slide.value_at(mid);
        assert!(drawn > 0.0 && drawn < 2.0);
        // Reversing continues from where the thumb is drawn.
        slide.retarget(0.0, false, mid);
        assert_eq!(slide.value_at(mid), drawn);
        let end = mid + INDICATOR_SLIDE.total();
        assert_eq!(slide.value_at(end), 0.0);
        assert!(!slide.animating_at(end));
        slide.retarget(1.0, true, end);
        assert_eq!(slide.value_at(end), 1.0);
        assert!(!slide.animating_at(end));
    }

    #[test]
    fn reduced_activity_pulse_is_slow_uniform_and_gentle() {
        for phase in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9] {
            let pulse = ActivityPulse {
                phase,
                subtle: true,
            };
            let opacity = pulse.opacity(0.0, 0.1);
            assert!((0.6..=0.8).contains(&opacity));
            assert_eq!(opacity, pulse.opacity(0.5, 0.1), "no travelling wave");
        }
        let rest = ActivityPulse {
            phase: 0.0,
            subtle: true,
        }
        .opacity(0.0, 0.1);
        let crest = ActivityPulse {
            phase: 0.5,
            subtle: true,
        }
        .opacity(0.0, 0.1);
        assert!(
            crest > rest,
            "the activity cue must visibly change brightness"
        );
    }

    #[gpui::test]
    fn activity_pulse_honors_explicit_on_and_background_pause(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        cx.update(|cx| {
            for (preference, system, pause_in_background, active, animates) in [
                (ReduceMotion::On, false, false, true, false),
                (ReduceMotion::On, true, false, true, false),
                (ReduceMotion::System, true, true, false, false),
                (ReduceMotion::Off, true, true, false, false),
                (ReduceMotion::System, true, true, true, true),
                (ReduceMotion::System, true, false, false, true),
                (ReduceMotion::System, false, false, true, true),
                (ReduceMotion::Off, true, false, true, true),
            ] {
                cx.set_global(MotionState {
                    preference,
                    pause_in_background,
                    system,
                    active,
                });
                apply(cx);
                let view = cx.new(|_| ());
                let pulse = activity_pulse(view.entity_id(), cx);
                let leased = cx
                    .try_global::<PulseClock>()
                    .is_some_and(|clock| clock.leases.contains_key(&view.entity_id()));
                assert_eq!(
                    leased, animates,
                    "{preference:?}, system={system}, pause={pause_in_background}, active={active}"
                );
                if reduced_motion(cx) {
                    assert!(pulse.subtle);
                    let decorative_view = cx.new(|_| ());
                    assert_eq!(pulse_delta(&PULSE, decorative_view.entity_id(), cx), 0.0);
                    assert!(!cx.try_global::<PulseClock>().is_some_and(|clock| {
                        clock.leases.contains_key(&decorative_view.entity_id())
                    }));
                }
            }
        });
    }

    #[test]
    fn hover_fade_ramps_and_reverses_continuously() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);

        // Enter: 0 at the flip, mid-flight strictly between, 1 at 150ms.
        fades.set_at("pill", true, false, t0);
        assert_eq!(fades.value_at("pill", t0), 0.0);
        let mid = fades.value_at("pill", ms(75));
        assert!(mid > 0.0 && mid < 1.0, "mid-flight enter: {mid}");
        assert_eq!(fades.value_at("pill", ms(150)), 1.0);
        assert_eq!(fades.value_at("pill", ms(400)), 1.0, "clamps past the end");

        // Leave mid-flight re-anchors at the current value — no jump.
        fades.set_at("pill", true, false, t0);
        let at_flip = fades.value_at("pill", ms(75));
        fades.set_at("pill", false, false, ms(75));
        let after_flip = fades.value_at("pill", ms(75));
        assert!(
            (after_flip - at_flip).abs() < 1e-4,
            "continuity: {at_flip} vs {after_flip}"
        );
        let falling = fades.value_at("pill", ms(140));
        assert!(falling < after_flip, "fades back down");
        assert_eq!(fades.value_at("pill", ms(225)), 0.0, "lands at rest");
    }

    #[test]
    fn state_morph_snaps_on_mount_and_reverses_continuously() {
        let mut morphs = StateMorphs::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        let morph = |m: &mut StateMorphs, on, at| m.value_at("glyph", on, GLYPH_STATE, false, at);

        // First sight lands on the state — no replay at mount.
        assert_eq!(morph(&mut morphs, true, t0), 1.0);
        assert!(!morphs.tick_at(t0));

        // Flip: starts from 1, mid-flight strictly between, lands at 0.
        assert_eq!(morph(&mut morphs, false, ms(10)), 1.0);
        let mid = morph(&mut morphs, false, ms(80));
        assert!(mid > 0.0 && mid < 1.0, "mid-flight: {mid}");
        assert!(morphs.tick_at(ms(80)));

        // Reverse mid-flight re-anchors — no jump — then lands at 1.
        let at_flip = morph(&mut morphs, true, ms(80));
        assert!(
            (at_flip - mid).abs() < 1e-4,
            "continuity: {mid} vs {at_flip}"
        );
        assert_eq!(morph(&mut morphs, true, ms(400)), 1.0);
        assert!(!morphs.tick_at(ms(400)));

        // Reduced motion snaps a flip.
        assert_eq!(
            morphs.value_at("glyph", false, GLYPH_STATE, true, ms(500)),
            0.0
        );

        // Unread for a full frame: pruned (a remount snaps again).
        morphs.tick_at(ms(600));
        morphs.tick_at(ms(700));
        assert!(morphs.entries.is_empty());
    }

    #[test]
    fn hover_fade_reduced_motion_snaps() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        fades.set_at("row", true, true, t0);
        assert_eq!(fades.value_at("row", t0), 1.0, "enter snaps to 1");
        fades.set_at("row", false, true, t0);
        assert_eq!(fades.value_at("row", t0), 0.0, "leave snaps to 0");
    }

    #[test]
    fn hover_fade_leave_without_enter_is_inert() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        fades.set_at("ghost", false, false, t0);
        assert!(fades.entries.is_empty(), "no entry for a leave-only key");
        assert_eq!(fades.value_at("ghost", t0), 0.0);
    }

    #[test]
    fn hover_tick_reports_flight_and_prunes() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);

        fades.set_at("a", true, false, t0);
        // Mid-flight: active, frames must keep coming (read each frame).
        assert!(fades.tick_at(ms(50)));
        fades.value_at("a", ms(50));
        assert!(fades.tick_at(ms(100)));
        fades.value_at("a", ms(100));
        // Settled hovered (still read): no more frames needed, entry kept.
        assert!(!fades.tick_at(ms(200)));
        fades.value_at("a", ms(200));
        assert_eq!(fades.value_at("a", ms(250)), 1.0);

        // Leave → fades → settles at rest → entry evicted.
        fades.set_at("a", false, false, ms(250));
        assert!(fades.tick_at(ms(300)));
        fades.value_at("a", ms(300));
        assert!(!fades.tick_at(ms(500)), "settled at rest");
        assert!(fades.entries.is_empty(), "rest entries are pruned");
    }

    #[test]
    fn hover_tick_evicts_unread_entries() {
        // An element that unmounts mid-hover never sends its leave — a full
        // frame without a read drops the entry so a remount starts clean.
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        fades.set_at("menu-row", true, false, t0);
        fades.tick_at(ms(16));
        fades.value_at("menu-row", ms(16)); // frame 1: mounted, read
        fades.tick_at(ms(32)); // frame 2: unmounted — no read
        fades.tick_at(ms(48)); // frame 3: a full unread frame has passed
        assert!(fades.entries.is_empty(), "unread entry evicted");
        assert_eq!(fades.value_at("menu-row", ms(64)), 0.0);
    }

    #[test]
    fn gspin_pulse_shape() {
        // Full at the cycle start, dim through the rest band, rising at the tail.
        assert_close(gspin_opacity(0.0, 0.1), 1.0, 1e-6, "cycle start");
        assert_close(gspin_opacity(0.45, 0.1), 0.1, 1e-6, "fully dim");
        assert_close(gspin_opacity(0.9, 0.1), 0.1, 1e-6, "rest band");
        assert_close(gspin_opacity(1.0, 0.1), 1.0, 1e-6, "wraps to full");
        let mid_fall = gspin_opacity(0.2, 0.1);
        assert!(mid_fall > 0.1 && mid_fall < 1.0, "eases down");
        let mid_rise = gspin_opacity(0.96, 0.1);
        assert!(mid_rise > 0.1 && mid_rise < 1.0, "eases up");
    }

    #[test]
    fn reduce_motion_follows_system_unless_pinned() {
        use ReduceMotion::*;
        assert!(resolve(System, true, false, true));
        assert!(!resolve(System, false, false, true));
        assert!(resolve(On, false, false, true));
        assert!(!resolve(Off, true, false, true), "Off overrides the system");
    }

    #[test]
    fn background_pause_only_applies_when_enabled_and_unfocused() {
        use ReduceMotion::*;
        assert!(resolve(Off, false, true, false));
        assert!(!resolve(Off, false, true, true), "focused window animates");
        assert!(!resolve(Off, false, false, false), "opt-in only");
        assert!(resolve(On, false, false, false));
    }

    #[gpui::test]
    fn focus_and_preference_drive_the_global_flag(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(MotionState {
                preference: ReduceMotion::Off,
                pause_in_background: false,
                system: true,
                active: true,
            });
            apply(cx);
            assert!(!reduced_motion(cx), "Off ignores the system");

            set_window_active(false, cx);
            assert!(!reduced_motion(cx), "pause is opt-in");
            set_pause_in_background(true, cx);
            assert!(reduced_motion(cx), "unfocused window pauses");
            set_window_active(true, cx);
            assert!(!reduced_motion(cx), "refocus resumes");

            set_preference(ReduceMotion::System, cx);
            assert!(reduced_motion(cx), "System follows the recorded OS value");
            set_system(false, cx);
            assert!(!reduced_motion(cx));
        });
    }
}
