//! Desktop banners and the Dock badge, for the app (docs/design/2026-11-18-desktop-notifications-brief.md).
//!
//! CONTRACT (stub): the signatures below are what `reno-app` builds against. The macOS bodies
//! are filled in by the native-layer task; until then every call is a no-op.
//!
//! - Call everything from the main thread (GPUI's app context is). A banner click is reported
//!   on the main thread, mid-event: the handler must not re-enter the UI, only send.
//! - Failures are logged and swallowed: a missing notifier never bothers the app.
//! - `RENO_DISABLE_NOTIFICATIONS` (any non-empty value but `0`) turns [`post`] into a no-op.

/// One banner. Posting a banner with the `id` of one still in Notification Center replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    /// Stable identity: replaces an earlier banner with the same id, and [`withdraw`] takes it.
    pub id: String,
    pub title: String,
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

/// The bundle identifier banners are attributed to when this process has none of its own (a
/// `cargo run` build): adopted only if an app with that id is installed. Call before the first
/// [`post`].
pub fn adopt_bundle_id(id: &str) {
    let _ = id;
}

/// How [`post`] will deliver, as far as can be told without posting.
pub fn delivery() -> Delivery {
    Delivery::None
}

/// Post `banner`.
pub fn post(banner: &Banner) {
    let _ = banner;
}

/// Remove a delivered banner from Notification Center, if it is still there.
pub fn withdraw(id: &str) {
    let _ = id;
}

/// Route banner clicks: `handler` receives the clicked banner's target. Replaces any previous
/// handler.
pub fn on_click(handler: impl Fn(String) + Send + 'static) {
    let _ = handler;
}

/// The Dock badge: `None` clears it.
pub fn set_badge(label: Option<&str>) {
    let _ = label;
}
