//! Opening a web page the daemon names (a sign-in page, with a device code to type in it).
//!
//! The daemon never opens a page; it tells the app, and the app opens it on the device the person
//! is at. Only web links are opened: anything else is refused. [`UrlOpener`] is injected
//! (ARCH-16): the real one is the platform's. Tests install a recording fake, and with none
//! installed a test that reaches [`open`] panics, so a test can never reach a real browser.

use gpui::App;
#[cfg(test)]
use gpui::Global;

/// Opens a web page for the person.
#[cfg(test)]
pub(crate) trait UrlOpener {
    fn open(&self, url: &str);
}

/// Whether `url` is a web link: the only kind opened from what a daemon says.
pub(crate) fn is_web_link(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

#[cfg(test)]
struct Handle(std::rc::Rc<dyn UrlOpener>);

#[cfg(test)]
impl Global for Handle {}

/// Installs the opener the sign-in uses. The app installs none: it uses the platform's.
#[cfg(test)]
pub(crate) fn install(cx: &mut App, opener: std::rc::Rc<dyn UrlOpener>) {
    cx.set_global(Handle(opener));
}

/// Opens `url` when it is a web link. `false` when it is not (nothing was opened). In a test with
/// no opener installed this panics.
pub(crate) fn open(cx: &App, url: &str) -> bool {
    if !is_web_link(url) {
        return false;
    }
    #[cfg(test)]
    match cx.try_global::<Handle>() {
        Some(handle) => handle.0.open(url),
        None => panic!("a test reached the real URL opener: install a fake with `install`"),
    }
    #[cfg(not(test))]
    cx.open_url(url);
    true
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::*;

    /// Records what would have been opened.
    #[derive(Default)]
    pub(crate) struct Recording {
        pub(crate) opened: RefCell<Vec<String>>,
    }

    impl UrlOpener for Recording {
        fn open(&self, url: &str) {
            self.opened.borrow_mut().push(url.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_are_web_links() {
        assert!(is_web_link("https://example.com/device"));
        assert!(is_web_link("http://localhost:8080/cb"));
        for no in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "x-apple.systempreferences:",
            "open -a Terminal",
            "",
        ] {
            assert!(!is_web_link(no), "{no}");
        }
    }
}
