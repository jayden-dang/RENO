//! Images in rendered markdown (TRANSCRIPT-11.1 to 11.5, 16.5, 16.7).
//!
//! A markdown image source is untrusted text. It is turned into a typed [`ImageSource`] (a
//! worktree path or decoded bytes) or into something that fetches nothing; it never reaches
//! `gpui::img()` as a string, because GPUI turns a URL-shaped string into a network fetch through
//! the app's HTTP client.
//!
//! - A relative path is read through the daemon's worktree-rooted `fs.read_image`
//!   ([`reno_client::Client::fs_read_image_all`]) for the host's run, once, and cached in a bounded
//!   LRU. A path that is absolute, climbs out with `..` or carries a scheme is refused here and
//!   the daemon refuses it again.
//! - A `data:` image is decoded in the app.
//! - An `http` or `https` image becomes a link card (its alt text and host) that opens only
//!   through the host's link policy; nothing is fetched for it.
//! - Anything else, and any failure, is "Image not loaded: <alt or path>".

use std::sync::Arc;

use base64::Engine as _;
use gpui::{AnyElement, App, ImageFormat, SharedString, Task, div, img, prelude::*, px};
use reno_client::Client;
use reno_proto::RunId;

use super::link::{self, Action, LinkPolicy};
use super::lru::Lru;
use crate::ui::icon::{Icon, icon};
use crate::ui::theme::Theme;

/// The largest `data:` payload decoded, in bytes.
const DATA_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Images kept decoded in memory, and the most bytes they may hold together (ARCH-12).
const CACHE_ENTRIES: usize = 64;
const CACHE_BYTES: usize = 48 * 1024 * 1024;

/// Where an image's bytes come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageSource {
    /// A file in the run's worktree, relative to its root.
    Worktree(String),
    /// Decoded in the app from a `data:` URL.
    Data(Arc<gpui::Image>),
}

/// What an image source in markdown turns into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Classified {
    Source(ImageSource),
    /// A web image: shown as a link card, never fetched.
    Remote {
        url: String,
        host: String,
    },
    /// Not loadable: an absolute or escaping path, an unknown scheme, a broken `data:` URL.
    Refused,
}

/// Sorts an image source from markdown text. Pure: it reads nothing.
pub(crate) fn classify(src: &str) -> Classified {
    let src = src.trim();
    let lower = src.to_ascii_lowercase();
    if lower.starts_with("data:") {
        return decode_data(src).map_or(Classified::Refused, |image| {
            Classified::Source(ImageSource::Data(Arc::new(image)))
        });
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Classified::Remote {
            url: src.to_owned(),
            host: host_of(src),
        };
    }
    match link::clean_path(src) {
        Some(path) => Classified::Source(ImageSource::Worktree(path)),
        None => Classified::Refused,
    }
}

/// `host` of a web URL, without credentials or port.
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host.to_owned()
}

fn decode_data(src: &str) -> Option<gpui::Image> {
    let rest = src.get("data:".len()..)?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    if !parts.any(|p| p.eq_ignore_ascii_case("base64")) {
        return None;
    }
    let format = match mime.as_str() {
        "image/svg+xml" => ImageFormat::Svg,
        other => ImageFormat::from_mime_type(other)?,
    };
    if payload.len() / 4 * 3 > DATA_MAX_BYTES {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    (!bytes.is_empty() && bytes.len() <= DATA_MAX_BYTES)
        .then(|| gpui::Image::from_bytes(format, bytes))
}

/// How the host reads a run's worktree images.
#[derive(Clone, Default)]
pub(crate) struct ImageHost {
    pub(crate) run: Option<RunId>,
    pub(crate) client: Option<Client>,
}

impl std::fmt::Debug for ImageHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageHost")
            .field("run", &self.run)
            .field("client", &self.client.is_some())
            .finish()
    }
}

/// A worktree image's load.
enum Slot {
    /// Asked for; the task is held so that a repaint does not ask again and a dropped cache
    /// entry cancels the read (ARCH-11).
    Loading(Task<()>),
    Ready(Arc<gpui::Image>),
    Failed,
}

/// The images read from worktrees, by (run, path), bounded.
struct Images {
    slots: Lru<(RunId, String), Slot>,
}

impl gpui::Global for Images {}

impl Default for Images {
    fn default() -> Self {
        Self {
            slots: Lru::new(CACHE_ENTRIES, CACHE_BYTES),
        }
    }
}

/// What an image is right now.
pub(crate) enum Shown {
    /// Being read.
    Loading,
    Ready(Arc<gpui::Image>),
    /// Missing, unreadable, an unsupported type, or no worktree to read it from.
    Failed,
}

/// The image `source` names: decoded bytes at once, a worktree file once it is read.
pub(crate) fn resolve(source: &ImageSource, host: &ImageHost, cx: &mut App) -> Shown {
    match source {
        ImageSource::Data(image) => Shown::Ready(image.clone()),
        ImageSource::Worktree(path) => state_of(host, path, cx),
    }
}

/// The state of `path` of `run`, starting its read when nobody has.
fn state_of(host: &ImageHost, path: &str, cx: &mut App) -> Shown {
    let (Some(run), Some(client)) = (host.run.clone(), host.client.clone()) else {
        return Shown::Failed;
    };
    let key = (run.clone(), path.to_owned());
    if let Some(slot) = cx.default_global::<Images>().slots.get(&key) {
        return match slot {
            Slot::Loading(_) => Shown::Loading,
            Slot::Ready(image) => Shown::Ready(image.clone()),
            Slot::Failed => Shown::Failed,
        };
    }
    let read = gpui_tokio::Tokio::spawn(cx, {
        let (run, path) = (run, path.to_owned());
        async move { client.fs_read_image_all(run, path).await }
    });
    let task = cx.spawn({
        let key = key.clone();
        async move |cx| {
            let slot = match read.await {
                Ok(Ok((mime, bytes))) => ImageFormat::from_mime_type(&mime)
                    .map(|format| {
                        let cost = bytes.len();
                        (
                            Slot::Ready(Arc::new(gpui::Image::from_bytes(format, bytes))),
                            cost,
                        )
                    })
                    .unwrap_or((Slot::Failed, 0)),
                _ => (Slot::Failed, 0),
            };
            cx.update(|cx| {
                let images = cx.default_global::<Images>();
                // Only while the entry is still ours: a cache that dropped it cancelled the read.
                if let Some(Slot::Loading(done)) = images.slots.remove(&key) {
                    done.detach();
                    let (slot, cost) = slot;
                    images.slots.insert(key, slot, cost);
                }
                super::settled(cx);
            });
        }
    });
    cx.default_global::<Images>()
        .slots
        .insert(key, Slot::Loading(task), 0);
    Shown::Loading
}

/// The text shown for an image that is not there: "Image not loaded: <alt or path>".
pub(crate) fn not_loaded_text(alt: &str, src: &str) -> String {
    let what = if alt.trim().is_empty() { src } else { alt };
    format!("Image not loaded: {}", what.trim())
}

pub(crate) fn note(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(8.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .text_size(px(13.0))
        .text_color(theme.muted)
        .child(text.into())
}

fn failed_note(theme: &Theme, text: String) -> AnyElement {
    note(theme, text)
        .debug_selector(|| "md-image-not-loaded".to_owned())
        .into_any_element()
}

/// The drawn picture of decoded bytes, scaled down to the column and never up.
pub(crate) fn picture(image: Arc<gpui::Image>) -> gpui::Div {
    div()
        .debug_selector(|| "md-image".to_owned())
        .child(img(image).max_w_full().rounded(px(6.0)))
}

/// The image a markdown `![alt](src)` stands for. `wrapper` is the link around the image
/// (`[![alt](src)](url)`), followed as any link is (TRANSCRIPT-11.5).
pub(crate) fn view(
    alt: &str,
    src: &str,
    wrapper: Option<&str>,
    host: &ImageHost,
    policy: &LinkPolicy,
    theme: &Theme,
    cx: &mut App,
) -> AnyElement {
    let action = wrapper.map(|url| policy.action(url));
    let body = match classify(src) {
        Classified::Source(source) => match resolve(&source, host, cx) {
            Shown::Ready(image) => picture(image).into_any_element(),
            Shown::Loading => note(theme, "Loading image\u{2026}").into_any_element(),
            Shown::Failed => failed_note(theme, not_loaded_text(alt, src)),
        },
        Classified::Remote { url, host: site } => {
            return link_card(alt, &url, &site, action, policy, theme);
        }
        Classified::Refused => failed_note(theme, not_loaded_text(alt, src)),
    };
    match action.filter(|a| *a != Action::Plain) {
        Some(action) => clickable(body, action, policy, alt),
        None => body,
    }
}

fn clickable(body: AnyElement, action: Action, policy: &LinkPolicy, label: &str) -> AnyElement {
    let policy = policy.clone();
    div()
        .id(SharedString::from(format!("md-image-link-{label}")))
        .cursor_pointer()
        .child(body)
        .on_click(move |_, window, cx| policy.run(&action, window, cx))
        .into_any_element()
}

/// A web image as a card: its alt text and host. It opens the page the image names, or the
/// link around it, only through the host's policy; nothing is fetched.
fn link_card(
    alt: &str,
    url: &str,
    site: &str,
    wrapper: Option<Action>,
    policy: &LinkPolicy,
    theme: &Theme,
) -> AnyElement {
    let action = wrapper
        .filter(|a| *a != Action::Plain)
        .unwrap_or_else(|| policy.action(url));
    let title = if alt.trim().is_empty() {
        "Image"
    } else {
        alt.trim()
    };
    let card = div()
        .id(SharedString::from(format!("md-image-card-{url}")))
        .debug_selector(|| "md-image-card".to_owned())
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .child(icon(Icon::Globe, 15.0, theme.muted))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(site.to_owned()),
                ),
        );
    if action == Action::Plain {
        return card.into_any_element();
    }
    let policy = policy.clone();
    card.cursor_pointer()
        .on_click(move |_, window, cx| policy.run(&action, window, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn a_relative_path_is_a_worktree_source() {
        assert_eq!(
            classify("docs/graph.png"),
            Classified::Source(ImageSource::Worktree("docs/graph.png".to_owned()))
        );
        assert_eq!(
            classify("./docs/graph.png"),
            Classified::Source(ImageSource::Worktree("docs/graph.png".to_owned()))
        );
    }

    #[test]
    fn a_path_outside_the_worktree_or_with_a_scheme_is_refused() {
        for bad in [
            "/etc/passwd",
            "../outside.png",
            "docs/../../outside.png",
            "file:///etc/passwd",
            "~/a.png",
            "ftp://example.com/a.png",
            "javascript:alert(1)",
            "C:\\a.png",
            "",
        ] {
            assert_eq!(classify(bad), Classified::Refused, "{bad}");
        }
    }

    #[test]
    fn a_web_image_is_a_card_with_its_host_and_never_a_source() {
        assert_eq!(
            classify("https://example.com/status.png"),
            Classified::Remote {
                url: "https://example.com/status.png".to_owned(),
                host: "example.com".to_owned()
            }
        );
        assert_eq!(
            host_of("http://user:pw@cdn.example.org:8080/a?b#c"),
            "cdn.example.org"
        );
        assert_eq!(host_of("https://[::1]:80/x"), "::1");
        assert!(matches!(
            classify("HTTP://A.B/c"),
            Classified::Remote { .. }
        ));
    }

    #[test]
    fn a_data_image_decodes_to_bytes() {
        match classify(PNG_1X1) {
            Classified::Source(ImageSource::Data(image)) => {
                assert_eq!(image.format, ImageFormat::Png);
                assert_eq!(&image.bytes[1..4], b"PNG");
            }
            other => panic!("{other:?}"),
        }
        for bad in [
            "data:image/png;base64,@@@@",
            "data:text/html;base64,PGI+",
            "data:image/png,rawbytes",
            "data:image/png;base64,",
            "data:",
        ] {
            assert_eq!(classify(bad), Classified::Refused, "{bad}");
        }
    }

    #[test]
    fn the_failure_text_names_the_alt_or_else_the_path() {
        assert_eq!(
            not_loaded_text("build graph", "docs/graph.png"),
            "Image not loaded: build graph"
        );
        assert_eq!(
            not_loaded_text("  ", "docs/graph.png"),
            "Image not loaded: docs/graph.png"
        );
    }

    #[gpui::test]
    fn a_worktree_image_without_a_run_is_not_loaded_and_asks_for_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let state = state_of(&ImageHost::default(), "docs/graph.png", cx);
            assert!(matches!(state, Shown::Failed));
            assert_eq!(cx.default_global::<Images>().slots.len(), 0);
        });
    }
}
