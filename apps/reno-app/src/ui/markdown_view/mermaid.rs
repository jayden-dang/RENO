//! Mermaid diagrams (TRANSCRIPT-10.1, 10.3, 10.5, 16.4, 16.9).
//!
//! `merman` parses, lays out and writes the diagram as SVG in a form `resvg` draws faithfully; the
//! app rasterises it. Two pieces are the app's own:
//!
//! - a [`HostTextMeasurer`] over the app's UI font (Geist, read with `ttf-parser`), so a label's
//!   box matches the letters drawn in it (the built-in metrics are for another font and make edge
//!   labels collide with their lines);
//! - the colours, written as a Mermaid `init` directive from the theme's tokens, so the diagram
//!   reads in the dark theme and the light one. A diagram is drawn again when the theme changes
//!   because the theme is part of its cache key.
//!
//! The work runs off the main thread on a task the cache owns (ARCH-11): a repaint finds the
//! entry and does not ask again, and an entry the cache drops cancels its task. A source that
//! does not parse, or whose type is not built in, comes back as [`Drawing::Failed`] and the view
//! draws it as a plain code block. Only flowchart and sequence diagrams are built in.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use gpui::{App, Task};
use merman::svg::{
    HostMeasurementResult, HostTextMeasurement, HostTextMeasurementRequest, HostTextMeasurer,
    MeasurementProfileId, SvgPipeline, TextMeasurementOperation, TextMeasurementPhase,
    TextMeasurementPolicy, TextMeasurementProfileIdentity, TextMetrics,
};
use merman::{OperationControl, RenderOutput, RenderRequest, Renderer, SvgRequest};

use super::lru::Lru;
use super::math::bytes_of;
use crate::ui::theme::Theme;

/// Diagram sources longer than this are shown as code.
const MAX_SOURCE_BYTES: usize = 16 * 1024;
/// The most device pixels on a side a diagram is rasterised to; a bigger one is drawn smaller.
const MAX_SIDE_PX: f32 = 4096.0;
/// Diagrams kept, and the pixel bytes they may hold together (ARCH-12).
const CACHE_ENTRIES: usize = 32;
const CACHE_BYTES: usize = 48 * 1024 * 1024;
/// The font family name of the app's UI font, as the SVG names it.
const FAMILY: &str = "Geist";

/// Measures label text in the app's UI font.
struct Measurer {
    face: ttf_parser::Face<'static>,
}

impl Measurer {
    fn new(font: &'static [u8]) -> Option<Self> {
        Some(Self {
            face: ttf_parser::Face::parse(font, 0).ok()?,
        })
    }

    /// The advance width of `text` at `size` pixels.
    fn width(&self, text: &str, size: f64) -> f64 {
        let upem = f64::from(self.face.units_per_em()).max(1.0);
        let units: f64 = text
            .chars()
            .filter_map(|c| self.face.glyph_index(c))
            .map(|g| f64::from(self.face.glyph_hor_advance(g).unwrap_or(0)))
            .sum();
        units * size / upem
    }

    /// `text` broken at spaces into lines no wider than `max` (a word wider than it keeps its
    /// own line): how many lines, and the widest.
    fn wrap(&self, text: &str, size: f64, max: Option<f64>) -> (usize, f64) {
        let Some(max) = max.filter(|m| *m > 0.0) else {
            return (1, self.width(text, size));
        };
        let space = self.width(" ", size);
        let (mut lines, mut widest, mut line) = (1, 0.0_f64, 0.0_f64);
        for word in text.split_whitespace() {
            let w = self.width(word, size);
            if line > 0.0 && line + space + w > max {
                widest = widest.max(line);
                lines += 1;
                line = w;
            } else {
                line = if line > 0.0 { line + space + w } else { w };
            }
        }
        (lines, widest.max(line))
    }
}

impl HostTextMeasurer for Measurer {
    fn measure(&self, request: HostTextMeasurementRequest<'_>) -> HostMeasurementResult {
        let size = request.style.font_size;
        let width = self.width(request.text, size);
        use TextMeasurementOperation as Op;
        match request.operation {
            Op::Measure => Ok(Some(HostTextMeasurement::Metrics(TextMetrics {
                width,
                height: size * 1.5,
                line_count: 1,
            }))),
            Op::Wrapped => {
                let (lines, widest) = self.wrap(request.text, size, request.max_width);
                Ok(Some(HostTextMeasurement::Metrics(TextMetrics {
                    width: widest,
                    height: size * 1.5 * lines as f64,
                    line_count: lines,
                })))
            }
            Op::ComputedLength
            | Op::SimpleBBoxWidth
            | Op::RawBBoxWidth
            | Op::TspanBBoxWidth
            | Op::BoundingClientRectWidth
            | Op::CanvasMeasureTextWidth
            | Op::WrapProbeBBoxWidth => Ok(Some(HostTextMeasurement::Length(width))),
            Op::RawBBoxHeight => Ok(Some(HostTextMeasurement::Length(size * 1.2))),
            // The rest the built-in metrics answer.
            _ => Ok(None),
        }
    }
}

fn measurer() -> Result<Arc<Measurer>, String> {
    static MEASURER: OnceLock<Option<Arc<Measurer>>> = OnceLock::new();
    MEASURER
        .get_or_init(|| Measurer::new(crate::app::GEIST_REGULAR).map(Arc::new))
        .clone()
        .ok_or_else(|| "the UI font did not load".to_owned())
}

/// The fonts the rasteriser draws labels with: the app's UI font only.
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_font_data(crate::app::GEIST_REGULAR.to_vec());
            for set in [
                usvg::fontdb::Database::set_sans_serif_family,
                usvg::fontdb::Database::set_serif_family,
                usvg::fontdb::Database::set_monospace_family,
                usvg::fontdb::Database::set_cursive_family,
                usvg::fontdb::Database::set_fantasy_family,
            ] {
                set(&mut db, FAMILY);
            }
            Arc::new(db)
        })
        .clone()
}

fn hex(color: gpui::Hsla) -> String {
    let [r, g, b, _] = bytes_of(color);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The Mermaid `init` directive that paints a diagram from the theme's tokens: nodes on the
/// raised tone, lines muted, labels in the text colour, and the edge-label chips in the card's
/// own tone so they do not show as grey boxes on a dark card.
pub(crate) fn init_directive(theme: &Theme) -> String {
    let vars = serde_json::json!({
        "fontFamily": FAMILY,
        "fontSize": "14px",
        "background": hex(theme.surface),
        "primaryColor": hex(theme.raised),
        "primaryTextColor": hex(theme.text),
        "primaryBorderColor": hex(theme.faint),
        "secondaryColor": hex(theme.canvas),
        "secondaryTextColor": hex(theme.text),
        "secondaryBorderColor": hex(theme.faint),
        "tertiaryColor": hex(theme.canvas),
        "tertiaryTextColor": hex(theme.text),
        "tertiaryBorderColor": hex(theme.faint),
        "lineColor": hex(theme.muted),
        "textColor": hex(theme.text),
        "mainBkg": hex(theme.raised),
        "nodeBorder": hex(theme.faint),
        "clusterBkg": hex(theme.canvas),
        "clusterBorder": hex(theme.faint),
        "edgeLabelBackground": hex(theme.surface),
        "titleColor": hex(theme.text),
        "actorBkg": hex(theme.raised),
        "actorBorder": hex(theme.faint),
        "actorTextColor": hex(theme.text),
        "actorLineColor": hex(theme.faint),
        "signalColor": hex(theme.muted),
        "signalTextColor": hex(theme.text),
        "labelBoxBkgColor": hex(theme.raised),
        "labelBoxBorderColor": hex(theme.faint),
        "labelTextColor": hex(theme.text),
        "loopTextColor": hex(theme.text),
        "noteBkgColor": hex(theme.raised),
        "noteBorderColor": hex(theme.faint),
        "noteTextColor": hex(theme.text),
        "activationBkgColor": hex(theme.canvas),
        "activationBorderColor": hex(theme.faint),
        "sequenceNumberColor": hex(theme.text),
    });
    // Tight spacing: a chain of five nodes has to fit the transcript's column without shrinking
    // its labels to specks.
    let flowchart = serde_json::json!({
        "nodeSpacing": 24,
        "rankSpacing": 28,
        "padding": 8,
        "diagramPadding": 8,
    });
    format!(
        "%%{{init: {}}}%%\n",
        serde_json::json!({ "theme": "base", "themeVariables": vars, "flowchart": flowchart })
    )
}

/// `source` with the theme directive in front of the diagram: after a leading `---` front matter
/// block when there is one (the directive must not come before it).
fn with_directive(source: &str, directive: &str) -> String {
    let body = source.trim_start();
    if let Some(rest) = body.strip_prefix("---")
        && rest.starts_with(['\n', '\r'])
    {
        let mut at = 0;
        for line in rest.split_inclusive('\n') {
            at += line.len();
            if line.trim_end() == "---" && at > 4 {
                let (front, diagram) = rest.split_at(at);
                return format!("---{front}{directive}{diagram}");
            }
        }
    }
    format!("{directive}{source}")
}

/// The diagram's SVG, with the theme's colours and labels measured in the UI font.
pub(crate) fn render_svg(source: &str, theme: &Theme) -> Result<String, String> {
    if source.trim().is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err("empty or too long".to_owned());
    }
    let measurer = measurer()?;
    let themed = with_directive(source, &init_directive(theme));
    // The layout engine is third-party code fed model output: a panic is a failed diagram.
    std::panic::catch_unwind(|| {
        let identity = TextMeasurementProfileIdentity::new(
            MeasurementProfileId::new("reno-geist").map_err(|e| e.to_string())?,
            "1",
        )
        .map_err(|e| e.to_string())?;
        let mut request = SvgRequest {
            pipeline: Some(SvgPipeline::resvg_safe()),
            ..SvgRequest::default()
        };
        request.environment =
            request
                .environment
                .with_text_measurement_policy(TextMeasurementPolicy::host_display(
                    identity,
                    measurer,
                    TextMeasurementPhase::ALL,
                ));
        let output = Renderer::new()
            .render(RenderRequest::svg(
                &themed,
                OperationControl::new(),
                request,
            ))
            .map_err(|e| e.to_string())?;
        match output {
            RenderOutput::Svg(Some(svg)) => Ok(svg.svg().to_string()),
            _ => Err("not a drawable diagram".to_owned()),
        }
    })
    .map_err(|_| "the diagram engine panicked".to_owned())?
}

/// A diagram rasterised, as PNG, with its logical size in pixels.
#[derive(Debug)]
pub(crate) struct Raster {
    pub(crate) png: Vec<u8>,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

/// An image resolver that reads no file: the default one reads whatever path an `<image href>`
/// names, so a diagram source could show a file from outside the worktree (TRANSCRIPT-16.7).
/// `data:` images still resolve.
pub(crate) fn no_files() -> usvg::ImageHrefResolver<'static> {
    usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(|_, _| None),
    }
}

/// The SVG drawn into pixels at `scale` (smaller when the picture would pass the size cap).
pub(crate) fn rasterize(svg: &str, scale: f32) -> Result<Raster, String> {
    let options = usvg::Options {
        fontdb: fonts(),
        font_family: FAMILY.to_owned(),
        image_href_resolver: no_files(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
        return Err("an empty diagram".to_owned());
    }
    let scale = scale.min(MAX_SIDE_PX / width.max(height)).max(0.01);
    let (w, h) = (
        (width * scale).ceil().max(1.0) as u32,
        (height * scale).ceil().max(1.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("no pixmap")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Raster {
        png: pixmap.encode_png().map_err(|e| e.to_string())?,
        width,
        height,
    })
}

/// A diagram as a picture, in logical pixels.
#[derive(Clone, Debug)]
pub(crate) struct Diagram {
    pub(crate) image: Arc<gpui::RenderImage>,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

/// Where a diagram is.
#[derive(Clone, Debug)]
pub(crate) enum Drawing {
    /// Being drawn off the main thread.
    Rendering,
    Ready(Arc<Diagram>),
    /// Did not parse, is not a supported type, or is too large.
    Failed,
}

enum Slot {
    Rendering(Task<()>),
    Ready(Arc<Diagram>),
    Failed,
}

/// (source, theme, scale).
type Key = (String, u64, u32);

struct Diagrams {
    slots: Lru<Key, Slot>,
}

impl gpui::Global for Diagrams {}

impl Default for Diagrams {
    fn default() -> Self {
        Self {
            slots: Lru::new(CACHE_ENTRIES, CACHE_BYTES),
        }
    }
}

/// A fingerprint of everything in the theme a diagram is drawn from.
pub(crate) fn theme_key(theme: &Theme) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    theme.appearance.is_dark().hash(&mut hasher);
    for color in [
        theme.surface,
        theme.raised,
        theme.canvas,
        theme.text,
        theme.muted,
        theme.faint,
    ] {
        bytes_of(color).hash(&mut hasher);
    }
    hasher.finish()
}

/// The drawing of `source` in `theme` at `scale`: asks for it off the main thread when nobody
/// has, and refreshes the windows when it lands.
pub(crate) fn drawing(source: &str, theme: &Theme, scale: f32, cx: &mut App) -> Drawing {
    let key: Key = (
        source.to_owned(),
        theme_key(theme),
        (scale * 100.0).round() as u32,
    );
    if let Some(slot) = cx.default_global::<Diagrams>().slots.get(&key) {
        return match slot {
            Slot::Rendering(_) => Drawing::Rendering,
            Slot::Ready(diagram) => Drawing::Ready(diagram.clone()),
            Slot::Failed => Drawing::Failed,
        };
    }
    let (text, theme) = (source.to_owned(), *theme);
    let task = cx.spawn({
        let key = key.clone();
        async move |cx| {
            let drawn = cx
                .background_executor()
                .spawn(
                    async move { render_svg(&text, &theme).and_then(|svg| rasterize(&svg, scale)) },
                )
                .await;
            cx.update(|cx| {
                let made = drawn.ok().and_then(|raster| {
                    let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, raster.png)
                        .to_image_data(cx.svg_renderer())
                        .ok()?;
                    Some(Diagram {
                        image,
                        width: raster.width,
                        height: raster.height,
                    })
                });
                let diagrams = cx.default_global::<Diagrams>();
                // Only while the entry is still ours: a cache that dropped it cancelled this.
                if let Some(Slot::Rendering(done)) = diagrams.slots.remove(&key) {
                    done.detach();
                    match made {
                        Some(diagram) => {
                            let size = diagram.image.size(0);
                            let cost = usize::try_from(size.width.0).unwrap_or(0)
                                * usize::try_from(size.height.0).unwrap_or(0)
                                * 4;
                            diagrams
                                .slots
                                .insert(key, Slot::Ready(Arc::new(diagram)), cost);
                        }
                        None => diagrams.slots.insert(key, Slot::Failed, 0),
                    }
                }
                super::settled(cx);
            });
        }
    });
    cx.default_global::<Diagrams>()
        .slots
        .insert(key, Slot::Rendering(task), 0);
    Drawing::Rendering
}

/// Entries held, for the bound's test.
#[cfg(test)]
pub(crate) fn cached(cx: &mut App) -> usize {
    cx.default_global::<Diagrams>().slots.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Appearance;

    const FLOW: &str =
        "flowchart LR\n  A[Client] -->|hello v2| B[Handshake]\n  B -->|offer| C{Agent}\n";
    const SEQ: &str = "sequenceDiagram\n  User->>Agent: ask\n  Agent-->>User: answer\n";

    fn dark() -> Theme {
        Theme::for_appearance(Appearance::Dark)
    }

    #[test]
    fn the_measurer_reads_the_app_font() {
        let m = measurer().unwrap();
        let w = m.width("Handshake", 14.0);
        assert!(w > 40.0 && w < 120.0, "{w}");
        assert!(m.width("WWWW", 14.0) > m.width("iiii", 14.0));
        let (lines, widest) = m.wrap("one two three four five six", 14.0, Some(60.0));
        assert!(lines > 1 && widest <= 60.0 + 1.0, "{lines} {widest}");
        assert_eq!(m.wrap("one two", 14.0, None).0, 1);
    }

    #[test]
    fn a_flowchart_and_a_sequence_diagram_draw_with_no_foreign_objects() {
        for source in [FLOW, SEQ] {
            let svg = render_svg(source, &dark()).unwrap();
            assert!(!svg.contains("foreignObject"), "resvg cannot draw those");
            let raster = rasterize(&svg, 2.0).unwrap();
            assert!(raster.width > 50.0 && raster.height > 30.0);
            assert_eq!(&raster.png[1..4], b"PNG");
        }
    }

    #[test]
    fn the_diagram_is_painted_from_the_theme_so_dark_and_light_differ() {
        let dark_svg = render_svg(FLOW, &dark()).unwrap();
        let light = Theme::for_appearance(Appearance::Light);
        let light_svg = render_svg(FLOW, &light).unwrap();
        assert_ne!(dark_svg, light_svg);
        assert!(dark_svg.to_lowercase().contains(&hex(dark().raised)));
        assert!(light_svg.to_lowercase().contains(&hex(light.raised)));
        assert_ne!(theme_key(&dark()), theme_key(&light));
        // The edge-label chip is the card's own tone, not the default grey.
        assert!(dark_svg.to_lowercase().contains(&hex(dark().surface)));
    }

    #[test]
    fn a_source_that_does_not_parse_or_is_not_a_built_in_type_is_an_error() {
        for bad in [
            "",
            "flowchart LR\n A -->",
            "not a diagram at all",
            "pie title Pets\n \"Dogs\" : 3",
        ] {
            assert!(render_svg(bad, &dark()).is_err(), "{bad:?}");
        }
        assert!(
            render_svg(
                &format!("flowchart LR\n{}", "A-->B\n".repeat(4000)),
                &dark()
            )
            .is_err()
        );
    }

    #[test]
    fn an_svg_cannot_make_the_rasteriser_read_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.svg");
        std::fs::write(
            &secret,
            "<svg xmlns='http://www.w3.org/2000/svg' width='50' height='50'><rect width='50' height='50' fill='#ff0000'/></svg>",
        )
        .unwrap();
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink' width='50' height='50'><image width='50' height='50' xlink:href='{}'/></svg>",
            secret.display()
        );
        let raster = rasterize(&svg, 1.0).unwrap();
        let pixmap = tiny_skia::Pixmap::decode_png(&raster.png).unwrap();
        assert!(
            pixmap.pixels().iter().all(|p| p.alpha() == 0),
            "the file's red was painted"
        );
        // The same svg does paint through the default resolver, so the test can fail.
        let options = usvg::Options::default();
        let tree = usvg::Tree::from_str(&svg, &options).unwrap();
        let mut open = tiny_skia::Pixmap::new(50, 50).unwrap();
        resvg::render(&tree, tiny_skia::Transform::identity(), &mut open.as_mut());
        assert!(open.pixels().iter().any(|p| p.alpha() > 0));
    }

    #[test]
    fn a_source_with_front_matter_still_draws() {
        let source = "---\ntitle: Demo\n---\nflowchart LR\n  A[One] --> B[Two]\n";
        assert!(render_svg(source, &dark()).is_ok());
    }

    #[test]
    fn a_huge_diagram_is_drawn_smaller_not_refused() {
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='9000' height='100'><rect width='9000' height='100' fill='red'/></svg>";
        let raster = rasterize(svg, 2.0).unwrap();
        assert!(
            (raster.width - 9000.0).abs() < 1.0,
            "the logical size is kept"
        );
        let pixmap = tiny_skia::Pixmap::decode_png(&raster.png).unwrap();
        assert!(pixmap.width() as f32 <= MAX_SIDE_PX + 1.0);
    }

    #[gpui::test]
    async fn a_diagram_is_drawn_off_the_main_thread_and_then_found(cx: &mut gpui::TestAppContext) {
        let theme = dark();
        let first = cx.update(|cx| drawing(FLOW, &theme, 2.0, cx));
        assert!(matches!(first, Drawing::Rendering));
        let again = cx.update(|cx| drawing(FLOW, &theme, 2.0, cx));
        assert!(matches!(again, Drawing::Rendering), "no second request");
        cx.executor().allow_parking();
        for _ in 0..200 {
            cx.run_until_parked();
            if matches!(
                cx.update(|cx| drawing(FLOW, &theme, 2.0, cx)),
                Drawing::Ready(_)
            ) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(matches!(
            cx.update(|cx| drawing(FLOW, &theme, 2.0, cx)),
            Drawing::Ready(_)
        ));
        let bad = cx.update(|cx| drawing("nonsense", &theme, 2.0, cx));
        assert!(matches!(bad, Drawing::Rendering));
        for _ in 0..200 {
            cx.run_until_parked();
            if matches!(
                cx.update(|cx| drawing("nonsense", &theme, 2.0, cx)),
                Drawing::Failed
            ) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("a bad diagram never settled to Failed");
    }

    #[gpui::test]
    fn the_diagram_cache_is_bounded(cx: &mut gpui::TestAppContext) {
        let theme = dark();
        cx.update(|cx| {
            for n in 0..(CACHE_ENTRIES + 10) {
                drawing(&format!("flowchart LR\n A{n} --> B"), &theme, 2.0, cx);
                assert!(cached(cx) <= CACHE_ENTRIES);
            }
        });
    }
}
