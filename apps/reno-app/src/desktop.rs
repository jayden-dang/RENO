//! The seam between the app and the desktop: banners, the Dock badge, how banners will show.
//!
//! [`Native`] calls `reno_notify` and is built only in `app::run`, when the app connects to the
//! real socket. `--fake` gets [`Silent`]; tests get `Recorder`. Nothing else in the app calls
//! `reno_notify`, so a test can never post a real banner, touch the Dock or reach `osascript`.

use reno_notify::{Banner, Delivery};

pub(crate) trait Desktop {
    /// Posts `banner`; one with the same id replaces the earlier one.
    fn post(&self, banner: &Banner);
    /// Removes a delivered banner, if it is still there.
    fn withdraw(&self, id: &str);
    /// The Dock badge; `None` clears it.
    fn set_badge(&self, label: Option<&str>);
    /// How banners will show.
    fn delivery(&self) -> Delivery;
}

/// The bundle identifier banners are attributed to when the process has none (a `cargo run`).
const BUNDLE_ID: &str = "dev.reno.app";

/// Banners and the badge through macOS.
pub(crate) struct Native;

impl Native {
    pub(crate) fn new() -> Self {
        reno_notify::adopt_bundle_id(BUNDLE_ID);
        Self
    }

    /// Routes banner clicks to `handler`, which only sends (the click fires mid-AppKit-event).
    pub(crate) fn on_click(handler: impl Fn(String) + Send + 'static) {
        reno_notify::on_click(handler);
    }
}

impl Desktop for Native {
    fn post(&self, banner: &Banner) {
        reno_notify::post(banner);
    }

    fn withdraw(&self, id: &str) {
        reno_notify::withdraw(id);
    }

    fn set_badge(&self, label: Option<&str>) {
        reno_notify::set_badge(label);
    }

    fn delivery(&self) -> Delivery {
        reno_notify::delivery()
    }
}

/// `--fake`: nothing is shown anywhere.
pub(crate) struct Silent;

impl Desktop for Silent {
    fn post(&self, _: &Banner) {}
    fn withdraw(&self, _: &str) {}
    fn set_badge(&self, _: Option<&str>) {}
    fn delivery(&self) -> Delivery {
        Delivery::None
    }
}

#[cfg(test)]
pub(crate) use recorder::Recorder;

#[cfg(test)]
mod recorder {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use reno_notify::{Banner, Delivery};

    use super::Desktop;

    /// What the app asked the desktop to do.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        Post(Banner),
        Withdraw(String),
        Badge(Option<String>),
    }

    /// Records calls instead of making them. Clones share the record.
    #[derive(Clone)]
    pub(crate) struct Recorder {
        calls: Rc<RefCell<Vec<Call>>>,
        delivery: Rc<Cell<Delivery>>,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                calls: Rc::default(),
                delivery: Rc::new(Cell::new(Delivery::Native)),
            }
        }
    }

    impl Recorder {
        pub(crate) fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        pub(crate) fn posts(&self) -> Vec<Banner> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Post(banner) => Some(banner),
                    _ => None,
                })
                .collect()
        }

        pub(crate) fn withdrawn(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Withdraw(id) => Some(id),
                    _ => None,
                })
                .collect()
        }

        /// The badge as last set: `None` when it was never set, `Some(None)` when cleared.
        pub(crate) fn badge(&self) -> Option<Option<String>> {
            self.calls().into_iter().rev().find_map(|call| match call {
                Call::Badge(label) => Some(label),
                _ => None,
            })
        }

        pub(crate) fn clear(&self) {
            self.calls.borrow_mut().clear();
        }

        pub(crate) fn set_delivery(&self, delivery: Delivery) {
            self.delivery.set(delivery);
        }
    }

    impl Desktop for Recorder {
        fn post(&self, banner: &Banner) {
            self.calls.borrow_mut().push(Call::Post(banner.clone()));
        }

        fn withdraw(&self, id: &str) {
            self.calls.borrow_mut().push(Call::Withdraw(id.to_owned()));
        }

        fn set_badge(&self, label: Option<&str>) {
            self.calls
                .borrow_mut()
                .push(Call::Badge(label.map(str::to_owned)));
        }

        fn delivery(&self) -> Delivery {
            self.delivery.get()
        }
    }
}
