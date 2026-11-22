//! Typeset math (TRANSCRIPT-9.1 to 9.6, 16.4): TeX source into a picture, with its metrics.
//!
//! `ratex` parses and lays the formula out in em units and writes it as SVG paths with the KaTeX
//! fonts embedded; `resvg` rasterises that at the window's scale, in the theme's text colour. The
//! layout's height and depth (em, times the font size) say where the baseline sits, so inline
//! math can be aligned on the text's. A formula that does not typeset is remembered as such
//! (TRANSCRIPT-9.3): the caller draws its source in code style. Results are cached in a bounded
//! LRU keyed by (source, display, size, colour, scale).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Hsla, Rgba};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_types::color::Color;
use ratex_types::math_style::MathStyle;

use super::lru::Lru;

/// Formulas longer than this are shown as source: typesetting costs grow with the input.
const MAX_SOURCE_BYTES: usize = 4 * 1024;
/// The largest picture rasterised, in device pixels on a side.
const MAX_SIDE_PX: f32 = 4096.0;
/// Formulas kept, and the pixel bytes they may hold together.
const CACHE_ENTRIES: usize = 256;
const CACHE_BYTES: usize = 24 * 1024 * 1024;

/// A formula as a picture, in logical pixels.
#[derive(Clone, Debug)]
pub(crate) struct Typeset {
    pub(crate) image: Arc<gpui::RenderImage>,
    pub(crate) width: f32,
    /// From the top of the picture to the baseline.
    pub(crate) above: f32,
    /// From the baseline to the bottom of the picture.
    pub(crate) below: f32,
}

impl Typeset {
    pub(crate) fn height(&self) -> f32 {
        self.above + self.below
    }
}

/// A formula rasterised, before it is made a GPUI image.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Raster {
    pub(crate) png: Vec<u8>,
    pub(crate) width: f32,
    pub(crate) above: f32,
    pub(crate) below: f32,
}

/// Typesets `tex` at `size` logical pixels per em, in `color`, for a display scale of `scale`.
/// `Err` when it does not parse, is too large, or would be too big a picture.
pub(crate) fn rasterize(
    tex: &str,
    display: bool,
    size: f32,
    color: [u8; 4],
    scale: f32,
) -> Result<Raster, String> {
    if tex.trim().is_empty() || tex.len() > MAX_SOURCE_BYTES {
        return Err("empty or too long".to_owned());
    }
    // The parser and layout are third-party code fed model output: a panic is an error, not a
    // crash (as for the markdown parser).
    std::panic::catch_unwind(|| draw(tex, display, size, color, scale))
        .map_err(|_| "the typesetter panicked".to_owned())?
}

fn draw(tex: &str, display: bool, size: f32, color: [u8; 4], scale: f32) -> Result<Raster, String> {
    let nodes = ratex_parser::parser::parse(tex).map_err(|e| e.to_string())?;
    let mut options = LayoutOptions::default().with_style(if display {
        MathStyle::Display
    } else {
        MathStyle::Text
    });
    options.color = Color::new(
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        f32::from(color[3]) / 255.0,
    );
    let list = to_display_list(&layout(&nodes, &options));
    let (em, scale) = (f64::from(size), f64::from(scale));
    let (width, above, below) = (list.width * em, list.height * em, list.depth * em);
    if !(width.is_finite() && above.is_finite() && below.is_finite())
        || width <= 0.0
        || (width + 1.0) * scale > f64::from(MAX_SIDE_PX)
        || (above + below + 1.0) * scale > f64::from(MAX_SIDE_PX)
    {
        return Err("empty or too large".to_owned());
    }
    let svg = ratex_svg::render_to_svg(
        &list,
        &ratex_svg::SvgOptions {
            font_size: em * scale,
            padding: 0.0,
            stroke_width: 1.0 * scale,
            embed_glyphs: true,
            font_dir: String::new(),
        },
    );
    // The root's width and height come in points; usvg would scale them by 96/72. They are pixels.
    let svg = match svg.find('>') {
        Some(end) => format!("{}{}", svg[..end].replace("pt\"", "\""), &svg[end..]),
        None => svg,
    };
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let (w, h) = (
        (tree.size().width().ceil() as u32).max(1),
        (tree.size().height().ceil() as u32).max(1),
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("no pixmap")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let png = pixmap.encode_png().map_err(|e| e.to_string())?;
    Ok(Raster {
        png,
        width: (f64::from(w) / scale) as f32,
        above: above as f32,
        below: below as f32,
    })
}

/// `Rgba` bytes of a theme colour.
pub(crate) fn bytes_of(color: Hsla) -> [u8; 4] {
    let rgba = Rgba::from(color);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(rgba.r), byte(rgba.g), byte(rgba.b), byte(rgba.a)]
}

type Key = (String, bool, u32, [u8; 4], u32);

thread_local! {
    static CACHE: RefCell<Lru<Key, Option<Rc<Typeset>>>> =
        RefCell::new(Lru::new(CACHE_ENTRIES, CACHE_BYTES));
}

/// The formula as a picture, or `None` when it does not typeset (cached too, so a bad formula is
/// not parsed again on every frame).
pub(crate) fn typeset(
    tex: &str,
    display: bool,
    size: f32,
    color: Hsla,
    scale: f32,
    cx: &App,
) -> Option<Rc<Typeset>> {
    let color = bytes_of(color);
    let key: Key = (
        tex.to_owned(),
        display,
        (size * 100.0).round() as u32,
        color,
        (scale * 100.0).round() as u32,
    );
    if let Some(hit) = CACHE.with(|cache| cache.borrow_mut().get(&key).cloned()) {
        return hit;
    }
    let made = rasterize(tex, display, size, color, scale)
        .ok()
        .and_then(|raster| {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, raster.png.clone())
                .to_image_data(cx.svg_renderer())
                .ok()?;
            Some(Rc::new(Typeset {
                image,
                width: raster.width,
                above: raster.above,
                below: raster.below,
            }))
        });
    let cost = made.as_ref().map_or(0, |t| {
        let size = t.image.size(0);
        usize::try_from(size.width.0).unwrap_or(0) * usize::try_from(size.height.0).unwrap_or(0) * 4
    });
    CACHE.with(|cache| cache.borrow_mut().insert(key, made.clone(), cost));
    made
}

/// Entries held, for the bound's test.
#[cfg(test)]
pub(crate) fn cached() -> usize {
    CACHE.with(|cache| cache.borrow().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn display_math_has_the_layouts_height_and_depth_in_em_times_the_size() {
        let raster = rasterize(r"\int_0^1 x^2\,dx = \tfrac13", true, 20.0, WHITE, 2.0).unwrap();
        // The spike's metrics for this formula: 5.7202 em wide, 1.5651 high, 0.9108 deep.
        assert!(
            (raster.width - 5.7202 * 20.0).abs() < 1.5,
            "{}",
            raster.width
        );
        assert!(
            (raster.above - 1.5651 * 20.0).abs() < 0.5,
            "{}",
            raster.above
        );
        assert!(
            (raster.below - 0.9108 * 20.0).abs() < 0.5,
            "{}",
            raster.below
        );
        assert_eq!(&raster.png[1..4], b"PNG");
    }

    #[test]
    fn inline_math_sits_on_the_baseline_with_no_depth() {
        let raster = rasterize("x^2", false, 15.0, WHITE, 1.0).unwrap();
        assert!((raster.above - 0.8141 * 15.0).abs() < 0.5);
        assert!(raster.below.abs() < 0.5);
    }

    #[test]
    fn the_size_scales_the_metrics() {
        let small = rasterize("x^2", false, 10.0, WHITE, 1.0).unwrap();
        let big = rasterize("x^2", false, 15.0, WHITE, 1.0).unwrap();
        assert!((big.width / small.width - 1.5).abs() < 0.1);
    }

    #[test]
    fn the_colour_is_the_one_asked_for() {
        let raster = rasterize("x", false, 20.0, [255, 0, 0, 255], 1.0).unwrap();
        let decoded = decode_rgba(&raster.png);
        assert!(
            decoded
                .iter()
                .any(|px| px[3] > 200 && px[0] > 200 && px[1] < 40 && px[2] < 40),
            "no red pixel"
        );
        assert!(
            !decoded
                .iter()
                .any(|px| px[3] > 200 && px[0] < 40 && px[1] < 40 && px[2] < 40),
            "black pixels remain"
        );
    }

    fn decode_rgba(png: &[u8]) -> Vec<[u8; 4]> {
        let pixmap = tiny_skia::Pixmap::decode_png(png).unwrap();
        pixmap
            .pixels()
            .iter()
            .map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect()
    }

    #[test]
    fn a_formula_that_does_not_typeset_is_an_error_not_a_panic() {
        for bad in [
            r"\frac{1",
            r"\notacommand{x}",
            "",
            "   ",
            r"\begin{matrix} a",
        ] {
            assert!(rasterize(bad, false, 15.0, WHITE, 1.0).is_err(), "{bad:?}");
        }
        assert!(rasterize(&"x+".repeat(MAX_SOURCE_BYTES), true, 15.0, WHITE, 1.0).is_err());
    }

    #[test]
    fn a_picture_too_large_is_refused() {
        let wide = "x".repeat(900);
        assert!(rasterize(&wide, false, 40.0, WHITE, 3.0).is_err());
    }

    #[gpui::test]
    fn failures_and_successes_are_cached_and_the_cache_is_bounded(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let color = gpui::black();
            assert!(typeset(r"\frac{1", false, 15.0, color, 1.0, cx).is_none());
            let before = cached();
            assert!(typeset(r"\frac{1", false, 15.0, color, 1.0, cx).is_none());
            assert_eq!(cached(), before, "the failure is held, not parsed again");
            let first = typeset("a+b", false, 15.0, color, 1.0, cx).unwrap();
            let again = typeset("a+b", false, 15.0, color, 1.0, cx).unwrap();
            assert!(Rc::ptr_eq(&first, &again));
            for n in 0..(CACHE_ENTRIES + 40) {
                typeset(&format!("x_{{{n}}}"), false, 15.0, color, 1.0, cx);
                assert!(cached() <= CACHE_ENTRIES);
            }
        });
    }
}
