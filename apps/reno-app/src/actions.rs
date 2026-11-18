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
        // The run screen's side panel.
        TogglePanel,
        NewPanelTab,
        ClosePanelTab,
        ReopenPanelTab,
        NextPanelTab,
        PrevPanelTab
    ]
);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-alt-b", TogglePanel, None),
        KeyBinding::new("cmd-t", NewPanelTab, None),
        KeyBinding::new("cmd-w", ClosePanelTab, None),
        KeyBinding::new("cmd-shift-t", ReopenPanelTab, None),
        KeyBinding::new("ctrl-tab", NextPanelTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevPanelTab, None),
    ]);
}
