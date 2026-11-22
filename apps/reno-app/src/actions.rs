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
        // Find in the run's conversation: open the bar, next and previous hit, and close it.
        FindInTranscript,
        FindNext,
        FindPrevious,
        CloseFind,
        // The find bar's options: match case, whole word and regular expression.
        ToggleMatchCase,
        ToggleWholeWord,
        ToggleRegex,
        // The transcript, while it has the keyboard: page, three lines, either end, and the
        // previous or next prompt.
        PageUp,
        PageDown,
        LineUp,
        LineDown,
        Home,
        End,
        PrevPrompt,
        NextPrompt,
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
        KeyBinding::new("cmd-f", FindInTranscript, None),
        KeyBinding::new("cmd-g", FindNext, Some(crate::ui::find::OPEN_CONTEXT)),
        KeyBinding::new(
            "cmd-shift-g",
            FindPrevious,
            Some(crate::ui::find::OPEN_CONTEXT),
        ),
        // While the bar is open Escape closes it, wherever the keyboard is in the run screen; the
        // composer and the approval panel pass theirs on to it.
        KeyBinding::new("escape", CloseFind, Some(crate::ui::find::OPEN_CONTEXT)),
        KeyBinding::new(
            "cmd-alt-c",
            ToggleMatchCase,
            Some(crate::ui::find::OPEN_CONTEXT),
        ),
        KeyBinding::new(
            "cmd-alt-w",
            ToggleWholeWord,
            Some(crate::ui::find::OPEN_CONTEXT),
        ),
        KeyBinding::new(
            "cmd-alt-x",
            ToggleRegex,
            Some(crate::ui::find::OPEN_CONTEXT),
        ),
        KeyBinding::new(
            "shift-enter",
            FindPrevious,
            Some(crate::ui::find::BAR_CONTEXT),
        ),
        KeyBinding::new("ctrl-tab", NextPanelTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevPanelTab, None),
        // Only while a menu, confirm or rename of the sidebar is open (the root view says so).
        KeyBinding::new("escape", DismissSidebar, Some(SIDEBAR_OPEN)),
        KeyBinding::new("cmd-shift-p", PinThread, None),
        KeyBinding::new("cmd-shift-a", ArchiveThread, None),
    ]);
}
