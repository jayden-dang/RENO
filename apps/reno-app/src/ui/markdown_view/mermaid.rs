//! Mermaid diagrams (TRANSCRIPT-10.1, 10.2, 10.3, 10.5, 16.4, 16.9).
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
//! draws it as a plain code block. Flowchart, sequence, class, state, ER, gantt and pie diagrams
//! are built in. A place that is drawn again (a changed source, a new theme) keeps its last
//! drawing on screen until the new one lands.

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
/// The latest drawing of each place, held to stand in while that place redraws; most of these are
/// shared with the cache above, so the byte cap only counts what outlives it.
const PREVIOUS_BYTES: usize = 24 * 1024 * 1024;
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

/// The pie slice colours, in order. [`theme_key`] hashes these too, so a drawing is redone when
/// any of them changes.
fn slice_colors(theme: &Theme) -> [gpui::Hsla; 12] {
    [
        theme.accent,
        theme.warning,
        theme.ansi[4],
        theme.danger,
        theme.ansi[5],
        theme.success,
        theme.ansi[6],
        theme.ansi[12],
        theme.ansi[13],
        theme.ansi[14],
        theme.ansi[10],
        theme.ansi[11],
    ]
}

/// The Mermaid `init` directive that paints a diagram from the theme's tokens: nodes on the
/// raised tone, lines muted, labels in the text colour, and the edge-label chips in the card's
/// own tone so they do not show as grey boxes on a dark card.
pub(crate) fn init_directive(theme: &Theme) -> String {
    let mut vars: serde_json::Map<String, serde_json::Value> = [
        ("fontFamily", FAMILY.into()),
        ("fontSize", "14px".into()),
        ("background", hex(theme.surface).into()),
        ("primaryColor", hex(theme.raised).into()),
        ("primaryTextColor", hex(theme.text).into()),
        ("primaryBorderColor", hex(theme.faint).into()),
        ("secondaryColor", hex(theme.canvas).into()),
        ("secondaryTextColor", hex(theme.text).into()),
        ("secondaryBorderColor", hex(theme.faint).into()),
        ("tertiaryColor", hex(theme.canvas).into()),
        ("tertiaryTextColor", hex(theme.text).into()),
        ("tertiaryBorderColor", hex(theme.faint).into()),
        ("lineColor", hex(theme.muted).into()),
        ("textColor", hex(theme.text).into()),
        ("mainBkg", hex(theme.raised).into()),
        ("nodeBorder", hex(theme.faint).into()),
        ("clusterBkg", hex(theme.canvas).into()),
        ("clusterBorder", hex(theme.faint).into()),
        ("edgeLabelBackground", hex(theme.surface).into()),
        ("titleColor", hex(theme.text).into()),
        ("actorBkg", hex(theme.raised).into()),
        ("actorBorder", hex(theme.faint).into()),
        ("actorTextColor", hex(theme.text).into()),
        ("actorLineColor", hex(theme.faint).into()),
        ("signalColor", hex(theme.muted).into()),
        ("signalTextColor", hex(theme.text).into()),
        ("labelBoxBkgColor", hex(theme.raised).into()),
        ("labelBoxBorderColor", hex(theme.faint).into()),
        ("labelTextColor", hex(theme.text).into()),
        ("loopTextColor", hex(theme.text).into()),
        ("noteBkgColor", hex(theme.raised).into()),
        ("noteBorderColor", hex(theme.faint).into()),
        ("noteTextColor", hex(theme.text).into()),
        ("activationBkgColor", hex(theme.canvas).into()),
        ("activationBorderColor", hex(theme.faint).into()),
        ("sequenceNumberColor", hex(theme.text).into()),
        ("pieTitleTextColor", hex(theme.text).into()),
        ("pieLegendTextColor", hex(theme.text).into()),
        ("pieSectionTextColor", hex(theme.canvas).into()),
        ("pieStrokeColor", hex(theme.surface).into()),
        ("pieOuterStrokeColor", hex(theme.surface).into()),
        ("pieOpacity", "1".into()),
        ("gridColor", hex(theme.faint).into()),
        ("taskBkgColor", hex(theme.raised).into()),
        ("taskBorderColor", hex(theme.faint).into()),
        ("taskTextColor", hex(theme.text).into()),
        ("taskTextOutsideColor", hex(theme.text).into()),
        ("sectionBkgColor", hex(theme.canvas).into()),
        ("altSectionBkgColor", hex(theme.surface).into()),
    ]
    .into_iter()
    .map(|(name, value): (&str, serde_json::Value)| (name.to_owned(), value))
    .collect();
    // Slices need colours that tell apart: the accent, then hues far from it and from each other.
    let slices = slice_colors(theme);
    for (n, color) in slices.into_iter().enumerate() {
        vars.insert(format!("pie{}", n + 1), hex(color).into());
    }
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

/// `svg` with `ink` as the colour that `currentColor` strokes (the gantt's grid lines among
/// them) take: the diagram's own style sheet does not reach them, and unset they are black.
fn with_ink(svg: &str, ink: gpui::Hsla) -> String {
    match svg.split_once("<svg") {
        Some((before, after)) => format!("{before}<svg color=\"{}\"{after}", hex(ink)),
        None => svg.to_owned(),
    }
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
            RenderOutput::Svg(Some(svg)) => Ok(with_ink(svg.svg(), theme.faint)),
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
    /// What it shows and in which colours ([`look_of`]), whatever size it is drawn at.
    look: u64,
}

/// A fingerprint of a diagram's source and theme: two drawings with the same one differ only in
/// the size they were made at.
fn look_of(source: &str, theme: &Theme) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    theme_key(theme).hash(&mut hasher);
    hasher.finish()
}

impl Diagram {
    /// Whether this is a drawing of `source` in `theme`, at whatever size: what stands in for a
    /// drawing being made again at a new size needs no "Rendering\u{2026}" note.
    pub(crate) fn is_look_of(&self, source: &str, theme: &Theme) -> bool {
        self.look == look_of(source, theme)
    }
}

/// Where a diagram is.
#[derive(Clone, Debug)]
pub(crate) enum Drawing {
    /// Being drawn off the main thread; `previous` is what was drawn in its place before.
    Rendering {
        previous: Option<Arc<Diagram>>,
    },
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
    /// The latest drawing per place (see [`drawing`]).
    latest: Lru<u64, Arc<Diagram>>,
}

impl Diagrams {
    fn remember(&mut self, place: u64, diagram: &Arc<Diagram>) {
        if self
            .latest
            .peek(&place)
            .is_some_and(|held| Arc::ptr_eq(held, diagram))
        {
            return;
        }
        self.latest.insert(place, diagram.clone(), diagram.cost());
    }
}

impl Diagram {
    /// The bytes of its pixels.
    fn cost(&self) -> usize {
        let size = self.image.size(0);
        usize::try_from(size.width.0).unwrap_or(0) * usize::try_from(size.height.0).unwrap_or(0) * 4
    }
}

impl gpui::Global for Diagrams {}

impl Default for Diagrams {
    fn default() -> Self {
        Self {
            slots: Lru::new(CACHE_ENTRIES, CACHE_BYTES),
            latest: Lru::new(CACHE_ENTRIES, PREVIOUS_BYTES),
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
    ]
    .into_iter()
    .chain(slice_colors(theme))
    {
        bytes_of(color).hash(&mut hasher);
    }
    hasher.finish()
}

/// The drawing of `source` in `theme` at `scale`: asks for it off the main thread when nobody
/// has, and refreshes the windows when it lands.
///
/// `slot` names the place the diagram is drawn in (a card of a row), which outlives a change of
/// its source: while that place redraws, the drawing it last had comes back as `previous`. With no
/// `slot` the place is the source itself, so a new theme or scale keeps its picture and an edited
/// source does not.
pub(crate) fn drawing(
    source: &str,
    theme: &Theme,
    scale: f32,
    slot: Option<u64>,
    cx: &mut App,
) -> Drawing {
    let place = slot.unwrap_or_else(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    });
    let key: Key = (
        source.to_owned(),
        theme_key(theme),
        (scale * 100.0).round() as u32,
    );
    let diagrams = cx.default_global::<Diagrams>();
    match diagrams.slots.get(&key) {
        Some(Slot::Ready(diagram)) => {
            let diagram = diagram.clone();
            diagrams.remember(place, &diagram);
            return Drawing::Ready(diagram);
        }
        Some(Slot::Rendering(_)) => {
            return Drawing::Rendering {
                previous: diagrams.latest.get(&place).cloned(),
            };
        }
        Some(Slot::Failed) => return Drawing::Failed,
        None => {}
    }
    let look = look_of(source, theme);
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
                    Some(Arc::new(Diagram {
                        image,
                        width: raster.width,
                        height: raster.height,
                        look,
                    }))
                });
                let diagrams = cx.default_global::<Diagrams>();
                // Only while the entry is still ours: a cache that dropped it cancelled this.
                if let Some(Slot::Rendering(done)) = diagrams.slots.remove(&key) {
                    done.detach();
                    match made {
                        Some(diagram) => {
                            diagrams.remember(place, &diagram);
                            let cost = diagram.cost();
                            diagrams.slots.insert(key, Slot::Ready(diagram), cost);
                        }
                        None => diagrams.slots.insert(key, Slot::Failed, 0),
                    }
                }
                super::settled(cx);
            });
        }
    });
    let diagrams = cx.default_global::<Diagrams>();
    diagrams.slots.insert(key, Slot::Rendering(task), 0);
    Drawing::Rendering {
        previous: diagrams.latest.get(&place).cloned(),
    }
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
    fn a_colour_the_drawing_reads_changes_its_cache_key() {
        let base = dark();
        let other = gpui::hsla(0.123, 0.9, 0.5, 1.0);
        type Set = fn(&mut Theme, gpui::Hsla);
        let changes: [(&str, Set); 13] = [
            ("surface", |t, c| t.surface = c),
            ("raised", |t, c| t.raised = c),
            ("canvas", |t, c| t.canvas = c),
            ("text", |t, c| t.text = c),
            ("muted", |t, c| t.muted = c),
            ("faint", |t, c| t.faint = c),
            ("accent", |t, c| t.accent = c),
            ("warning", |t, c| t.warning = c),
            ("danger", |t, c| t.danger = c),
            ("success", |t, c| t.success = c),
            ("ansi[4]", |t, c| t.ansi[4] = c),
            ("ansi[14]", |t, c| t.ansi[14] = c),
            ("ansi[10]", |t, c| t.ansi[10] = c),
        ];
        for (name, change) in changes {
            let mut theme = dark();
            change(&mut theme, other);
            assert_ne!(theme_key(&base), theme_key(&theme), "{name}");
        }
        // Everything a slice is painted from is something the drawing reads.
        let mut theme = dark();
        theme.accent = other;
        assert_ne!(
            render_svg("pie title Pets\n \"Dogs\" : 3\n \"Cats\" : 2\n", &base).unwrap(),
            render_svg("pie title Pets\n \"Dogs\" : 3\n \"Cats\" : 2\n", &theme).unwrap()
        );
    }

    #[test]
    fn a_source_that_does_not_parse_or_is_not_a_built_in_type_is_an_error() {
        for bad in [
            "",
            "flowchart LR\n A -->",
            "not a diagram at all",
            "journey\n  title A day\n  section Work\n    Code: 5: Me",
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
        let first = cx.update(|cx| drawing(FLOW, &theme, 2.0, None, cx));
        assert!(matches!(first, Drawing::Rendering { .. }));
        let again = cx.update(|cx| drawing(FLOW, &theme, 2.0, None, cx));
        assert!(
            matches!(again, Drawing::Rendering { .. }),
            "no second request"
        );
        cx.executor().allow_parking();
        for _ in 0..200 {
            cx.run_until_parked();
            if matches!(
                cx.update(|cx| drawing(FLOW, &theme, 2.0, None, cx)),
                Drawing::Ready(_)
            ) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(matches!(
            cx.update(|cx| drawing(FLOW, &theme, 2.0, None, cx)),
            Drawing::Ready(_)
        ));
        let bad = cx.update(|cx| drawing("nonsense", &theme, 2.0, None, cx));
        assert!(matches!(bad, Drawing::Rendering { .. }));
        for _ in 0..200 {
            cx.run_until_parked();
            if matches!(
                cx.update(|cx| drawing("nonsense", &theme, 2.0, None, cx)),
                Drawing::Failed
            ) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("a bad diagram never settled to Failed");
    }

    /// One row per diagram type: its source and a label that must come out in the drawing.
    const TYPES: [(&str, &str, &str); 5] = [
        (
            "class",
            "classDiagram\n  class Animal {\n    +String name\n    +eat()\n  }\n  Animal <|-- Dog\n",
            "Animal",
        ),
        (
            "state",
            "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Busy : start\n  Busy --> [*]\n",
            "Busy",
        ),
        (
            "er",
            "erDiagram\n  CUSTOMER ||--o{ ORDER : places\n  ORDER ||--|{ LINE : contains\n",
            "CUSTOMER",
        ),
        (
            "gantt",
            "gantt\n  title Plan\n  dateFormat YYYY-MM-DD\n  section Build\n  Write it :a1, 2024-01-01, 3d\n  Test it :after a1, 2d\n",
            "Write it",
        ),
        (
            "pie",
            "pie title Pets\n  \"Dogs\" : 3\n  \"Cats\" : 2\n  \"Fish\" : 1\n",
            "Dogs",
        ),
    ];

    #[test]
    fn class_state_er_gantt_and_pie_each_draw() {
        let theme = dark();
        for (name, source, label) in TYPES {
            let svg = render_svg(source, &theme).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                !svg.contains("foreignObject"),
                "{name}: resvg cannot draw those"
            );
            assert!(svg.contains(label), "{name}: the label {label:?} is drawn");
            assert!(
                svg.contains(&format!("color=\"{}\"", hex(theme.faint))),
                "{name}: strokes that follow the text colour take the theme's"
            );
            let raster = rasterize(&svg, 2.0).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(raster.width > 50.0 && raster.height > 30.0, "{name}");
            let pixmap = tiny_skia::Pixmap::decode_png(&raster.png).unwrap();
            let inked = pixmap.pixels().iter().filter(|p| p.alpha() > 0).count();
            assert!(inked > 200, "{name}: something is painted, {inked} pixels");
        }
    }

    #[test]
    fn every_diagram_type_is_painted_from_the_theme_so_dark_and_light_differ() {
        let light = Theme::for_appearance(Appearance::Light);
        for (name, source, _) in TYPES {
            assert_ne!(
                render_svg(source, &dark()).unwrap(),
                render_svg(source, &light).unwrap(),
                "{name}"
            );
        }
    }

    async fn settled_drawing(
        cx: &mut gpui::TestAppContext,
        source: &str,
        theme: &Theme,
        slot: Option<u64>,
    ) -> Drawing {
        cx.executor().allow_parking();
        for _ in 0..200 {
            cx.run_until_parked();
            let now = cx.update(|cx| drawing(source, theme, 2.0, slot, cx));
            if !matches!(now, Drawing::Rendering { .. }) {
                return now;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("a diagram never settled");
    }

    #[gpui::test]
    async fn a_changed_source_keeps_the_previous_drawing_until_the_new_one_lands(
        cx: &mut gpui::TestAppContext,
    ) {
        let theme = dark();
        let slot = Some(7);
        let Drawing::Ready(first) = settled_drawing(cx, FLOW, &theme, slot).await else {
            panic!("the first source draws");
        };
        // The source changes in the same place: the old drawing is held while the new draws.
        let Drawing::Rendering { previous } = cx.update(|cx| drawing(SEQ, &theme, 2.0, slot, cx))
        else {
            panic!("a new source is drawn off the main thread");
        };
        let held = previous.expect("the previous drawing is kept");
        assert!(Arc::ptr_eq(&held, &first));
        let Drawing::Ready(second) = settled_drawing(cx, SEQ, &theme, slot).await else {
            panic!("the new source draws");
        };
        assert!(!Arc::ptr_eq(&second, &first));
        // A third change holds the second, not the first.
        let Drawing::Rendering { previous } = cx.update(|cx| {
            drawing(
                FLOW,
                &Theme::for_appearance(Appearance::Light),
                2.0,
                slot,
                cx,
            )
        }) else {
            panic!("a new theme is drawn again");
        };
        assert!(Arc::ptr_eq(
            &previous.expect("kept across a theme change"),
            &second
        ));
        // A diagram in another place has no previous drawing to borrow.
        let Drawing::Rendering { previous } =
            cx.update(|cx| drawing("flowchart LR\n  X --> Y\n", &theme, 2.0, Some(8), cx))
        else {
            panic!("a new diagram is drawn off the main thread");
        };
        assert!(previous.is_none());
        // With no place named, a new theme of the same source keeps its drawing.
        let Drawing::Ready(plain) = settled_drawing(cx, SEQ, &theme, None).await else {
            panic!("it draws");
        };
        let Drawing::Rendering { previous } = cx.update(|cx| {
            drawing(
                SEQ,
                &Theme::for_appearance(Appearance::Light),
                2.0,
                None,
                cx,
            )
        }) else {
            panic!("a new theme is drawn again");
        };
        assert!(Arc::ptr_eq(&previous.expect("kept"), &plain));
    }

    #[gpui::test]
    fn the_diagram_cache_is_bounded(cx: &mut gpui::TestAppContext) {
        let theme = dark();
        cx.update(|cx| {
            for n in 0..(CACHE_ENTRIES + 10) {
                drawing(&format!("flowchart LR\n A{n} --> B"), &theme, 2.0, None, cx);
                assert!(cached(cx) <= CACHE_ENTRIES);
            }
        });
    }
}
