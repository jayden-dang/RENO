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
        Minimize,
        // The run screen's side panel.
        TogglePanel,
        NewPanelTab,
        ClosePanelTab,
        ReopenPanelTab,
        NextPanelTab,
        PrevPanelTab,
        // Hands the keyboard to the approval panel waiting in the run.
        JumpToPanel,
        // The sidebar's thread menu: Escape closes what is open; the others act on the thread.
        DismissSidebar,
        PinThread,
        ArchiveThread
    ]
);

/// The key context the root view carries while something of the sidebar is open.
pub(crate) const SIDEBAR_OPEN: &str = "SidebarOpen";

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-alt-b", TogglePanel, None),
        KeyBinding::new("cmd-t", NewPanelTab, None),
        // ⌘W closes the run panel's tab when one is open, else the window (RootView).
        KeyBinding::new("cmd-w", ClosePanelTab, None),
        KeyBinding::new("cmd-j", JumpToPanel, None),
        KeyBinding::new("cmd-shift-t", ReopenPanelTab, None),
        KeyBinding::new("ctrl-tab", NextPanelTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevPanelTab, None),
        // Only while a menu, confirm or rename of the sidebar is open (the root view says so).
        KeyBinding::new("escape", DismissSidebar, Some(SIDEBAR_OPEN)),
        KeyBinding::new("cmd-shift-p", PinThread, None),
        KeyBinding::new("cmd-shift-a", ArchiveThread, None),
    ]);
}
