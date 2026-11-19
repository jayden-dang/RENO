//! Which composer controls are available: the one place that decides (ui-brief, "Real daemon vs.
//! not-yet controls"). Controls whose daemon support is not built are hidden with the real
//! daemon and shown on the in-process fake, so the design stays testable. The store derives the
//! set from its connection; nothing else asks "is this the fake?".

/// What the composer and its panels may show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// "Plan mode" in the "+" menu (no daemon support yet).
    pub(crate) plan_mode: bool,
    /// "Goal" in the "+" menu (no daemon support yet).
    pub(crate) goal: bool,
    /// The harness's own permission-mode chip (ROAD-4).
    pub(crate) permission_modes: bool,
    /// "Allow similar commands" in the approval panel's ▾ menu (ROAD-5).
    pub(crate) allow_similar: bool,
    /// A question can be answered from the app (ROAD-1 is not built: the fake's mocked answer
    /// contract). Without it the question panel is read-only.
    pub(crate) answer_questions: bool,
    /// The run's plan, background tasks and subagents (ROAD-35 / ROAD-52 are not built: the fake's
    /// mocked activity contract). Without it none of those controls exist.
    pub(crate) run_activity: bool,
}

/// One row of the "+" menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlusItem {
    Files,
    PlanMode,
    Goal,
}

/// One item of the approval panel's ▾ menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalItem {
    /// Deny, with a note that reaches the harness (`approval.message`): real.
    DenyWithNote,
    /// The scopes that keep the answer as a rule (ROAD-5): not built, fake only.
    AllowSimilar,
}

impl ApprovalItem {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::DenyWithNote => "Deny with a note\u{2026}",
            Self::AllowSimilar => "Allow similar commands",
        }
    }
}

impl PlusItem {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Files => "Files and folders",
            Self::PlanMode => "Plan mode",
            Self::Goal => "Goal",
        }
    }
}

impl Capabilities {
    /// A real daemon: every not-yet control is off.
    pub(crate) const REAL: Self = Self {
        plan_mode: false,
        goal: false,
        permission_modes: false,
        allow_similar: false,
        answer_questions: false,
        run_activity: false,
    };

    /// The fake daemon: every not-yet control is on.
    pub(crate) const FAKE: Self = Self {
        plan_mode: true,
        goal: true,
        permission_modes: true,
        allow_similar: true,
        answer_questions: true,
        run_activity: true,
    };

    /// The approval panel's ▾ menu: the note needs a daemon that takes one; the similar-command
    /// scopes need the not-yet capability and a daemon that keeps standing rules.
    pub(crate) fn approval_menu(self, takes_note: bool, keeps_rules: bool) -> Vec<ApprovalItem> {
        let mut items = Vec::new();
        if takes_note {
            items.push(ApprovalItem::DenyWithNote);
        }
        if self.allow_similar && keeps_rules {
            items.push(ApprovalItem::AllowSimilar);
        }
        items
    }

    /// The "+" menu's rows. `files` says whether this box takes attachments (a new task does; a
    /// live run's steer does not).
    pub(crate) fn plus_items(self, files: bool) -> Vec<PlusItem> {
        let mut items = Vec::new();
        if files {
            items.push(PlusItem::Files);
        }
        if self.plan_mode {
            items.push(PlusItem::PlanMode);
        }
        if self.goal {
            items.push(PlusItem::Goal);
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_daemon_hides_every_not_yet_control() {
        assert_eq!(Capabilities::REAL.plus_items(false), []);
        assert_eq!(Capabilities::REAL.plus_items(true), [PlusItem::Files]);
        let real = Capabilities::REAL;
        assert!(
            !real.permission_modes
                && !real.allow_similar
                && !real.answer_questions
                && !real.run_activity
        );
    }

    #[test]
    fn the_approval_menu_depends_on_the_capabilities() {
        use ApprovalItem::{AllowSimilar, DenyWithNote};
        assert_eq!(Capabilities::REAL.approval_menu(true, true), [DenyWithNote]);
        assert_eq!(
            Capabilities::FAKE.approval_menu(true, true),
            [DenyWithNote, AllowSimilar]
        );
        assert_eq!(
            Capabilities::FAKE.approval_menu(true, false),
            [DenyWithNote]
        );
        assert_eq!(Capabilities::FAKE.approval_menu(false, false), []);
    }

    #[test]
    fn the_fake_daemon_shows_them_all() {
        assert_eq!(
            Capabilities::FAKE.plus_items(true),
            [PlusItem::Files, PlusItem::PlanMode, PlusItem::Goal]
        );
        assert_eq!(
            Capabilities::FAKE.plus_items(false),
            [PlusItem::PlanMode, PlusItem::Goal]
        );
        let fake = Capabilities::FAKE;
        assert!(fake.permission_modes && fake.allow_similar && fake.answer_questions);
    }
}
