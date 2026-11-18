//! Desktop banners and the Dock badge, for the app (docs/design/2026-11-18-desktop-notifications-brief.md).
//!
//! The signatures below are what `reno-app` builds against. macOS goes through
//! `NSUserNotification` (deprecated, still shipping; the class is looked up defensively), with
//! `osascript` when this process has no notification center; every other platform is a no-op.
//!
//! - Call everything from the main thread (GPUI's app context is). A banner click is reported
//!   on the main thread, mid-event: the handler must not re-enter the UI, only send.
//! - Failures are swallowed silently (the app has no logging facade): a missing notifier never
//!   bothers the app.
//! - `RENO_DISABLE_NOTIFICATIONS` (any non-empty value but `0`) turns [`post`] into a no-op.

use std::ffi::OsStr;
use std::sync::Mutex;

const DISABLE_ENV: &str = "RENO_DISABLE_NOTIFICATIONS";

type ClickHandler = Box<dyn Fn(String) + Send>;

static CLICK: Mutex<Option<ClickHandler>> = Mutex::new(None);

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;

/// One banner. Posting a banner with the `id` of one still in Notification Center replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    /// Stable identity: replaces an earlier banner with the same id, and [`withdraw`] takes it.
    pub id: String,
    /// What happened, short: macOS cuts a long title to one line.
    pub title: String,
    /// The specific: what is asked, why it failed. One line.
    pub subtitle: Option<String>,
    /// Shown on up to two lines.
    pub body: String,
    /// Opaque to this crate; handed back to the [`on_click`] handler.
    pub target: Option<String>,
}

/// How banners will show in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Through Notification Center, as the app (this process's bundle, or an adopted one).
    Native,
    /// Through `osascript`: shown as Script Editor, clicks report nothing.
    Script,
    /// Not at all (not macOS, or disabled by the environment).
    None,
}

/// Whether an environment value switches banners off: anything non-empty but `0`.
fn disabled_by(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

fn disabled() -> bool {
    disabled_by(std::env::var_os(DISABLE_ENV).as_deref())
}

/// The bundle identifier banners are attributed to when this process has none of its own (a
/// `cargo run` build): adopted only if an app with that id is installed. Call before the first
/// [`post`].
pub fn adopt_bundle_id(id: &str) {
    #[cfg(target_os = "macos")]
    macos::adopt_bundle_id(id);
    #[cfg(not(target_os = "macos"))]
    let _ = id;
}

/// How [`post`] will deliver, as far as can be told without posting.
pub fn delivery() -> Delivery {
    if disabled() {
        return Delivery::None;
    }
    #[cfg(target_os = "macos")]
    {
        if macos::has_center() {
            Delivery::Native
        } else {
            Delivery::Script
        }
    }
    #[cfg(not(target_os = "macos"))]
    Delivery::None
}

/// Post `banner`.
pub fn post(banner: &Banner) {
    post_via(banner, disabled(), deliver);
}

/// The seam under [`post`]: `deliver` runs only when banners are not disabled.
fn post_via(banner: &Banner, disabled: bool, deliver: impl FnOnce(&Banner)) {
    if !disabled {
        deliver(banner);
    }
}

fn deliver(banner: &Banner) {
    #[cfg(target_os = "macos")]
    macos::post(banner);
    #[cfg(not(target_os = "macos"))]
    let _ = banner;
}

/// Remove a delivered banner from Notification Center, if it is still there.
pub fn withdraw(id: &str) {
    #[cfg(target_os = "macos")]
    macos::withdraw(id);
    #[cfg(not(target_os = "macos"))]
    let _ = id;
}

/// Route banner clicks: `handler` receives the clicked banner's target. Replaces any previous
/// handler.
pub fn on_click(handler: impl Fn(String) + Send + 'static) {
    set_click_handler(handler);
    #[cfg(target_os = "macos")]
    macos::install_delegate();
}

/// Store the click handler without touching Notification Center (the test seam under [`on_click`]).
fn set_click_handler(handler: impl Fn(String) + Send + 'static) {
    *CLICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(handler));
}

/// The Dock badge: `None` clears it. Main thread only; anywhere else it does nothing.
pub fn set_badge(label: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::set_badge(label);
    #[cfg(not(target_os = "macos"))]
    let _ = label;
}

/// Hand a clicked banner's target to the registered handler.
#[cfg(target_os = "macos")]
fn clicked(target: String) {
    if let Some(handler) = CLICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        handler(target);
    }
}

/// Escape a string for interpolation inside a double-quoted AppleScript literal: backslash and
/// double-quote are the metacharacters, and a raw newline ends the statement (which would
/// silently drop the banner), so those flatten to spaces.
#[cfg(any(target_os = "macos", test))]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn banner() -> Banner {
        Banner {
            id: "done:r1".into(),
            title: "t".into(),
            subtitle: None,
            body: "b".into(),
            target: Some("run:r1".into()),
        }
    }

    #[test]
    fn applescript_escaping() {
        assert_eq!(applescript_escape("plain"), "plain");
        assert_eq!(
            applescript_escape(r#"say "hi" \ bye"#),
            r#"say \"hi\" \\ bye"#
        );
        // Raw newlines would end the AppleScript statement mid-literal.
        assert_eq!(applescript_escape("two\nlines\r\n"), "two lines  ");
    }

    #[test]
    fn the_disable_value_is_non_empty_and_not_zero() {
        assert!(!disabled_by(None));
        assert!(!disabled_by(Some(OsStr::new(""))));
        assert!(!disabled_by(Some(OsStr::new("0"))));
        assert!(disabled_by(Some(OsStr::new("1"))));
        assert!(disabled_by(Some(OsStr::new("yes"))));
    }

    #[test]
    fn a_disabled_post_delivers_nothing() {
        let mut delivered = false;
        post_via(&banner(), true, |_| delivered = true);
        assert!(!delivered);
        post_via(&banner(), false, |_| delivered = true);
        assert!(delivered);
    }
}
