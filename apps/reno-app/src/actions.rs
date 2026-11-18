//! App-wide actions and their key bindings.

use gpui::{App, KeyBinding, actions};

actions!(
    reno,
    [
        OpenPalette,
        ToggleSidebar,
        ToggleTheme,
        GoRuns,
        Quit,
        About,
        OpenSettings,
        CloseWindow,
        Minimize
    ]
);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", Minimize, None),
    ]);
}
