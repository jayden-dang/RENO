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
//! Not ported from the source module, because they need unported crates or the OS accessibility
//! probe: hover colour fades, state morphs, the indicator slide, resize-edge bounce, the
//! user-preference and system reduced-motion resolver, and the Windows high-resolution tick.

// A catalog: entries are used as screens adopt them, so some are unreached today.
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationElement, App, ElementId, EntityId, Global, Hsla, IntoElement, Rgba, Styled,
    px,
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
}
