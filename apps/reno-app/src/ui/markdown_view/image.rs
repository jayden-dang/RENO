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
use gpui::{
    AnyElement, App, Div, ImageFormat, MouseButton, SharedString, Stateful, Task, div, img,
    prelude::*, px,
};
use reno_client::Client;
use reno_markdown::parser::InlineImage;
use reno_proto::RunId;

use super::Host;
use super::link::{self, Action};
use super::lru::Lru;
use crate::ui::icon::{Icon, icon};
use crate::ui::theme::Theme;

/// The largest `data:` payload decoded, in bytes.
const DATA_MAX_BYTES: usize = 8 * 1024 * 1024;
const SVG_MIME: &str = "image/svg+xml";
/// Bound the raster allocation independently of the source dimensions.
const MAX_SVG_SIDE: f32 = 2048.0;
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
    /// Encoded Markdown SVG, decoded and rasterized by the image cache.
    SvgData(Arc<str>),
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
        return decode_data(src).map_or(Classified::Refused, Classified::Source);
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

/// The format of a raster image type. An SVG is refused: GPUI draws it with a renderer that
/// reads any file path an `<image>` inside it names, and a picture in a reply must not show a
/// file from outside the worktree (TRANSCRIPT-16.7).
pub(crate) fn raster_format(mime: &str) -> Option<ImageFormat> {
    ImageFormat::from_mime_type(&mime.to_ascii_lowercase()).filter(|f| *f != ImageFormat::Svg)
}

fn decode_data(src: &str) -> Option<ImageSource> {
    let rest = src.get("data:".len()..)?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    if !parts.any(|p| p.eq_ignore_ascii_case("base64")) {
        return None;
    }
    let payload = payload.trim();
    if payload.is_empty() || payload.len() > DATA_MAX_BYTES.div_ceil(3) * 4 {
        return None;
    }
    if mime == SVG_MIME {
        return Some(ImageSource::SvgData(Arc::from(payload)));
    }
    let format = raster_format(&mime)?;
    let bytes = decode_payload(payload)?;
    Some(ImageSource::Data(Arc::new(gpui::Image::from_bytes(
        format, bytes,
    ))))
}

fn decode_payload(payload: &str) -> Option<Vec<u8>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    (!bytes.is_empty() && bytes.len() <= DATA_MAX_BYTES).then_some(bytes)
}

/// Draw a Markdown SVG with the bundled font and without resolving nested image resources.
pub(crate) fn rasterize_svg(bytes: &[u8], scale: f32) -> Option<super::mermaid::Raster> {
    if bytes.len() > DATA_MAX_BYTES || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let mut image_href_resolver = super::mermaid::no_files();
    // Embedded images can allocate or decompress independently of the outer canvas.
    image_href_resolver.resolve_data = Box::new(|_, _, _| None);
    let options = usvg::Options {
        fontdb: super::mermaid::fonts(),
        font_family: "sans-serif".to_owned(),
        image_href_resolver,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(std::str::from_utf8(bytes).ok()?, &options).ok()?;
    if tree.root().children().is_empty() {
        return None;
    }
    let (width, height) = (tree.size().width(), tree.size().height());
    let raster_scale = scale.min(MAX_SVG_SIDE / width.max(height));
    let mut pixmap = tiny_skia::Pixmap::new(
        (width * raster_scale).ceil() as u32,
        (height * raster_scale).ceil() as u32,
    )?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(raster_scale, raster_scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .pixels()
        .iter()
        .any(|pixel| pixel.alpha() != 0)
        .then_some(())?;
    Some(super::mermaid::Raster {
        png: pixmap.encode_png().ok()?,
        width: pixmap.width() as f32,
        height: pixmap.height() as f32,
    })
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

/// Bitmap-only callers and Markdown use separate worktree cache keys.
#[derive(Clone, Eq, Hash, PartialEq)]
enum CacheKey {
    Worktree { run: RunId, path: String, svg: bool },
    SvgData(Arc<str>),
}

struct Images {
    slots: Lru<CacheKey, Slot>,
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

/// Images posted by tools and the visual desk are bitmap-only.
pub(crate) fn resolve(source: &ImageSource, host: &ImageHost, cx: &mut App) -> Shown {
    match source {
        ImageSource::Data(image) => Shown::Ready(image.clone()),
        ImageSource::Worktree(path) => state_of(host, path, false, cx),
        ImageSource::SvgData(_) => Shown::Failed,
    }
}

fn resolve_markdown(source: &ImageSource, host: &ImageHost, cx: &mut App) -> Shown {
    match source {
        ImageSource::Data(image) => Shown::Ready(image.clone()),
        ImageSource::Worktree(path) => state_of(host, path, true, cx),
        ImageSource::SvgData(payload) => {
            let payload = payload.clone();
            let key = CacheKey::SvgData(payload.clone());
            let cost = payload.len();
            cached_load(key, cost, cx, move |cx| {
                cx.background_spawn(async move {
                    decode_payload(&payload).and_then(|bytes| decoded(SVG_MIME, bytes, true))
                })
            })
        }
    }
}

fn decoded(mime: &str, bytes: Vec<u8>, svg: bool) -> Option<(Arc<gpui::Image>, usize)> {
    let (format, bytes, pixels) = if svg && mime.eq_ignore_ascii_case(SVG_MIME) {
        let raster = rasterize_svg(&bytes, 1.0)?;
        (
            ImageFormat::Png,
            raster.png,
            raster.width as usize * raster.height as usize * 4,
        )
    } else {
        (raster_format(mime)?, bytes, 0)
    };
    let cost = bytes.len() + pixels;
    Some((Arc::new(gpui::Image::from_bytes(format, bytes)), cost))
}

/// The state of `path` of `run`, starting its read when nobody has.
fn state_of(host: &ImageHost, path: &str, svg: bool, cx: &mut App) -> Shown {
    let (Some(run), Some(client)) = (host.run.clone(), host.client.clone()) else {
        return Shown::Failed;
    };
    let path = path.to_owned();
    let key = CacheKey::Worktree {
        run: run.clone(),
        path: path.clone(),
        svg,
    };
    cached_load(key, 0, cx, move |cx| {
        let read =
            gpui_tokio::Tokio::spawn(cx, async move { client.fs_read_image_all(run, path).await });
        let executor = cx.background_executor().clone();
        async move {
            let (mime, bytes) = read.await.ok()?.ok()?;
            executor
                .spawn(async move { decoded(&mime, bytes, svg) })
                .await
        }
    })
}

fn cached_load<F: Future<Output = Option<(Arc<gpui::Image>, usize)>> + 'static>(
    key: CacheKey,
    key_cost: usize,
    cx: &mut App,
    start: impl FnOnce(&mut App) -> F,
) -> Shown {
    if let Some(slot) = cx.default_global::<Images>().slots.get(&key) {
        return match slot {
            Slot::Loading(_) => Shown::Loading,
            Slot::Ready(image) => Shown::Ready(image.clone()),
            Slot::Failed => Shown::Failed,
        };
    }
    let load = start(cx);
    let task = cx.spawn({
        let key = key.clone();
        async move |cx| {
            let (slot, cost) = load.await.map_or((Slot::Failed, 0), |(image, cost)| {
                (Slot::Ready(image), cost)
            });
            cx.update(|cx| {
                let images = cx.default_global::<Images>();
                // A cache that dropped the entry cancelled its load.
                if let Some(Slot::Loading(done)) = images.slots.remove(&key) {
                    done.detach();
                    images.slots.insert(key, slot, key_cost + cost);
                }
                super::settled(cx);
            });
        }
    });
    cx.default_global::<Images>()
        .slots
        .insert(key, Slot::Loading(task), key_cost);
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

/// The image a markdown `![alt](src)` stands for. The wrapper is the link around the image
/// (`[![alt](src)](url)`), followed as any link is (TRANSCRIPT-11.5).
pub(crate) fn view(
    image: &InlineImage,
    atom: usize,
    host: &ImageHost,
    markdown: &Host,
    theme: &Theme,
    cx: &mut App,
) -> AnyElement {
    let (alt, src) = (image.alt.as_str(), image.source.as_str());
    let wrapped = image
        .link
        .as_deref()
        .map(|url| (markdown.links.action(url), url))
        .filter(|(action, _)| *action != Action::Plain);
    let body = match classify(src) {
        Classified::Source(source) => match resolve_markdown(&source, host, cx) {
            Shown::Ready(image) => picture(image).into_any_element(),
            Shown::Loading => note(theme, "Loading image\u{2026}").into_any_element(),
            Shown::Failed => failed_note(theme, not_loaded_text(alt, src)),
        },
        Classified::Remote { url, host: site } => {
            let (action, target) = wrapped.unwrap_or_else(|| (markdown.links.action(&url), &url));
            return clickable(
                link_card(alt, &url, &site, theme),
                action,
                target,
                atom,
                markdown,
            );
        }
        Classified::Refused => failed_note(theme, not_loaded_text(alt, src)),
    };
    match wrapped {
        Some((action, target)) => clickable(
            div()
                .id(SharedString::from(format!("md-image-link-{alt}")))
                .child(body),
            action,
            target,
            atom,
            markdown,
        ),
        None => body,
    }
}

fn clickable(
    body: Stateful<Div>,
    action: Action,
    target: &str,
    atom: usize,
    markdown: &Host,
) -> AnyElement {
    if action == Action::Plain {
        return body.into_any_element();
    }
    let policy = markdown.links.clone();
    let target = target.to_owned();
    body.cursor_pointer()
        .on_click(move |_, window, cx| policy.run(&action, window, cx))
        .when_some(markdown.link_menu.clone(), |card, menu| {
            let (track, tracked_target) = (menu.clone(), target.clone());
            card.relative()
                .child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| {
                            track.paint_target(atom, &tracked_target, bounds, window);
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    menu.open(Some(&target), event, window, cx);
                })
        })
        .into_any_element()
}

/// A web image as a card: its alt text and host. It opens the page the image names, or the
/// link around it, only through the host's policy; nothing is fetched.
fn link_card(alt: &str, url: &str, site: &str, theme: &Theme) -> Stateful<Div> {
    let title = if alt.trim().is_empty() {
        "Image"
    } else {
        alt.trim()
    };
    div()
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
        )
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
    fn svg_text_uses_the_bundled_font() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="32">
            <text x="4" y="24">Geist</text>
        </svg>"#;
        assert!(rasterize_svg(svg, 1.0).is_some());
    }

    #[test]
    fn embedded_svg_images_are_not_decoded() {
        use base64::engine::general_purpose::STANDARD;

        let nested = STANDARD.encode(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">
            <rect width="8" height="8"/>
            </svg>"#,
        );
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">
             <image href="data:image/svg+xml;base64,{nested}" width="8" height="8"/>
            </svg>"#,
        );
        assert!(rasterize_svg(svg.as_bytes(), 1.0).is_none());
    }

    #[test]
    fn a_malformed_svg_does_not_rasterize() {
        assert!(rasterize_svg(b"<svg/>", 1.0).is_none());
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
            let state = state_of(&ImageHost::default(), "docs/graph.png", true, cx);
            assert!(matches!(state, Shown::Failed));
            assert_eq!(cx.default_global::<Images>().slots.len(), 0);
        });
    }
}
