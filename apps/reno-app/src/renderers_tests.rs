//! ROAD-33, the transcript's renderers drawn in a real (test) window: code-block chrome and
//! Copy, math, diagrams, images and links, and the lock that nothing in a reply reaches the
//! network. The pure parts (cache bounds, the typed image source, the link policy, the settings
//! keys) have their unit tests beside their code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use gpui::http_client::{AsyncBody, HttpClient, Response, Url, http::HeaderValue};
use gpui::{
    AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
    Window, div, prelude::*, px,
};
use reno_client::fake::FakeConfig;
use reno_client::fake::transcript::{RENDER_REPLY, RENDER_RUN};
use reno_client::visuals::{Visual, VisualImage, VisualKind};
use reno_markdown::BlockTree;

use crate::links;
use crate::store::Store;
use crate::tests::{
    click, draw, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until,
};
use crate::ui::markdown_view::link::{Action, FileLinks, FileTarget, LinkPolicy};
use crate::ui::markdown_view::visual::visual_view;
use crate::ui::markdown_view::{self, Host, Streaming};
use crate::ui::theme::{Appearance, Theme};
use crate::vm::capabilities::Capabilities;

/// Counts every request the app's HTTP client is asked to make, and answers none.
struct Counting(Arc<AtomicUsize>);

impl HttpClient for Counting {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        _req: gpui::http_client::http::Request<AsyncBody>,
    ) -> BoxFuture<'static, gpui::Result<Response<AsyncBody>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(std::io::Error::other("no network in tests").into()) })
    }
}

/// Draws a tree the way a transcript row does.
struct Probe {
    tree: BlockTree,
    host: Host,
    theme: Theme,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(640.0))
            .text_size(px(15.0))
            .line_height(px(24.0))
            .child(markdown_view::blocks_with(
                &self.tree,
                &self.theme,
                self.host.clone(),
            ))
    }
}

fn tree_of(text: &str) -> BlockTree {
    let mut stream = Streaming::default();
    stream.set_text(text);
    stream.tree().clone()
}

fn probe<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
    host: Host,
    appearance: Appearance,
) -> (Entity<Probe>, &'a mut VisualTestContext) {
    one_window_test_at_a_time();
    let tree = tree_of(text);
    let (view, vcx) = cx.add_window_view(move |_, _| Probe {
        tree,
        host,
        theme: Theme::for_appearance(appearance),
    });
    vcx.simulate_resize(gpui::size(px(800.0), px(900.0)));
    draw(vcx);
    (view, vcx)
}

fn watch_files() -> Rc<Files> {
    Rc::new(Files::default())
}

/// A host whose files all resolve, recording what is opened.
#[derive(Default)]
struct Files {
    opened: RefCell<Vec<FileTarget>>,
    refuse: bool,
}

impl FileLinks for Rc<Files> {
    fn resolves(&self, _: &str, _: &gpui::App) -> bool {
        !self.refuse
    }

    fn open(&self, target: FileTarget, _: &mut Window, _: &mut gpui::App) {
        self.opened.borrow_mut().push(target);
    }
}

fn settle(vcx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
    wait_drawn(vcx, done);
}

#[gpui::test]
fn a_reply_with_an_https_image_and_links_makes_no_request(cx: &mut TestAppContext) {
    let sends = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| cx.set_http_client(Arc::new(Counting(sends.clone()))));
    let hits = Rc::new(links::fake::Recording::default());
    cx.update(|cx| links::install(cx, hits.clone()));
    let host = Host::transcript(None, None, Some(Rc::new(watch_files())));
    let text = format!(
        "{RENDER_REPLY}\nSee [the docs](https://example.com/docs) and <https://example.org>."
    );
    let (_view, vcx) = probe(cx, &text, host, Appearance::Dark);
    // Draw again and again, as scrolling does.
    for _ in 0..5 {
        draw(vcx);
    }
    assert!(
        painted(vcx, "md-image-card"),
        "the web image is a link card"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0, "nothing was fetched");
    assert!(
        hits.opened.borrow().is_empty(),
        "nothing was opened by drawing"
    );
}

#[gpui::test]
fn a_web_image_card_opens_only_through_the_link_opener(cx: &mut TestAppContext) {
    let sends = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| cx.set_http_client(Arc::new(Counting(sends.clone()))));
    let hits = Rc::new(links::fake::Recording::default());
    cx.update(|cx| links::install(cx, hits.clone()));
    let host = Host::transcript(None, None, None);
    let (_view, vcx) = probe(
        cx,
        "![status](https://example.com/status.png)",
        host,
        Appearance::Dark,
    );
    click(vcx, "md-image-card");
    assert_eq!(
        *hits.opened.borrow(),
        vec!["https://example.com/status.png".to_owned()]
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);
}

#[gpui::test]
fn the_transcript_policy_runs_web_links_through_the_opener_and_files_through_the_host(
    cx: &mut TestAppContext,
) {
    one_window_test_at_a_time();
    let hits = Rc::new(links::fake::Recording::default());
    cx.update(|cx| links::install(cx, hits.clone()));
    let files = watch_files();
    let policy = LinkPolicy::Transcript {
        files: Some(Rc::new(files.clone())),
    };
    let (_view, vcx) = cx.add_window_view(|_, _| Probe {
        tree: BlockTree::default(),
        host: Host::transcript(None, None, None),
        theme: Theme::for_appearance(Appearance::Dark),
    });
    vcx.update(|window, cx| {
        let web = policy.action("https://example.com/a");
        policy.run(&web, window, cx);
        // mailto and a custom scheme are plain text: nothing to run.
        assert_eq!(policy.action("mailto:a@example.org"), Action::Plain);
        policy.run(&policy.action("mailto:a@example.org"), window, cx);
        let file = policy.action("src/supervisor.rs:42");
        policy.run(&file, window, cx);
    });
    assert_eq!(
        *hits.opened.borrow(),
        vec!["https://example.com/a".to_owned()]
    );
    assert_eq!(
        *files.opened.borrow(),
        vec![FileTarget {
            path: "src/supervisor.rs".to_owned(),
            line: Some(42)
        }]
    );
}

#[gpui::test]
fn a_file_the_run_does_not_have_is_not_opened(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    let files = Rc::new(Files {
        refuse: true,
        ..Files::default()
    });
    let policy = LinkPolicy::Transcript {
        files: Some(Rc::new(files.clone())),
    };
    let (_view, vcx) = cx.add_window_view(|_, _| Probe {
        tree: BlockTree::default(),
        host: Host::preview(),
        theme: Theme::for_appearance(Appearance::Dark),
    });
    vcx.update(|window, cx| {
        policy.run(&policy.action("gone.rs:3"), window, cx);
    });
    assert!(files.opened.borrow().is_empty());
}

/// A code block's Copy puts only its code on the clipboard.
#[gpui::test]
fn copy_puts_only_the_blocks_code_on_the_clipboard(cx: &mut TestAppContext) {
    let text = "Before.\n\n```rust\nfn a() {}\nlet b = 2;\n```\n\nAfter.";
    let (_view, vcx) = probe(cx, text, Host::preview(), Appearance::Dark);
    click(vcx, "Copy code");
    let copied = vcx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("fn a() {}\nlet b = 2;"));
}

/// A code block has Copy and Wrap and no permanent header; Wrap flips and flips back. (That they
/// are hidden until the pointer is over the block is `chrome`'s own test.)
#[gpui::test]
fn a_code_block_has_copy_and_wrap_in_its_frame_and_no_header(cx: &mut TestAppContext) {
    let (_view, vcx) = probe(
        cx,
        "```rust\nfn a() {}\n```",
        Host::preview(),
        Appearance::Light,
    );
    assert!(painted(vcx, "md-card"));
    assert!(!painted(vcx, "md-card-header"));
    assert!(painted(vcx, "Copy code"));
    assert!(painted(vcx, "Wrap lines"));
    click(vcx, "Wrap lines");
    assert!(painted(vcx, "Unwrap lines"), "the wrap toggle flipped");
    click(vcx, "Unwrap lines");
    assert!(painted(vcx, "Wrap lines"));
}

#[gpui::test]
fn wrap_toggles_long_lines_and_unwrapped_lines_scroll_sideways(cx: &mut TestAppContext) {
    use crate::transcript::Key;
    use crate::ui::markdown_view::SelectHost;
    use crate::ui::markdown_view::select::{Registry, selection_style};

    let line = "let message = hello_world; ".repeat(12);
    let text = format!("```\n{}\n```", line.trim_end());
    let mut stream = Streaming::default();
    stream.set_text(&text);
    let registry = Registry::default();
    let host = Host::transcript(None, None, None).selectable(Some(Rc::new(SelectHost {
        registry: registry.clone(),
        key: Key(1),
        atoms: stream.atoms(),
        selected: None,
        style: selection_style(&Theme::for_appearance(Appearance::Light)),
    })));
    let (_view, vcx) = probe(cx, &text, host, Appearance::Light);
    let line_bounds = || registry.take().last().expect("painted code line").bounds;
    let scroll_sideways = |vcx: &mut VisualTestContext| {
        let at = vcx
            .debug_bounds("md-card")
            .expect("painted code block")
            .center();
        wheel_by(vcx, at, gpui::point(px(-120.0), px(0.0)), false);
    };

    let unwrapped = line_bounds();
    scroll_sideways(vcx);
    let scrolled = line_bounds();
    assert!(
        scrolled.left() < unwrapped.left(),
        "unwrapped code scrolls sideways: {unwrapped:?} -> {scrolled:?}"
    );

    click(vcx, "Wrap lines");
    draw(vcx);
    let wrapped = line_bounds();
    assert!(
        wrapped.size.height > unwrapped.size.height,
        "the long line wraps onto more rows"
    );
    scroll_sideways(vcx);
    assert_eq!(
        line_bounds().left(),
        wrapped.left(),
        "wrapped code stays in place"
    );

    click(vcx, "Unwrap lines");
    draw(vcx);
    let unwrapped_again = line_bounds();
    assert_eq!(unwrapped_again.size.height, unwrapped.size.height);
    scroll_sideways(vcx);
    assert!(
        line_bounds().left() < unwrapped_again.left(),
        "unwrapping restores sideways scrolling"
    );
}

#[gpui::test]
fn math_is_typeset_display_and_inline_or_its_source_when_off(cx: &mut TestAppContext) {
    let text = "Area: $x^2$ here.\n\n$$\\int_0^1 x^2\\,dx = \\tfrac13$$\n\nBroken: $$\\frac{1$$";
    let (_view, vcx) = probe(cx, text, Host::preview(), Appearance::Dark);
    assert!(painted(vcx, "md-math-display"), "display math is a picture");
    // Off: no picture, the source in code style.
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.typeset_math = false));
    draw(vcx);
    assert!(!painted(vcx, "md-math-display"));
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.typeset_math = true));
    draw(vcx);
    assert!(painted(vcx, "md-math-display"));
}

#[gpui::test]
fn display_math_reads_at_one_and_a_fifth_of_the_text_size_times_the_math_setting(
    cx: &mut TestAppContext,
) {
    let tex = r"\int_0^1 x^2\,dx = \tfrac13";
    let (_view, vcx) = probe(
        cx,
        &format!("$$ {tex} $$"),
        Host::preview(),
        Appearance::Dark,
    );
    // The probe's text is 15 px; the box adds 4 px above and below the picture.
    let height = |vcx: &mut VisualTestContext| {
        let bounds = vcx.debug_bounds("md-math-display").expect("painted");
        f32::from(bounds.size.height) - 8.0
    };
    let at = |size: f32| {
        let r = crate::ui::markdown_view::math::rasterize(tex, true, size, [255; 4], 1.0).unwrap();
        r.above + r.below
    };
    let got = height(vcx);
    assert!((got - at(15.0 * 1.2)).abs() < 1.5, "{got} vs {}", at(18.0));
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.math_size = 150));
    draw(vcx);
    let got = height(vcx);
    assert!(
        (got - at(15.0 * 1.2 * 1.5)).abs() < 1.5,
        "{got} vs {}",
        at(27.0)
    );
}

#[gpui::test]
fn a_formula_that_does_not_typeset_shows_its_source_and_no_picture(cx: &mut TestAppContext) {
    let (_view, vcx) = probe(cx, "$$\\frac{1$$", Host::preview(), Appearance::Dark);
    assert!(!painted(vcx, "md-math-display"));
}

#[gpui::test]
fn a_flowchart_is_drawn_and_a_bad_diagram_is_a_plain_code_block(cx: &mut TestAppContext) {
    let good = "```mermaid\nflowchart LR\n  A[Client] -->|hello| B[Handshake]\n```";
    let (_view, vcx) = probe(cx, good, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
    assert!(
        painted(vcx, "Copy code"),
        "the diagram frame copies its source"
    );
    assert!(!painted(vcx, "Wrap lines"), "a drawing does not wrap");

    let bad = "```mermaid\nnot a diagram\n```";
    let (_view, vcx) = probe(cx, bad, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| !painted(vcx, "md-diagram-rendering"));
    assert!(!painted(vcx, "md-diagram"));
    assert!(painted(vcx, "Wrap lines"), "a plain code block, with wrap");
}

#[gpui::test]
fn a_data_image_is_shown_and_a_path_outside_the_worktree_is_not_loaded(cx: &mut TestAppContext) {
    let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let host = Host::transcript(None, None, None);
    let (_view, vcx) = probe(
        cx,
        &format!("![dot]({png})"),
        host.clone(),
        Appearance::Dark,
    );
    assert!(painted(vcx, "md-image"));
    for src in [
        "../outside.png",
        "/etc/passwd",
        "file:///etc/passwd",
        "docs/graph.png",
    ] {
        let (_view, vcx) = probe(
            cx,
            &format!("![pic]({src})"),
            host.clone(),
            Appearance::Dark,
        );
        assert!(painted(vcx, "md-image-not-loaded"), "{src}");
        assert!(!painted(vcx, "md-image"), "{src}");
    }
}

fn fake_with_transcript(cx: &mut TestAppContext) -> Entity<Store> {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                step: std::time::Duration::ZERO,
                transcript_threads: true,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&run_id(RENDER_RUN)).is_some() && s.client().is_some()
        })
    });
    store
}

#[gpui::test]
fn a_worktree_image_is_read_through_the_daemon_for_the_run_and_shown(cx: &mut TestAppContext) {
    let store = fake_with_transcript(cx);
    let client = store.read_with(cx, |s, _| s.client());
    let host = Host::transcript(Some(run_id(RENDER_RUN)), client, None);
    let (_view, vcx) = probe(
        cx,
        "![build graph](docs/graph.png)",
        host.clone(),
        Appearance::Dark,
    );
    settle(vcx, |vcx| painted(vcx, "md-image"));
    // A file the run does not have, and a path outside the worktree.
    for src in ["docs/missing.png", "../Cargo.toml"] {
        let (_view, vcx) = probe(cx, &format!("![x]({src})"), host.clone(), Appearance::Dark);
        settle(vcx, |vcx| painted(vcx, "md-image-not-loaded"));
        assert!(!painted(vcx, "md-image"), "{src}");
    }
}

#[gpui::test]
fn the_fake_posts_visuals_only_while_the_capability_is_on(cx: &mut TestAppContext) {
    let store = fake_with_transcript(cx);
    let run = run_id(RENDER_RUN);
    assert_eq!(store.read_with(cx, |s, _| s.run_visuals(&run).len()), 2);
    store.update(cx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    assert!(store.read_with(cx, |s, _| s.run_visuals(&run).is_empty()));
}

#[gpui::test]
fn a_posted_image_and_diagram_draw_with_their_captions_and_a_failure_says_so(
    cx: &mut TestAppContext,
) {
    let store = fake_with_transcript(cx);
    let run = run_id(RENDER_RUN);
    let client = store.read_with(cx, |s, _| s.client());
    let host = Host::transcript(Some(run.clone()), client, None);
    let visuals = store.read_with(cx, |s, _| s.run_visuals(&run));
    let theme = Theme::for_appearance(Appearance::Dark);
    for visual in &visuals {
        let (host, visual) = (host.clone(), visual.clone());
        let (_view, vcx) = cx.add_window_view(move |_, _| ProbeVisual {
            visual,
            host,
            theme,
        });
        vcx.simulate_resize(gpui::size(px(800.0), px(900.0)));
        settle(vcx, |vcx| {
            painted(vcx, "md-image") || painted(vcx, "md-diagram")
        });
        assert!(painted(vcx, "visual"));
        assert!(!painted(vcx, "md-visual-failed"));
    }
    let broken = [
        Visual {
            id: "b1".to_owned(),
            caption: "Gone".to_owned(),
            after: reno_proto::Seq(2),
            kind: VisualKind::Image(VisualImage::Worktree("../escape.png".to_owned())),
        },
        Visual {
            id: "b2".to_owned(),
            caption: "Bad".to_owned(),
            after: reno_proto::Seq(2),
            kind: VisualKind::Diagram("not a diagram".to_owned()),
        },
    ];
    for visual in broken {
        let host = host.clone();
        let (_view, vcx) = cx.add_window_view(move |_, _| ProbeVisual {
            visual,
            host,
            theme,
        });
        vcx.simulate_resize(gpui::size(px(800.0), px(900.0)));
        settle(vcx, |vcx| painted(vcx, "md-visual-failed"));
        assert!(painted(vcx, "visual"), "the caption stays");
    }
}

struct ProbeVisual {
    visual: Visual,
    host: Host,
    theme: Theme,
}

impl Render for ProbeVisual {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(640.0))
            .child(visual_view(&self.visual, self.host.clone(), &self.theme))
    }
}

#[gpui::test]
fn a_diagram_still_arriving_is_shown_as_code_and_not_drawn(cx: &mut TestAppContext) {
    let partial = "Here:\n\n```mermaid\nflowchart LR\n  A --> B";
    let (_view, vcx) = probe(
        cx,
        partial,
        Host::transcript(None, None, None).while_streaming(true),
        Appearance::Dark,
    );
    assert!(!painted(vcx, "md-diagram"));
    assert!(!painted(vcx, "md-diagram-rendering"));
    assert!(painted(vcx, "Wrap lines"), "a code block meanwhile");
    // Once the message is whole the same text is drawn.
    let (_view, vcx) = probe(
        cx,
        partial,
        Host::transcript(None, None, None),
        Appearance::Dark,
    );
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
}

#[gpui::test]
fn two_identical_code_blocks_each_have_their_own_controls(cx: &mut TestAppContext) {
    let text = "```rust\nfn a() {}\n```\n\nAgain:\n\n```rust\nfn a() {}\n```";
    let (_view, vcx) = probe(cx, text, Host::preview(), Appearance::Dark);
    assert!(painted(vcx, "Copy code"));
    click(vcx, "Copy code");
    let copied = vcx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("fn a() {}"));
}

/// The Preview tab shows the drawing and the Code tab the source; Copy gives the source from
/// either; a failed diagram is a plain code block with no tabs.
#[gpui::test]
fn the_code_tab_shows_the_source_and_preview_the_drawing(cx: &mut TestAppContext) {
    let source = "flowchart LR\n  A[Client] -->|hello| B[Handshake]";
    let text = format!("```mermaid\n{source}\n```");
    let (_view, vcx) = probe(cx, &text, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
    assert!(painted(vcx, "diagram-tab-preview") && painted(vcx, "diagram-tab-code"));
    assert!(
        !painted(vcx, "md-diagram-source"),
        "the drawing, not its source"
    );

    click(vcx, "diagram-tab-code");
    assert!(painted(vcx, "md-diagram-source"));
    assert!(!painted(vcx, "md-diagram"), "no drawing on the Code tab");
    click(vcx, "Copy code");
    let copied = vcx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some(source), "Copy gives the source");

    click(vcx, "diagram-tab-preview");
    assert!(painted(vcx, "md-diagram"));
    assert!(!painted(vcx, "md-diagram-source"));

    let bad = "```mermaid\nnot a diagram\n```";
    let (_view, vcx) = probe(cx, bad, Host::preview(), Appearance::Dark);
    // Settled on a positive signal: the code block is painted and the "Rendering" note is gone
    // (while it draws, the card shows the note and the tabs).
    settle(vcx, |vcx| {
        painted(vcx, "Copy code") && !painted(vcx, "md-diagram-rendering")
    });
    assert!(!painted(vcx, "md-diagram"), "nothing was drawn");
    assert!(
        !painted(vcx, "diagram-tab-code"),
        "a plain code block has no tabs"
    );
}

/// A diagram that has to be drawn again in a new theme stays on screen as it was, with the
/// "Rendering\u{2026}" note, until the new drawing lands.
#[gpui::test]
fn a_diagram_drawn_again_keeps_its_picture_and_says_so(cx: &mut TestAppContext) {
    let text = "```mermaid\nflowchart LR\n  A[Client] --> B[Handshake]\n```";
    let (view, vcx) = probe(cx, text, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
    assert!(!painted(vcx, "md-diagram-rendering"));
    view.update(vcx, |probe, cx| {
        probe.theme = Theme::for_appearance(Appearance::Light);
        cx.notify();
    });
    assert!(painted(vcx, "md-diagram"), "the old picture stays");
    assert!(
        painted(vcx, "md-diagram-rendering"),
        "and says it is drawing"
    );
    settle(vcx, |vcx| !painted(vcx, "md-diagram-rendering"));
    assert!(painted(vcx, "md-diagram"));
}

/// A flowchart wider than the transcript's column, so it fits no better than three quarters of
/// its size and scrolls sideways once it is zoomed.
const WIDE_FLOW: &str = "flowchart LR\n  A[Client] --> B[Handshake] --> C[Negotiation] --> D[Session] --> E[Transfer] --> F[Teardown] --> G[Archive]";

fn wide_flow_key() -> u64 {
    markdown_view::chrome::key_of(Some("mermaid"), WIDE_FLOW)
}

fn natural_width_of(source: &str) -> f32 {
    let theme = Theme::for_appearance(Appearance::Dark);
    let svg = markdown_view::mermaid::render_svg(source, &theme).unwrap();
    markdown_view::mermaid::rasterize(&svg, 1.0).unwrap().width
}

/// One wheel step of `dy` points over the middle of the drawn diagram, with or without the
/// command key, and the redraw that follows.
fn wheel(vcx: &mut VisualTestContext, dy: f32, command: bool) {
    let at = vcx.debug_bounds("md-diagram").expect("drawn").center();
    wheel_by(vcx, at, gpui::point(px(0.0), px(dy)), command);
}

fn wheel_by(
    vcx: &mut VisualTestContext,
    at: gpui::Point<gpui::Pixels>,
    delta: gpui::Point<gpui::Pixels>,
    command: bool,
) {
    vcx.simulate_event(gpui::ScrollEvent {
        position: at,
        delta: gpui::ScrollDelta::Pixels(delta),
        modifiers: if command {
            gpui::Modifiers::command()
        } else {
            gpui::Modifiers::none()
        },
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    draw(vcx);
}

fn zoom_of(vcx: &mut VisualTestContext) -> Option<f32> {
    let key = wide_flow_key();
    vcx.update(|_, cx| markdown_view::chrome::zoom(key, cx).map(|z| z.factor))
}

fn zoomed_width(vcx: &mut VisualTestContext) -> f32 {
    let zoom = vcx.debug_bounds("md-diagram-zoom").expect("drawn");
    f32::from(zoom.size.width) - 24.0
}

/// Holding the command key while scrolling over a diagram zooms it, from where it fits the
/// column to twice its size, shows the level with a reset while zoomed, and the reset (or
/// scrolling back out) returns it to fit. Scrolling without the key never zooms.
#[gpui::test]
fn cmd_scroll_zooms_between_fit_and_two_hundred_percent_and_reset_returns(cx: &mut TestAppContext) {
    let natural = natural_width_of(WIDE_FLOW);
    let text = format!("```mermaid\n{WIDE_FLOW}\n```");
    let (_view, vcx) = probe(cx, &text, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
    assert!(
        natural > 700.0,
        "the diagram must be wider than the column to tell fit from natural size: {natural}"
    );
    let fit = zoomed_width(vcx);
    assert!(
        fit < natural,
        "fitted to the column, not at its natural size"
    );
    assert_eq!(zoom_of(vcx), None);
    assert!(!painted(vcx, "diagram-zoom-reset"), "no reset while fitted");

    // No command key: the wheel is the list's, not the diagram's.
    wheel(vcx, 200.0, false);
    assert_eq!(zoom_of(vcx), None);

    // Scrolling out from fit goes nowhere.
    wheel(vcx, -300.0, true);
    assert_eq!(zoom_of(vcx), None);
    assert!(!painted(vcx, "diagram-zoom-reset"));

    // One step in: zoomed, with the reset showing.
    wheel(vcx, 40.0, true);
    let first = zoom_of(vcx).expect("zoomed");
    assert!(first > fit / natural, "larger than fit: {first}");
    assert!(first < 2.0);
    assert!(painted(vcx, "diagram-zoom-reset"));
    assert!(
        (zoomed_width(vcx) - first * natural).abs() < 1.5,
        "drawn at the zoom's share of its size"
    );

    // Far past the top: held at twice its size, never more.
    for _ in 0..30 {
        wheel(vcx, 200.0, true);
    }
    assert_eq!(zoom_of(vcx), Some(2.0));
    assert!((zoomed_width(vcx) - 2.0 * natural).abs() < 1.5);

    // Sideways scroll reaches the part of the zoomed diagram outside the column.
    let before = vcx.debug_bounds("md-diagram-zoom").unwrap().origin.x;
    let at = vcx.debug_bounds("md-diagram").unwrap().center();
    wheel_by(vcx, at, gpui::point(px(-120.0), px(0.0)), false);
    let after = vcx.debug_bounds("md-diagram-zoom").unwrap().origin.x;
    assert!(after < before - px(100.0), "{before:?} -> {after:?}");
    assert_eq!(zoom_of(vcx), Some(2.0), "a sideways scroll is not a zoom");

    // The reset goes straight back to fit.
    click(vcx, "diagram-zoom-reset");
    assert_eq!(zoom_of(vcx), None);
    assert!((zoomed_width(vcx) - fit).abs() < 1.5);
    assert!(!painted(vcx, "diagram-zoom-reset"));

    // And so does scrolling all the way out.
    wheel(vcx, 60.0, true);
    assert!(zoom_of(vcx).is_some());
    for _ in 0..30 {
        wheel(vcx, -200.0, true);
    }
    assert_eq!(zoom_of(vcx), None);
    assert!((zoomed_width(vcx) - fit).abs() < 1.5);
}

/// The picture is sharpened once the owner stops zooming: one new drawing 300 ms after the last
/// step, however many steps came before, and none while steps keep coming.
#[gpui::test]
fn the_re_raster_fires_once_after_the_last_zoom_step(cx: &mut TestAppContext) {
    use markdown_view::chrome::{self, RERASTER_AFTER};
    let text = format!("```mermaid\n{WIDE_FLOW}\n```");
    let (_view, vcx) = probe(cx, &text, Host::preview(), Appearance::Dark);
    settle(vcx, |vcx| painted(vcx, "md-diagram"));
    let key = wide_flow_key();
    let drawings =
        |vcx: &mut VisualTestContext| vcx.update(|_, cx| markdown_view::mermaid::cached(cx));
    let raster = |vcx: &mut VisualTestContext| vcx.update(|_, cx| chrome::raster_factor(key, cx));
    let fired = |vcx: &mut VisualTestContext| vcx.update(|_, cx| chrome::rerasters(cx));
    let base = drawings(vcx);
    assert_eq!(raster(vcx), 1.0);

    for step in 0..3 {
        wheel(vcx, 150.0, true);
        if step < 2 {
            vcx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            vcx.run_until_parked();
        }
    }
    let zoom = zoom_of(vcx).expect("zoomed");
    assert!(zoom > 1.0, "zoomed past its natural size: {zoom}");

    vcx.executor()
        .advance_clock(RERASTER_AFTER - std::time::Duration::from_millis(1));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        (fired(vcx), raster(vcx)),
        (0, 1.0),
        "not before the wait is up"
    );
    assert_eq!(
        drawings(vcx),
        base,
        "no new drawing while steps may still come"
    );

    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(2));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(fired(vcx), 1, "once, not once per step");
    assert!(
        (raster(vcx) - zoom).abs() < 1e-4,
        "drawn at the zoom it settled on"
    );
    assert_eq!(drawings(vcx), base + 1, "one new drawing, at the new size");

    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(2_000));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(fired(vcx), 1, "nothing is armed once it has fired");
    assert_eq!(drawings(vcx), base + 1);

    // A new step arms it again, and only the last of a burst fires.
    wheel(vcx, -20.0, true);
    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    wheel(vcx, -20.0, true);
    vcx.executor()
        .advance_clock(RERASTER_AFTER + std::time::Duration::from_millis(1));
    vcx.run_until_parked();
    assert_eq!(fired(vcx), 2);
}
