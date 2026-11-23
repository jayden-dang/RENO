//! One image or diagram an agent posted (TRANSCRIPT-12.1, 12.4): its caption as a title, then the
//! picture. An image goes through the same typed source as a markdown image; a diagram through
//! the Mermaid path and the diagram frame. A visual that
//! does not load or draw is its caption and "Visual not loaded".

use gpui::{AnyElement, App, FontWeight, IntoElement, RenderOnce, Window, div, prelude::*, px};
use reno_client::visuals::{Visual, VisualImage, VisualKind};
use std::sync::Arc;

use super::chrome::{self, Card};
use super::image::{self, ImageSource, Shown};
use super::link::clean_path;
use super::{Host, mermaid};
use crate::ui::theme::Theme;

/// One posted visual, drawn for `host` (which says where a worktree image is read from).
#[derive(IntoElement)]
pub(crate) struct VisualView {
    visual: Visual,
    host: Host,
    theme: Theme,
}

/// Draws `visual` with its caption. The host's image reader is the run's, as for a markdown
/// image.
#[allow(dead_code)] // The transcript inserts visuals after their entry (ROAD-33 wiring).
pub(crate) fn visual_view(visual: &Visual, host: Host, theme: &Theme) -> AnyElement {
    VisualView {
        visual: visual.clone(),
        host,
        theme: *theme,
    }
    .into_any_element()
}

/// The image a visual posts, as the typed source: refused when its path leaves the worktree or
/// its bytes are not a known image type.
fn source_of(image: &VisualImage) -> Option<ImageSource> {
    match image {
        VisualImage::Worktree(path) => clean_path(path).map(ImageSource::Worktree),
        VisualImage::Data { mime, bytes } => {
            let format = image::raster_format(mime)?;
            (!bytes.is_empty()).then(|| {
                ImageSource::Data(Arc::new(gpui::Image::from_bytes(format, bytes.clone())))
            })
        }
    }
}

fn failed(theme: &Theme) -> AnyElement {
    image::note(theme, "Visual not loaded")
        .debug_selector(|| "md-visual-failed".to_owned())
        .into_any_element()
}

fn body(visual: &Visual, host: &Host, theme: &Theme, window: &Window, cx: &mut App) -> AnyElement {
    match &visual.kind {
        VisualKind::Image(posted) => {
            let (Some(source), Some(images)) = (source_of(posted), host.images.as_ref()) else {
                return failed(theme);
            };
            match image::resolve(&source, images, cx) {
                Shown::Ready(picture) => image::picture(picture).into_any_element(),
                Shown::Loading => image::note(theme, "Loading image\u{2026}").into_any_element(),
                Shown::Failed => failed(theme),
            }
        }
        VisualKind::Diagram(source) => {
            match mermaid::drawing(source, theme, window.scale_factor(), None, cx) {
                mermaid::Drawing::Ready(diagram) => chrome::card(
                    &Card {
                        kind: chrome::Kind::Diagram,
                        copy: source,
                        key: chrome::key_of(Some("mermaid"), source),
                        wrappable: false,
                        wrapped: false,
                        ordinal: 0,
                    },
                    theme,
                    super::diagram_picture(&diagram, chrome::key_of(Some("mermaid"), source)),
                ),
                mermaid::Drawing::Rendering { .. } => {
                    image::note(theme, "Drawing\u{2026}").into_any_element()
                }
                mermaid::Drawing::Failed => failed(theme),
            }
        }
    }
}

impl RenderOnce for VisualView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let content = body(&self.visual, &self.host, &self.theme, window, cx);
        div()
            .id(gpui::SharedString::from(format!(
                "visual-{}",
                self.visual.id
            )))
            .debug_selector(|| "visual".to_owned())
            .flex()
            .flex_col()
            .items_start()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(self.theme.muted)
                    .child(self.visual.caption.clone()),
            )
            .child(content)
    }
}
