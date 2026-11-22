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
use crate::vm::draft33::Variant;

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

/// A code block's Copy puts only its code on the clipboard, in every variant.
#[gpui::test]
fn copy_puts_only_the_blocks_code_on_the_clipboard(cx: &mut TestAppContext) {
    let text = "Before.\n\n```rust\nfn a() {}\nlet b = 2;\n```\n\nAfter.";
    let (_view, vcx) = probe(cx, text, Host::preview(), Appearance::Dark);
    click(vcx, "Copy code");
    let copied = vcx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("fn a() {}\nlet b = 2;"));
}

#[gpui::test]
fn every_variant_has_copy_and_wrap_and_the_chrome_that_variant_names(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    for (variant, header) in [(Variant::A, false), (Variant::B, true), (Variant::C, true)] {
        cx.update(|cx| crate::ui::draft33::install(variant, cx));
        let (_view, vcx) = probe(
            cx,
            "```rust\nfn a() {}\n```",
            Host::preview(),
            Appearance::Light,
        );
        assert_eq!(painted(vcx, "md-card-header"), header, "{variant:?}");
        assert!(painted(vcx, "Copy code"), "{variant:?}");
        assert!(painted(vcx, "Wrap lines"), "{variant:?}");
        click(vcx, "Wrap lines");
        assert!(
            painted(vcx, "Unwrap lines"),
            "{variant:?}: the wrap toggle flipped"
        );
        click(vcx, "Unwrap lines");
        assert!(painted(vcx, "Wrap lines"), "{variant:?}");
    }
}

#[gpui::test]
fn outside_the_draft_a_code_block_has_the_quiet_variant(cx: &mut TestAppContext) {
    let (_view, vcx) = probe(cx, "```\nplain\n```", Host::preview(), Appearance::Dark);
    assert!(painted(vcx, "md-card"));
    assert!(!painted(vcx, "md-card-header"));
    assert!(painted(vcx, "Copy code"));
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
    let (_view, vcx) = probe(cx, &format!("$$ {tex} $$"), Host::preview(), Appearance::Dark);
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
    assert!((got - at(15.0 * 1.2 * 1.5)).abs() < 1.5, "{got} vs {}", at(27.0));
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
