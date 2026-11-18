//! The Terminal tab's pure parts: what a key, a paste, a pointer or a focus change sends to the
//! shell, where a pointer is in the grid, what colour a cell paints, how input is queued, and
//! the timing rules (resize debounce, reconnect backoff, wheel accumulation, edge scrolling).
//!
//! The algorithms follow the terminal of the project this one grew from (MIT); the colour and
//! theme glue is Reno's. No window, no runtime, no clock: every rule is a unit test.

use std::time::Duration;

use gpui::{Hsla, Modifiers};
use reno_proto::{ErrorCode, RunStatus, TERM_MAX_INPUT_BYTES};
use reno_term::emulator::{CellColor, Side};

use super::panel_home::holds_workspace;
use crate::ui::theme::{Appearance, Theme};

// --- constants -----------------------------------------------------------------------------------

/// The grid's font size and the row height that goes with it.
pub(crate) const FONT_SIZE: f32 = 12.5;
pub(crate) const LINE_HEIGHT: f32 = 18.0;
/// Space between the panel's edge and the grid.
pub(crate) const PADDING: f32 = 10.0;
/// How long a burst of size changes waits before `term.resize` is sent.
pub(crate) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(80);
/// Cursor blink half-period; a keystroke holds the cursor solid for this long.
pub(crate) const BLINK: Duration = Duration::from_millis(500);
/// The blink rests (cursor solid) after this long without typing.
pub(crate) const BLINK_IDLE: Duration = Duration::from_secs(15);
/// How often a selection dragged past the grid's edge scrolls.
pub(crate) const EDGE_SCROLL_TICK: Duration = Duration::from_millis(24);
/// Pointer travel before a press turns into a selection.
pub(crate) const DRAG_THRESHOLD: f32 = 2.0;
/// How long after an attach the replies the emulator returns are not sent: what is replayed was
/// answered when it was live.
pub(crate) const REPLAY_QUIET: Duration = Duration::from_millis(250);
/// The most characters of the shell's own title the tab shows.
const TITLE_CHARS: usize = 24;

// --- palette -------------------------------------------------------------------------------------

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// xterm 256-colour cube component levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The RGB of an xterm colour past the sixteen the theme owns. 16 to 231 is the colour cube,
/// left as the program asked: remapping it would invent colours. 232 to 255 is the grey ramp,
/// which tools use to de-emphasise; on a light canvas it is mirrored so its faint end stays
/// the faint one.
pub(crate) fn extended_rgb(appearance: Appearance, index: u8) -> (u8, u8, u8) {
    match index {
        16..=231 => {
            let n = usize::from(index) - 16;
            (
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            )
        }
        232..=255 => {
            let step = index - 232;
            let step = if appearance.is_dark() { step } else { 23 - step };
            let v = 8 + 10 * step;
            (v, v, v)
        }
        // The sixteen belong to the theme; a caller that asks anyway gets black.
        _ => (0, 0, 0),
    }
}

/// The colour a cell colour paints, against the theme. The terminal's default pair is the
/// canvas and the text colour, so it reads as part of the panel.
pub(crate) fn resolve_color(color: CellColor, theme: &Theme) -> Hsla {
    match color {
        CellColor::Foreground => theme.text,
        CellColor::Background => theme.canvas,
        CellColor::Indexed(ix @ 0..=15) => theme.ansi[usize::from(ix)],
        CellColor::Indexed(ix) => {
            let (r, g, b) = extended_rgb(theme.appearance, ix);
            rgb8(r, g, b)
        }
        CellColor::Rgb(r, g, b) => rgb8(r, g, b),
    }
}

/// Contrast floor for text the program coloured itself (WCAG ratio).
pub(crate) const MIN_CONTRAST: f32 = 3.0;

fn relative_luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let lin = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb.r) + 0.7152 * lin(rgb.g) + 0.0722 * lin(rgb.b)
}

/// The WCAG contrast ratio of two colours, 1 to 21.
pub(crate) fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `fg`, moved in lightness just far enough from `bg` to reach `min`, keeping its hue. A
/// foreground equal to its background is a deliberate hidden block and is left alone.
pub(crate) fn ensure_contrast(fg: Hsla, bg: Hsla, min: f32) -> Hsla {
    if fg == bg || contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let toward_light = relative_luminance(bg) < 0.18;
    let (mut lo, mut hi) = if toward_light {
        (fg.l, 1.0)
    } else {
        (0.0, fg.l)
    };
    let mut best = Hsla {
        l: if toward_light { 1.0 } else { 0.0 },
        ..fg
    };
    for _ in 0..16 {
        let mid = (lo + hi) / 2.0;
        let candidate = Hsla { l: mid, ..fg };
        if contrast_ratio(candidate, bg) >= min {
            best = candidate;
            if toward_light {
                hi = mid;
            } else {
                lo = mid;
            }
        } else if toward_light {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
}

// --- pointer -> cell -----------------------------------------------------------------------------

/// Which cell a pointer is over, and which edge of it a selection anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellHit {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) side: Side,
}

/// Maps a position relative to the first glyph's top-left onto a cell. A position outside the
/// grid clamps to the nearest cell (a drag leaves the panel), and overshoot forces the side:
/// past the bottom takes the last line whole, past the top starts the first.
pub(crate) fn cell_at(
    x: f32,
    y: f32,
    cell_w: f32,
    line_h: f32,
    cols: usize,
    rows: usize,
) -> CellHit {
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if cols == 0 || rows == 0 || !usable(cell_w) || !usable(line_h) {
        return CellHit {
            row: 0,
            col: 0,
            side: Side::Left,
        };
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    let (last_col, last_row) = (cols - 1, rows - 1);
    let raw_col = (x / cell_w).floor();
    let mut side = if x.max(0.0) % cell_w > cell_w / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let col = if raw_col > last_col as f32 {
        side = Side::Right;
        last_col
    } else {
        raw_col.max(0.0) as usize
    };
    let raw_row = (y / line_h).floor();
    let row = if raw_row > last_row as f32 {
        side = Side::Right;
        last_row
    } else if raw_row < 0.0 {
        side = Side::Left;
        0
    } else {
        raw_row as usize
    };
    CellHit { row, col, side }
}

/// The grid a panel body of `width` by `height` px holds, or `None` when it is too small to
/// be a terminal (a panel mid-animation, or hidden): such a size is never sent to the shell.
pub(crate) fn grid_for(
    width: f32,
    height: f32,
    cell_w: f32,
    line_h: f32,
    padding: f32,
) -> Option<(u16, u16)> {
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if !usable(cell_w) || !usable(line_h) || !usable(width) || !usable(height) {
        return None;
    }
    let cols = ((width - 2.0 * padding) / cell_w).floor();
    let rows = ((height - 2.0 * padding) / line_h).floor();
    if cols < 2.0 || rows < 1.0 {
        return None;
    }
    Some((cols.min(500.0) as u16, rows.min(500.0) as u16))
}

// --- keyboard -> bytes ---------------------------------------------------------------------------

/// Encodes a keystroke as the bytes a shell expects. `None` means the key is not the
/// terminal's. `app_cursor` switches arrows, Home and End from CSI to SS3 (DECCKM).
pub(crate) fn keystroke_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: &Modifiers,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Cmd belongs to the app's keymap, never the shell.
    if mods.platform {
        return None;
    }
    if mods.alt {
        let inner = keystroke_bytes(
            key,
            key_char,
            &Modifiers {
                alt: false,
                ..*mods
            },
            app_cursor,
        )?;
        let mut out = vec![0x1b];
        out.extend(inner);
        return Some(out);
    }
    if mods.control {
        return control_bytes(key);
    }
    let seq = |csi: &[u8], ss3: &[u8]| Some(if app_cursor { ss3 } else { csi }.to_vec());
    match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "escape" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => seq(b"\x1b[A", b"\x1bOA"),
        "down" => seq(b"\x1b[B", b"\x1bOB"),
        "right" => seq(b"\x1b[C", b"\x1bOC"),
        "left" => seq(b"\x1b[D", b"\x1bOD"),
        "home" => seq(b"\x1b[H", b"\x1bOH"),
        "end" => seq(b"\x1b[F", b"\x1bOF"),
        "insert" => Some(b"\x1b[2~".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "f1" => Some(b"\x1bOP".to_vec()),
        "f2" => Some(b"\x1bOQ".to_vec()),
        "f3" => Some(b"\x1bOR".to_vec()),
        "f4" => Some(b"\x1bOS".to_vec()),
        "f5" => Some(b"\x1b[15~".to_vec()),
        "f6" => Some(b"\x1b[17~".to_vec()),
        "f7" => Some(b"\x1b[18~".to_vec()),
        "f8" => Some(b"\x1b[19~".to_vec()),
        "f9" => Some(b"\x1b[20~".to_vec()),
        "f10" => Some(b"\x1b[21~".to_vec()),
        "f11" => Some(b"\x1b[23~".to_vec()),
        "f12" => Some(b"\x1b[24~".to_vec()),
        _ => {
            let text = key_char
                .filter(|c| !c.is_empty())
                .or((key.chars().count() == 1).then_some(key))?;
            Some(text.as_bytes().to_vec())
        }
    }
}

/// How a keydown reaches the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    /// Write these bytes now.
    Bytes(Vec<u8>),
    /// Typed text: the platform's text input delivers it, so input methods, dead keys and
    /// layouts compose it first.
    Text,
    /// Not the terminal's (the app's shortcuts).
    NotOurs,
}

/// Routes a keydown. Printable text goes through the platform text input; named and modified
/// keys are encoded here.
///
/// Option follows the layout: where it types an ASCII symbol (`@`, `|` on many European
/// layouts) that symbol is typed; where it would type something else (`∫` for Option-B on a US
/// layout) it acts as Meta, so word motions work without a setting.
pub(crate) fn route_key(
    key: &str,
    key_char: Option<&str>,
    mods: &Modifiers,
    app_cursor: bool,
) -> KeyRoute {
    if mods.platform {
        return KeyRoute::NotOurs;
    }
    let printable = key_char.filter(|c| !c.is_empty() && !c.chars().any(char::is_control));
    if mods.alt && !mods.control {
        if let Some(text) = printable
            && text.is_ascii()
            && !text.eq_ignore_ascii_case(key)
            && key.chars().count() == 1
        {
            return KeyRoute::Text;
        }
        let base = (key.chars().count() == 1).then(|| {
            if mods.shift {
                key.to_uppercase()
            } else {
                key.to_owned()
            }
        });
        return keystroke_bytes(key, base.as_deref(), mods, app_cursor)
            .map_or(KeyRoute::NotOurs, KeyRoute::Bytes);
    }
    if !mods.control && !mods.function && printable.is_some() && !is_named_key(key) {
        return KeyRoute::Text;
    }
    keystroke_bytes(key, key_char, mods, app_cursor).map_or(KeyRoute::NotOurs, KeyRoute::Bytes)
}

fn is_named_key(key: &str) -> bool {
    matches!(
        key,
        "enter"
            | "backspace"
            | "tab"
            | "escape"
            | "space"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "insert"
            | "delete"
            | "pageup"
            | "pagedown"
    ) || (key.len() >= 2 && key.starts_with('f') && key[1..].chars().all(|c| c.is_ascii_digit()))
}

/// Ctrl-key encoding (caret notation).
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let (c, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return match key {
            "space" => Some(vec![0x00]),
            "backspace" => Some(vec![0x08]),
            "enter" => Some(b"\r".to_vec()),
            _ => None,
        };
    }
    match c {
        'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
        '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' | '/' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

/// Wraps pasted text for the shell. Bracketed mode frames it, and strips the one sequence a
/// paste could use to break out of the frame; otherwise line ends become carriage returns, as
/// typing Enter would send.
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let sanitized = text.replace("\x1b[201~", "");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

// --- pointer and focus reports -------------------------------------------------------------------

pub(crate) const MOUSE_LEFT: u8 = 0;
pub(crate) const MOUSE_MIDDLE: u8 = 1;
pub(crate) const MOUSE_RIGHT: u8 = 2;
/// The legacy encoding's "some button released", and motion with nothing held.
pub(crate) const MOUSE_NONE: u8 = 3;
pub(crate) const MOUSE_WHEEL_UP: u8 = 64;
pub(crate) const MOUSE_WHEEL_DOWN: u8 = 65;
/// Added to a button code for motion reports.
pub(crate) const MOUSE_MOTION: u8 = 32;

/// A pointer event for a program that asked for mouse reports; `col` and `row` are 0-based.
/// SGR (1006) names the released button; the legacy encoding cannot, and cannot address
/// cells past 222, so such an event is dropped rather than sent garbled.
pub(crate) fn mouse_report(
    button: u8,
    col: usize,
    row: usize,
    release: bool,
    mods: &Modifiers,
    sgr: bool,
) -> Option<Vec<u8>> {
    let mut code = button;
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.control {
        code += 16;
    }
    if sgr {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    if release {
        code = (code & !0b11) | MOUSE_NONE;
    }
    let encode = |v: usize| u8::try_from(v + 1 + 32).ok().filter(|b| *b < 255);
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        encode(col)?,
        encode(row)?,
    ])
}

/// A focus change report (1004).
pub(crate) fn focus_report(focused: bool) -> &'static [u8] {
    if focused { b"\x1b[I" } else { b"\x1b[O" }
}

/// Wheel rows as arrow keys for a full-screen program that does not track the mouse
/// (alternate scroll, 1007). Positive rows scroll up.
pub(crate) fn wheel_arrows(rows: i32, app_cursor: bool) -> Vec<u8> {
    let arrow: &[u8] = match (rows > 0, app_cursor) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    arrow.repeat(rows.unsigned_abs() as usize)
}

// --- scrolling -----------------------------------------------------------------------------------

/// Where a wheel movement of `rows` (positive is up) leaves the scrollback: the whole line
/// offset and the fraction of a line left over. Clamped flush to the live bottom and to the
/// history's top, where no fraction is kept.
pub(crate) fn scroll_target(
    display_offset: usize,
    remainder: f32,
    rows: f32,
    history: usize,
) -> (usize, f32) {
    let target = (display_offset as f32 + remainder + rows).clamp(0.0, history as f32);
    let whole = target.floor();
    (whole as usize, target - whole)
}

/// The whole rows a program is owed from `owed` fractional ones, and what stays owed.
pub(crate) fn whole_steps(owed: f32) -> (i32, f32) {
    let step = owed.trunc() as i32;
    (step, owed - step as f32)
}

/// A wheel delta in pixels as rows.
pub(crate) fn rows_of_pixels(dy: f32, line_h: f32) -> f32 {
    if line_h.is_finite() && line_h > 0.0 {
        dy / line_h
    } else {
        0.0
    }
}

/// How many lines a selection dragged to `y` scrolls, given where the grid is: positive goes
/// up into history, negative toward the live bottom, zero in the middle. Speed grows as the
/// pointer goes deeper into the edge band.
pub(crate) fn selection_scroll_lines(top: f32, line_h: f32, rows: usize, y: f32) -> i32 {
    let grid_height = line_h * rows as f32;
    if !(grid_height.is_finite() && grid_height > 0.0) {
        return 0;
    }
    let edge = line_h.min(grid_height / 3.0);
    let bottom = top + grid_height;
    let speed = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        (1.0 + 2.0 * t * t).round() as i32
    };
    if y < top + edge {
        speed(top + edge - y)
    } else if y > bottom - edge {
        -speed(y - (bottom - edge))
    } else {
        0
    }
}

// --- input queue ---------------------------------------------------------------------------------

/// Keyboard bytes on their way to the shell. A key is written the moment it lands; while that
/// write is in flight, later keys gather here and ride the next one. One write at a time keeps
/// keys in typed order, and nothing waits on a timer. A write carries at most what the daemon
/// takes in one call.
#[derive(Debug, Default)]
pub(crate) struct InputQueue {
    pending: Vec<u8>,
    in_flight: bool,
}

impl InputQueue {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next write, when one may start now. The caller must [`Self::finish`] it.
    pub(crate) fn start(&mut self) -> Option<Vec<u8>> {
        if self.in_flight || self.pending.is_empty() {
            return None;
        }
        self.in_flight = true;
        let n = self.pending.len().min(TERM_MAX_INPUT_BYTES);
        Some(self.pending.drain(..n).collect())
    }

    /// The write that was in flight is over; the next may start.
    pub(crate) fn finish(&mut self) {
        self.in_flight = false;
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> &[u8] {
        &self.pending
    }
}

// --- timing --------------------------------------------------------------------------------------

/// The wait before reconnecting after `attempt` failures in a row: 500 ms, doubling to 8 s.
pub(crate) fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << attempt.min(4)).min(8_000))
}

/// Whether the replies the emulator returns for output are sent: not while the output being
/// fed is a replay of what a viewer that came back missed, which was answered when it was live.
pub(crate) fn replies_allowed(since_attach: Duration, resuming: bool) -> bool {
    !resuming || since_attach >= REPLAY_QUIET
}

// --- the tab's words -----------------------------------------------------------------------------

/// The tab's label: "Terminal", with the title a program set (OSC 0/2) after it.
pub(crate) fn tab_label(title: Option<&str>) -> String {
    let title: String = title
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let title = title.trim();
    if title.is_empty() {
        return "Terminal".to_owned();
    }
    let mut shown: String = title.chars().take(TITLE_CHARS).collect();
    if title.chars().count() > TITLE_CHARS {
        shown.push('\u{2026}');
    }
    format!("Terminal \u{b7} {shown}")
}

/// The dimmed trailer a shell's end leaves in the grid.
pub(crate) fn exit_message(code: Option<i32>, signal: Option<&str>) -> Vec<u8> {
    let what = match (code, signal) {
        (_, Some(signal)) => format!("process ended by {signal}"),
        (Some(code), None) => format!("process exited {code}"),
        (None, None) => "process ended".to_owned(),
    };
    format!("\r\n\x1b[90m[{what}]\x1b[0m\r\n").into_bytes()
}

/// What the person is told when the daemon refuses to open a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenFail {
    /// The sandbox for a live run could not start on this Mac.
    SandboxUnavailable,
    /// The run's worktree is gone.
    WorkspaceRemoved,
    /// The daemon holds as many terminals as it allows.
    Limit,
    Other(String),
}

impl OpenFail {
    pub(crate) fn of(code: Option<ErrorCode>, message: &str) -> Self {
        match code {
            Some(ErrorCode::SandboxUnavailable) => Self::SandboxUnavailable,
            Some(ErrorCode::WorkspaceUnavailable) => Self::WorkspaceRemoved,
            Some(ErrorCode::TerminalLimit) => Self::Limit,
            _ => Self::Other(message.to_owned()),
        }
    }

    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::SandboxUnavailable => "No terminal on a live run",
            Self::WorkspaceRemoved => "Workspace removed",
            Self::Limit => "Too many terminals",
            Self::Other(_) => "Could not open a terminal",
        }
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::SandboxUnavailable => "A terminal cannot be opened on a live run on this Mac: \
                the sandbox that keeps protected files read-only could not start. Open it \
                after the run ends."
                .to_owned(),
            Self::WorkspaceRemoved => "The workspace was removed, so there is nowhere to open a \
                shell."
                .to_owned(),
            Self::Limit => "The daemon holds as many terminals as it allows. Close one that has \
                ended to make room."
                .to_owned(),
            Self::Other(message) => message.clone(),
        }
    }
}

/// What a terminal is, as far as its banner is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BannerFacts<'a> {
    pub(crate) sandboxed: bool,
    pub(crate) in_place: bool,
    pub(crate) cwd: &'a str,
    pub(crate) run: RunStatus,
}

/// The one line above a terminal (D7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Banner {
    /// The run is live and this shell cannot write the protected files.
    Protected,
    /// The run ended; this shell still has the limits and a new one would not.
    Limited,
    /// The shell is in the person's own folder.
    InPlace(String),
}

impl Banner {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Protected => "Protected files (.claude, .codex, .reno, .mcp.json, git) are \
                read-only here while the run is live"
                .to_owned(),
            Self::Limited => "Protected files (.claude, .codex, .reno, .mcp.json, git) are \
                read-only in this shell, which started while the run was live"
                .to_owned(),
            Self::InPlace(path) => format!("in {path}, not a worktree"),
        }
    }

    /// Whether the banner offers a shell without the limits.
    pub(crate) fn offers_reopen(&self) -> bool {
        matches!(self, Self::Limited)
    }
}

pub(crate) fn banner(facts: &BannerFacts<'_>) -> Option<Banner> {
    if facts.in_place {
        return Some(Banner::InPlace(facts.cwd.to_owned()));
    }
    match (facts.sandboxed, holds_workspace(facts.run)) {
        (true, true) => Some(Banner::Protected),
        (true, false) => Some(Banner::Limited),
        (false, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Palette;

    fn mods() -> Modifiers {
        Modifiers::default()
    }

    fn with(f: impl FnOnce(&mut Modifiers)) -> Modifiers {
        let mut m = mods();
        f(&mut m);
        m
    }

    fn bytes(key: &str, key_char: Option<&str>, m: &Modifiers, app: bool) -> Option<Vec<u8>> {
        keystroke_bytes(key, key_char, m, app)
    }

    #[test]
    fn printables_prefer_the_typed_character() {
        assert_eq!(bytes("a", Some("a"), &mods(), false), Some(b"a".to_vec()));
        let shift = with(|m| m.shift = true);
        assert_eq!(bytes("a", Some("A"), &shift, false), Some(b"A".to_vec()));
        assert_eq!(
            bytes("e", Some("\u{e9}"), &mods(), false),
            Some("\u{e9}".as_bytes().to_vec())
        );
        assert_eq!(bytes("/", None, &mods(), false), Some(b"/".to_vec()));
        assert_eq!(bytes("capslock", None, &mods(), false), None);
    }

    #[test]
    fn named_keys() {
        for (key, want) in [
            ("enter", &b"\r"[..]),
            ("backspace", &[0x7f][..]),
            ("tab", b"\t"),
            ("escape", &[0x1b][..]),
            ("space", b" "),
            ("delete", b"\x1b[3~"),
            ("insert", b"\x1b[2~"),
            ("pageup", b"\x1b[5~"),
            ("pagedown", b"\x1b[6~"),
            ("f1", b"\x1bOP"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f12", b"\x1b[24~"),
        ] {
            assert_eq!(bytes(key, None, &mods(), false).as_deref(), Some(want));
        }
        assert_eq!(
            bytes("tab", None, &with(|m| m.shift = true), false),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        assert_eq!(bytes("up", None, &mods(), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(bytes("up", None, &mods(), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            bytes("home", None, &mods(), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(bytes("end", None, &mods(), true), Some(b"\x1bOF".to_vec()));
        assert_eq!(
            bytes("left", None, &mods(), false),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            bytes("right", None, &mods(), true),
            Some(b"\x1bOC".to_vec())
        );
    }

    #[test]
    fn ctrl_combinations_are_control_bytes() {
        let ctrl = with(|m| m.control = true);
        assert_eq!(bytes("c", Some("c"), &ctrl, false), Some(vec![0x03]));
        assert_eq!(bytes("z", None, &ctrl, false), Some(vec![0x1a]));
        assert_eq!(bytes("d", None, &ctrl, false), Some(vec![0x04]));
        assert_eq!(bytes("space", None, &ctrl, false), Some(vec![0x00]));
        assert_eq!(bytes("[", None, &ctrl, false), Some(vec![0x1b]));
        assert_eq!(bytes("_", None, &ctrl, false), Some(vec![0x1f]));
        assert_eq!(bytes("1", Some("1"), &ctrl, false), None, "no caret form");
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = with(|m| m.alt = true);
        assert_eq!(bytes("b", Some("b"), &alt, false), Some(vec![0x1b, b'b']));
        let both = with(|m| {
            m.alt = true;
            m.control = true;
        });
        assert_eq!(bytes("c", None, &both, false), Some(vec![0x1b, 0x03]));
    }

    #[test]
    fn command_combinations_are_the_apps() {
        let cmd = with(|m| m.platform = true);
        assert_eq!(bytes("j", Some("j"), &cmd, false), None);
        assert_eq!(bytes("enter", None, &cmd, false), None);
        assert_eq!(route_key("v", Some("v"), &cmd, false), KeyRoute::NotOurs);
    }

    #[test]
    fn typed_text_goes_through_text_input_and_named_keys_are_encoded() {
        let shift = with(|m| m.shift = true);
        assert_eq!(route_key("a", Some("a"), &mods(), false), KeyRoute::Text);
        assert_eq!(route_key("a", Some("A"), &shift, false), KeyRoute::Text);
        assert_eq!(
            route_key("enter", Some("\r"), &mods(), false),
            KeyRoute::Bytes(b"\r".to_vec())
        );
        assert_eq!(
            route_key("up", None, &mods(), true),
            KeyRoute::Bytes(b"\x1bOA".to_vec())
        );
        assert_eq!(
            route_key("c", Some("c"), &with(|m| m.control = true), false),
            KeyRoute::Bytes(vec![0x03])
        );
    }

    #[test]
    fn option_types_layout_symbols_and_otherwise_acts_as_meta() {
        let alt = with(|m| m.alt = true);
        assert_eq!(
            route_key("b", Some("\u{222b}"), &alt, false),
            KeyRoute::Bytes(vec![0x1b, b'b'])
        );
        let alt_shift = with(|m| {
            m.alt = true;
            m.shift = true;
        });
        assert_eq!(
            route_key("b", Some("\u{131}"), &alt_shift, false),
            KeyRoute::Bytes(vec![0x1b, b'B'])
        );
        assert_eq!(route_key("l", Some("@"), &alt, false), KeyRoute::Text);
        assert_eq!(
            route_key("left", None, &alt, false),
            KeyRoute::Bytes(b"\x1b\x1b[D".to_vec())
        );
    }

    #[test]
    fn paste_is_bracketed_when_the_program_asked_and_cannot_break_out() {
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf", true),
            b"\x1b[200~arm -rf\x1b[201~".to_vec()
        );
        // Unbracketed, a line end is Enter. Bracketed, the text is left as it is.
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            paste_bytes("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn mouse_reports_in_both_encodings() {
        let none = mods();
        assert_eq!(
            mouse_report(MOUSE_LEFT, 4, 1, false, &none, true).as_deref(),
            Some(&b"\x1b[<0;5;2M"[..])
        );
        assert_eq!(
            mouse_report(MOUSE_LEFT, 4, 1, true, &none, true).as_deref(),
            Some(&b"\x1b[<0;5;2m"[..]),
            "SGR names the released button"
        );
        let ctrl = with(|m| m.control = true);
        assert_eq!(
            mouse_report(MOUSE_WHEEL_UP, 0, 0, false, &ctrl, true).as_deref(),
            Some(&b"\x1b[<80;1;1M"[..])
        );
        assert_eq!(
            mouse_report(MOUSE_RIGHT, 0, 0, false, &none, false),
            Some(vec![0x1b, b'[', b'M', 32 + 2, 33, 33])
        );
        assert_eq!(
            mouse_report(MOUSE_RIGHT, 0, 0, true, &none, false),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33]),
            "a legacy release is button 3"
        );
        assert_eq!(
            mouse_report(MOUSE_LEFT, 300, 0, false, &none, false),
            None,
            "the legacy encoding cannot address column 301"
        );
        assert_eq!(
            mouse_report(MOUSE_LEFT, 0, 0, false, &with(|m| m.alt = true), true).as_deref(),
            Some(&b"\x1b[<8;1;1M"[..])
        );
    }

    #[test]
    fn focus_reports_and_wheel_arrows() {
        assert_eq!(focus_report(true), b"\x1b[I");
        assert_eq!(focus_report(false), b"\x1b[O");
        assert_eq!(wheel_arrows(2, false), b"\x1b[A\x1b[A");
        assert_eq!(wheel_arrows(-1, true), b"\x1bOB");
        assert_eq!(wheel_arrows(0, true), b"");
    }

    fn hit(x: f32, y: f32) -> CellHit {
        cell_at(x, y, 8.0, 16.0, 80, 24)
    }

    #[test]
    fn a_pointer_maps_to_the_cell_it_is_over() {
        assert_eq!((hit(0.0, 0.0).row, hit(0.0, 0.0).col), (0, 0));
        assert_eq!((hit(7.9, 15.9).row, hit(7.9, 15.9).col), (0, 0));
        assert_eq!((hit(8.0, 16.0).row, hit(8.0, 16.0).col), (1, 1));
        assert_eq!((hit(100.0, 100.0).row, hit(100.0, 100.0).col), (6, 12));
    }

    #[test]
    fn the_side_splits_a_cell_at_its_middle() {
        assert_eq!(hit(3.0, 4.0).side, Side::Left);
        assert_eq!(hit(5.0, 4.0).side, Side::Right);
    }

    #[test]
    fn overshoot_clamps_and_forces_the_side() {
        let low = hit(1000.0, 1000.0);
        assert_eq!((low.row, low.col, low.side), (23, 79, Side::Right));
        let high = hit(-5.0, -5.0);
        assert_eq!((high.row, high.col, high.side), (0, 0, Side::Left));
        // Past the bottom takes the line whole even with the pointer left of the start.
        assert_eq!(hit(1.0, 1000.0).side, Side::Right);
    }

    #[test]
    fn degenerate_metrics_do_not_panic() {
        for (w, h, cols, rows) in [
            (0.0, 16.0, 80, 24),
            (8.0, 0.0, 80, 24),
            (f32::NAN, 16.0, 80, 24),
            (8.0, f32::INFINITY, 80, 24),
            (8.0, 16.0, 0, 24),
            (8.0, 16.0, 80, 0),
        ] {
            let h = cell_at(10.0, 10.0, w, h, cols, rows);
            assert_eq!((h.row, h.col), (0, 0));
        }
        let nan = cell_at(f32::NAN, f32::NAN, 8.0, 16.0, 80, 24);
        assert_eq!((nan.row, nan.col), (0, 0));
    }

    #[test]
    fn the_grid_follows_the_panel_and_a_degenerate_panel_has_none() {
        assert_eq!(grid_for(660.0, 440.0, 8.0, 16.0, 10.0), Some((80, 26)));
        assert_eq!(grid_for(0.0, 440.0, 8.0, 16.0, 10.0), None);
        assert_eq!(grid_for(660.0, 20.0, 8.0, 16.0, 10.0), None);
        assert_eq!(grid_for(30.0, 440.0, 8.0, 16.0, 10.0), None, "under two columns");
        assert_eq!(grid_for(660.0, 440.0, f32::NAN, 16.0, 10.0), None);
        assert_eq!(
            grid_for(100_000.0, 100_000.0, 8.0, 16.0, 10.0),
            Some((500, 500)),
            "capped"
        );
    }

    fn hsl(r: u8, g: u8, b: u8) -> Hsla {
        rgb8(r, g, b)
    }

    #[test]
    fn low_contrast_colours_are_lifted_and_good_ones_kept() {
        let black = hsl(0, 0, 0);
        let navy = hsl(0, 0, 128);
        assert!(contrast_ratio(navy, black) < MIN_CONTRAST);
        let lifted = ensure_contrast(navy, black, MIN_CONTRAST);
        assert!(contrast_ratio(lifted, black) >= MIN_CONTRAST);
        assert!((lifted.h - navy.h).abs() < 1e-3, "the hue is kept");
        let white = hsl(255, 255, 255);
        assert_eq!(ensure_contrast(white, black, MIN_CONTRAST), white);
        // A colour on itself is a deliberate hidden block.
        assert_eq!(ensure_contrast(navy, navy, MIN_CONTRAST), navy);
        // A light background darkens the text instead.
        let pale = hsl(250, 250, 160);
        let darker = ensure_contrast(pale, white, MIN_CONTRAST);
        assert!(darker.l < pale.l && contrast_ratio(darker, white) >= MIN_CONTRAST);
    }

    #[test]
    fn contrast_ratio_spans_one_to_twenty_one() {
        let (b, w) = (hsl(0, 0, 0), hsl(255, 255, 255));
        assert!((contrast_ratio(b, w) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(w, w) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn colours_resolve_from_the_theme_the_cube_and_the_ramp() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            assert_eq!(resolve_color(CellColor::Foreground, &theme), theme.text);
            assert_eq!(resolve_color(CellColor::Background, &theme), theme.canvas);
            for ix in 0..16u8 {
                assert_eq!(
                    resolve_color(CellColor::Indexed(ix), &theme),
                    theme.ansi[usize::from(ix)]
                );
            }
            assert_eq!(resolve_color(CellColor::Indexed(1), &theme), theme.danger);
            assert_eq!(
                resolve_color(CellColor::Rgb(10, 20, 30), &theme),
                hsl(10, 20, 30)
            );
        }
        // The cube is the same in both: index 196 is #ff0000 by arithmetic.
        let dark = Theme::for_appearance(Appearance::Dark);
        let light = Theme::for_appearance(Appearance::Light);
        for theme in [&dark, &light] {
            assert_eq!(
                resolve_color(CellColor::Indexed(196), theme),
                hsl(255, 0, 0)
            );
            assert_eq!(resolve_color(CellColor::Indexed(16), theme), hsl(0, 0, 0));
            assert_eq!(
                resolve_color(CellColor::Indexed(231), theme),
                hsl(255, 255, 255)
            );
        }
    }

    #[test]
    fn the_grey_ramp_mirrors_on_a_light_canvas() {
        assert_eq!(extended_rgb(Appearance::Dark, 232), (8, 8, 8));
        assert_eq!(extended_rgb(Appearance::Dark, 255), (238, 238, 238));
        assert_eq!(extended_rgb(Appearance::Light, 232), (238, 238, 238));
        assert_eq!(extended_rgb(Appearance::Light, 255), (8, 8, 8));
    }

    #[test]
    fn every_ansi_slot_of_both_palettes_resolves() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let p = Palette::for_appearance(appearance);
            assert_eq!(p.ansi.len(), 16);
            let mut seen = p.ansi.to_vec();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), 16, "no two slots share a colour");
        }
    }

    #[test]
    fn the_input_queue_keeps_one_write_in_flight_and_order() {
        let mut q = InputQueue::default();
        assert_eq!(q.start(), None, "nothing pending");
        q.push(b"a");
        assert_eq!(q.start(), Some(b"a".to_vec()), "the first key goes at once");
        q.push(b"b");
        q.push(b"c");
        assert_eq!(q.start(), None, "keys typed mid-write wait for it");
        assert_eq!(q.pending(), b"bc");
        q.finish();
        assert_eq!(q.start(), Some(b"bc".to_vec()), "and ride the next write");
        q.finish();
        assert_eq!(q.start(), None);
    }

    #[test]
    fn a_write_never_exceeds_what_the_daemon_takes() {
        let mut q = InputQueue::default();
        let big = vec![b'x'; TERM_MAX_INPUT_BYTES * 2 + 5];
        q.push(&big);
        let first = q.start().expect("a write");
        assert_eq!(first.len(), TERM_MAX_INPUT_BYTES);
        q.finish();
        assert_eq!(q.start().expect("a write").len(), TERM_MAX_INPUT_BYTES);
        q.finish();
        assert_eq!(q.start().expect("a write").len(), 5);
    }

    #[test]
    fn backoff_doubles_to_a_ceiling() {
        let ms: Vec<u128> = (0..8).map(|n| backoff(n).as_millis()).collect();
        assert_eq!(ms, [500, 1000, 2000, 4000, 8000, 8000, 8000, 8000]);
        assert_eq!(backoff(u32::MAX).as_millis(), 8000);
    }

    #[test]
    fn timing_constants_are_the_specified_ones() {
        assert_eq!(RESIZE_DEBOUNCE.as_millis(), 80);
        assert_eq!(BLINK.as_millis(), 500);
        assert_eq!(BLINK_IDLE.as_secs(), 15);
    }

    #[test]
    fn slow_wheel_movement_accumulates_and_rests_flush() {
        // Quarter-row events: nothing moves for three, then a row.
        let (mut offset, mut rem) = (0usize, 0.0f32);
        let mut moved = Vec::new();
        for _ in 0..8 {
            (offset, rem) = scroll_target(offset, rem, 0.25, 100);
            moved.push(offset);
        }
        assert_eq!(moved, [0, 0, 0, 1, 1, 1, 1, 2]);
        // Back down past the live bottom rests flush with it.
        for _ in 0..30 {
            (offset, rem) = scroll_target(offset, rem, -0.7, 100);
        }
        assert_eq!((offset, rem), (0, 0.0));
        // And clamps at the history's top with no fraction.
        assert_eq!(scroll_target(98, 0.5, 9.0, 100), (100, 0.0));
    }

    #[test]
    fn rows_owed_to_a_program_are_whole() {
        assert_eq!(whole_steps(0.75), (0, 0.75));
        assert_eq!(whole_steps(2.5), (2, 0.5));
        assert_eq!(whole_steps(-1.25), (-1, -0.25));
        assert_eq!(rows_of_pixels(36.0, 18.0), 2.0);
        assert_eq!(rows_of_pixels(36.0, 0.0), 0.0);
    }

    #[test]
    fn a_selection_scrolls_toward_the_edge_it_is_past() {
        // The grid is 24 rows of 16 px from y = 100.
        let lines = |y| selection_scroll_lines(100.0, 16.0, 24, y);
        assert_eq!(lines(300.0), 0, "the middle");
        assert!(lines(90.0) > 0, "above: into history");
        assert!(lines(100.0 + 24.0 * 16.0 + 30.0) < 0, "below: toward live");
        assert!(lines(60.0) >= lines(99.0), "deeper is faster");
        assert_eq!(selection_scroll_lines(0.0, 0.0, 24, 5.0), 0);
        assert_eq!(selection_scroll_lines(0.0, 16.0, 0, 5.0), 0);
    }

    #[test]
    fn replayed_output_is_not_answered_but_live_output_is() {
        let quiet = REPLAY_QUIET;
        assert!(!replies_allowed(Duration::ZERO, true));
        assert!(!replies_allowed(quiet - Duration::from_millis(1), true));
        assert!(replies_allowed(quiet, true));
        // A terminal this app just opened has nothing to replay: its first query is live.
        assert!(replies_allowed(Duration::ZERO, false));
    }

    #[test]
    fn the_tab_says_terminal_and_what_the_program_titled_it() {
        assert_eq!(tab_label(None), "Terminal");
        assert_eq!(tab_label(Some("")), "Terminal");
        assert_eq!(tab_label(Some("  \u{7} ")), "Terminal");
        assert_eq!(tab_label(Some("vim notes.md")), "Terminal \u{b7} vim notes.md");
        let long = "x".repeat(40);
        let label = tab_label(Some(&long));
        assert!(label.ends_with('\u{2026}'));
        assert_eq!(label.chars().count(), "Terminal \u{b7} ".chars().count() + 25);
        assert_eq!(tab_label(Some("a\u{1b}b")), "Terminal \u{b7} ab", "controls dropped");
    }

    #[test]
    fn a_shell_that_ended_leaves_a_dim_trailer() {
        assert_eq!(
            exit_message(Some(0), None),
            b"\r\n\x1b[90m[process exited 0]\x1b[0m\r\n".to_vec()
        );
        assert!(String::from_utf8(exit_message(None, Some("SIGKILL")))
            .unwrap_or_default()
            .contains("SIGKILL"));
        assert!(String::from_utf8(exit_message(None, None))
            .unwrap_or_default()
            .contains("ended"));
    }

    #[test]
    fn open_errors_become_their_words() {
        assert_eq!(
            OpenFail::of(Some(ErrorCode::SandboxUnavailable), "x"),
            OpenFail::SandboxUnavailable
        );
        assert!(
            OpenFail::SandboxUnavailable
                .text()
                .contains("cannot be opened on a live run on this Mac")
        );
        assert_eq!(
            OpenFail::of(Some(ErrorCode::WorkspaceUnavailable), "x"),
            OpenFail::WorkspaceRemoved
        );
        assert_eq!(OpenFail::WorkspaceRemoved.title(), "Workspace removed");
        assert_eq!(
            OpenFail::of(Some(ErrorCode::TerminalLimit), "x"),
            OpenFail::Limit
        );
        assert_eq!(
            OpenFail::of(Some(ErrorCode::NotFound), "no such run"),
            OpenFail::Other("no such run".into())
        );
        assert_eq!(OpenFail::of(None, "offline"), OpenFail::Other("offline".into()));
    }

    #[test]
    fn the_banner_follows_the_run_and_the_sandbox() {
        let facts = |sandboxed, in_place, run| BannerFacts {
            sandboxed,
            in_place,
            cwd: "/work/app",
            run,
        };
        let live = banner(&facts(true, false, RunStatus::Running)).expect("a banner");
        assert_eq!(live, Banner::Protected);
        assert!(live.text().starts_with("Protected files (.claude, .codex, .reno, .mcp.json, git) are read-only here while the run is live"));
        assert!(!live.offers_reopen());
        let ended = banner(&facts(true, false, RunStatus::Succeeded)).expect("a banner");
        assert_eq!(ended, Banner::Limited);
        assert!(ended.offers_reopen());
        assert_eq!(banner(&facts(false, false, RunStatus::Running)), None);
        assert_eq!(banner(&facts(false, false, RunStatus::Succeeded)), None);
        let place = banner(&facts(false, true, RunStatus::Running)).expect("a banner");
        assert_eq!(place.text(), "in /work/app, not a worktree");
        assert!(!place.offers_reopen());
    }
}
