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
}

/// One row of the "+" menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlusItem {
    Files,
    PlanMode,
    Goal,
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
    };

    /// The fake daemon: every not-yet control is on.
    pub(crate) const FAKE: Self = Self {
        plan_mode: true,
        goal: true,
        permission_modes: true,
        allow_similar: true,
    };

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
        assert!(!Capabilities::REAL.permission_modes);
        assert!(!Capabilities::REAL.allow_similar);
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
        assert!(Capabilities::FAKE.permission_modes && Capabilities::FAKE.allow_similar);
    }
}
