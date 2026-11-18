//! The composer and the routing UI against the fake daemon: attachments, route preview (debounce,
//! "why?", unavailable), steer and stop, the draft coming back when a send fails, and the IME
//! entry points. IME composition itself is driven through the platform handler methods, as the
//! platform calls them; the text model's own tests cover what each step does to the text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{ClipboardEntry, ClipboardItem, EntityInputHandler as _, ExternalPaths, TestAppContext};
use reno_proto::{ErrorCode, ErrorObject, RunStatus};

use crate::store::RunRouting;
use crate::tests::{approvals, click, draw, fake_store, painted, wait_drawn, wait_until, window};
use crate::ui::composer::{PREVIEW_DEBOUNCE, bind_keys};
use crate::ui::theme::Appearance;
use crate::vm::routing::{Class, Preview};

fn settle_debounce(vcx: &mut gpui::VisualTestContext) {
    vcx.executor()
        .advance_clock(PREVIEW_DEBOUNCE + Duration::from_millis(10));
    vcx.run_until_parked();
}

fn ready(vcx: &gpui::VisualTestContext, c: &gpui::Entity<crate::ui::composer::Composer>) -> bool {
    c.read_with(vcx, |c, _| matches!(c.preview(), Preview::Ready(_)))
}

#[gpui::test]
fn the_preview_asks_the_daemon_once_after_the_typing_stops(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let daemon = store.read_with(cx, |s, _| s.fake().cloned().unwrap());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());

    // The first ask goes out at once; wait for it.
    wait_drawn(vcx, |vcx| ready(vcx, &composer));
    let before = daemon.route_previews().len();
    assert_eq!(before, 1);

    // Many edits in a row: nothing is asked while they keep coming.
    for chunk in ["f", "fi", "fix", "fix the", "fix the bug"] {
        composer.update_in(vcx, |c, window, cx| {
            c.replace_text_in_range(Some(0..usize::MAX), chunk, window, cx);
        });
        vcx.executor().advance_clock(Duration::from_millis(50));
        vcx.run_until_parked();
    }
    assert_eq!(daemon.route_previews().len(), before, "not on every key");

    settle_debounce(vcx);
    wait_until(vcx, |_| daemon.route_previews().len() == before + 1);
    let asked = daemon.route_previews().pop().unwrap();
    assert_eq!(asked.prompt, "fix the bug");
    assert_eq!(asked.executor, None, "the router decides");
    assert_eq!(asked.bot.as_deref(), Some("builder"));
    assert!(asked.attachments.is_empty());

    // The same text again asks for nothing.
    settle_debounce(vcx);
    assert_eq!(daemon.route_previews().len(), before + 1);
}

#[gpui::test]
fn choosing_a_class_previews_its_bots_route_and_why_lists_who_was_passed_over(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    wait_drawn(vcx, |vcx| ready(vcx, &composer));

    // Review goes to the reviewer bot, which prefers Codex, which is signed out in the demo.
    click(vcx, "class-menu");
    wait_drawn(vcx, |vcx| painted(vcx, "class-review"));
    click(vcx, "class-review");
    settle_debounce(vcx);
    wait_drawn(vcx, |vcx| {
        composer.read_with(vcx, |c, _| match c.preview() {
            Preview::Ready(d) => d
                .chosen
                .as_ref()
                .is_some_and(|e| e.as_str() == "claude-code"),
            _ => false,
        })
    });
    assert!(
        painted(vcx, "route-name"),
        "Auto's chip names where it routes"
    );
    assert!(!painted(vcx, "why-panel"));
    // "why?" lives on Auto's row in the picker; it opens the panel and closes the picker.
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "why"));
    click(vcx, "why");
    wait_drawn(vcx, |vcx| painted(vcx, "why-panel"));
    assert!(!painted(vcx, "model-picker"));
    assert!(composer.read_with(vcx, |c, _| c.why_is_open()));
    assert!(painted(vcx, "rejected-0"));
    let Preview::Ready(d) = composer.read_with(vcx, |c, _| c.preview().clone()) else {
        panic!("ready");
    };
    let view = crate::vm::routing::view(&d);
    assert_eq!(view.rejected[0].who.name, "Codex");
    assert_eq!(view.rejected[0].reason, "Signed out. Run `codex login`.");
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "why"));
    click(vcx, "why");
    draw(vcx);
    assert!(!painted(vcx, "why-panel"));
}

#[gpui::test]
fn a_preview_the_daemon_cannot_answer_says_so_and_sending_still_works(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let daemon = store.read_with(cx, |s, _| s.fake().cloned().unwrap());
    daemon.fail(
        "route.preview",
        ErrorObject::new(ErrorCode::InvalidParams, "no router today"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    wait_drawn(vcx, |vcx| {
        composer.read_with(vcx, |c, _| matches!(c.preview(), Preview::Unavailable(_)))
    });
    assert!(painted(vcx, "route-unavailable"));
    composer.update_in(vcx, |c, window, cx| {
        c.fill("still goes", window, cx);
        c.submit(window, cx);
    });
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
}

#[gpui::test]
fn a_task_goes_with_its_attachments_and_the_daemon_lists_them(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot.png");
    std::fs::write(&shot, [0u8; 3000]).unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, b"x").unwrap();
    let shot = std::fs::canonicalize(shot).unwrap();

    // The drop path: the same call the drop handler makes.
    composer.update_in(vcx, |c, _, cx| {
        c.add_attachments(&[shot.clone(), notes.clone()], cx);
    });
    draw(vcx);
    assert!(painted(vcx, "attachment-0") && painted(vcx, "attachment-1"));
    click(vcx, "remove-attachment-1");
    draw(vcx);
    assert!(!painted(vcx, "attachment-1"));
    assert_eq!(composer.read_with(vcx, |c, _| c.files().items().len()), 1);

    composer.update_in(vcx, |c, window, cx| c.fill("look at this", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    assert!(composer.read_with(vcx, |c, _| c.files().items().is_empty()));
    let run = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.run_id().clone());
    let prompt = store
        .read_with(vcx, |s, _| s.state().run(&run).cloned())
        .unwrap()
        .prompt
        .unwrap();
    assert_eq!(
        prompt,
        format!(
            "look at this\n\nAttached files:\n- {} (shot.png, image/png)\n",
            shot.display()
        )
    );
}

#[gpui::test]
fn paste_attaches_files_and_screenshots_and_inserts_text(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    vcx.update(|window, cx| window.focus(&composer.read(cx).focus_handle_ref().clone(), cx));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, b"# hi").unwrap();

    vcx.update(|_, cx| {
        cx.write_to_clipboard(ClipboardItem::from(ClipboardEntry::ExternalPaths(
            ExternalPaths([file.clone()].into_iter().collect()),
        )));
    });
    vcx.simulate_keystrokes("cmd-v");
    assert_eq!(composer.read_with(vcx, |c, _| c.files().items().len()), 1);
    assert_eq!(composer.read_with(vcx, |c, _| c.text().to_owned()), "");

    vcx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("plain text".into())));
    vcx.simulate_keystrokes("cmd-v");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "plain text"
    );
    assert_eq!(composer.read_with(vcx, |c, _| c.files().items().len()), 1);

    // Undo and redo are keys too.
    vcx.simulate_keystrokes("cmd-z");
    assert_eq!(composer.read_with(vcx, |c, _| c.text().to_owned()), "");
    vcx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "plain text"
    );
}

/// What a Finder copy puts on the macOS pasteboard: the file URL, its name as text, and the
/// file's icon as an image.
fn finder_copy(file: &std::path::Path) -> ClipboardItem {
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(ExternalPaths(
                [file.to_path_buf()].into_iter().collect(),
            )),
            ClipboardEntry::String(gpui::ClipboardString::new(name)),
            ClipboardEntry::Image(gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![0; 8])),
        ],
    }
}

const STEER_TOAST: (&str, &str) = ("Files", "Only a new task takes files. A steer is text.");

#[gpui::test]
fn a_finder_copy_pastes_as_a_chip_on_home_and_a_toast_on_a_steer(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"x").unwrap();

    // Home: a chip, not the file's name.
    let home = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    vcx.update(|window, cx| window.focus(&home.read(cx).focus_handle_ref().clone(), cx));
    vcx.update(|_, cx| cx.write_to_clipboard(finder_copy(&file)));
    vcx.simulate_keystrokes("cmd-v");
    assert_eq!(home.read_with(vcx, |c, _| c.files().items().len()), 1);
    assert_eq!(home.read_with(vcx, |c, _| c.text().to_owned()), "");

    // A steer: a toast, no chip, the text as it was.
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "stop"));
    let steer = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("use the mirror", window, cx));
    vcx.update(|_, cx| cx.write_to_clipboard(finder_copy(&file)));
    vcx.simulate_keystrokes("cmd-v");
    assert_eq!(
        steer.read_with(vcx, |c, _| c.text().to_owned()),
        "use the mirror"
    );
    assert!(steer.read_with(vcx, |c, _| c.files().items().is_empty()));
    let shown = vcx.update(|_, cx| crate::ui::toast::shown(cx));
    assert!(
        shown.contains(&(STEER_TOAST.0.to_owned(), STEER_TOAST.1.to_owned())),
        "{shown:?}"
    );
}

#[gpui::test]
fn dropping_files_on_a_steer_says_a_steer_is_text(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "stop"));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"x").unwrap();
    let steer = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.steer_input().clone());
    let at = vcx.debug_bounds("composer").unwrap().center();
    vcx.simulate_event(gpui::FileDropEvent::Entered {
        position: at,
        paths: ExternalPaths([file].into_iter().collect()),
    });
    vcx.simulate_event(gpui::FileDropEvent::Pending { position: at });
    vcx.simulate_event(gpui::FileDropEvent::Submit { position: at });
    vcx.run_until_parked();
    assert!(steer.read_with(vcx, |c, _| c.files().items().is_empty()));
    let shown = vcx.update(|_, cx| crate::ui::toast::shown(cx));
    assert!(
        shown.contains(&(STEER_TOAST.0.to_owned(), STEER_TOAST.1.to_owned())),
        "{shown:?}"
    );
    // A refusal is a caution, not the green tick of something that worked.
    let tones = vcx.update(|_, cx| crate::ui::toast::shown_with_tone(cx));
    assert_eq!(
        tones,
        [(crate::ui::toast::Tone::NotYet, STEER_TOAST.0.to_owned())]
    );
}

#[gpui::test]
fn keys_move_by_word_and_line_and_enter_sends_while_shift_enter_breaks_the_line(
    cx: &mut TestAppContext,
) {
    cx.update(bind_keys);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    vcx.update(|window, cx| window.focus(&composer.read(cx).focus_handle_ref().clone(), cx));
    vcx.simulate_input("alpha beta gamma");
    vcx.simulate_keystrokes("alt-left alt-left");
    vcx.simulate_input("X");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "alpha Xbeta gamma"
    );
    vcx.simulate_keystrokes("alt-backspace");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "alpha beta gamma"
    );
    vcx.simulate_keystrokes("cmd-down shift-enter");
    vcx.simulate_input("second");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "alpha beta gamma\nsecond"
    );
    vcx.simulate_keystrokes("cmd-a");
    vcx.simulate_input("done");
    assert_eq!(composer.read_with(vcx, |c, _| c.text().to_owned()), "done");
    vcx.simulate_keystrokes("enter");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
}

#[gpui::test]
fn an_ime_composition_marks_then_commits_and_enter_belongs_to_it(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    let before = store.read_with(vcx, |s, _| s.state().runs().len());
    composer.update_in(vcx, |c, window, cx| {
        c.replace_and_mark_text_in_range(None, "vie", Some(3..3), window, cx);
    });
    assert_eq!(
        composer.update_in(vcx, |c, window, cx| c.marked_text_range(window, cx)),
        Some(0..3)
    );
    // Enter while composing does not send.
    composer.update_in(vcx, |c, window, cx| c.submit(window, cx));
    assert_eq!(store.read_with(vcx, |s, _| s.state().runs().len()), before);
    composer.update_in(vcx, |c, window, cx| {
        c.replace_and_mark_text_in_range(None, "việ", Some(3..3), window, cx);
        c.replace_text_in_range(None, "việt", window, cx);
    });
    assert_eq!(
        composer.update_in(vcx, |c, window, cx| c.marked_text_range(window, cx)),
        None
    );
    assert_eq!(composer.read_with(vcx, |c, _| c.text().to_owned()), "việt");
    draw(vcx);
    // After a paint the candidate window has somewhere to go.
    let bounds = composer.update_in(vcx, |c, window, cx| {
        c.bounds_for_range(0..2, gpui::Bounds::default(), window, cx)
    });
    assert!(bounds.is_some());
}

#[gpui::test]
fn a_draft_comes_back_when_the_daemon_refuses_the_task(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let daemon = store.read_with(cx, |s, _| s.fake().cloned().unwrap());
    daemon.fail(
        "task.run",
        ErrorObject::new(ErrorCode::InvalidParams, "refused"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, b"x").unwrap();
    composer.update_in(vcx, |c, window, cx| {
        c.add_attachments(&[file], cx);
        c.fill("do the thing", window, cx);
        c.submit(window, cx);
    });
    wait_until(vcx, |cx| {
        composer.read_with(cx, |c, _| c.text() == "do the thing")
    });
    assert_eq!(composer.read_with(vcx, |c, _| c.files().items().len()), 1);
    assert!(root.read_with(vcx, |r, _| r.run_screen().is_none()));
}

#[gpui::test]
fn a_live_run_shows_stop_until_something_is_typed_and_steer_then(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "stop"));
    assert!(!painted(vcx, "send"), "one button at a time");
    let steer = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("use the mirror", window, cx));
    wait_drawn(vcx, |vcx| painted(vcx, "send") && !painted(vcx, "stop"));
    click(vcx, "send");
    assert_eq!(steer.read_with(vcx, |c, _| c.text().to_owned()), "");
    wait_drawn(vcx, |vcx| painted(vcx, "composer-note"));
    assert!(steer.read_with(vcx, |c, _| c.is_sent_noted()));
    // Stop is back, and it stops the run.
    wait_drawn(vcx, |vcx| painted(vcx, "stop"));
    click(vcx, "stop");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .runs()
                .iter()
                .any(|r| r.run_id.to_string() == "run_keeper1" && r.status == RunStatus::Cancelled)
        })
    });
    // Ended: no Stop; the box now offers a new run in the project, so Send stays (disabled
    // until something is typed).
    wait_drawn(vcx, |vcx| {
        !painted(vcx, "stop") && painted(vcx, "composer-note")
    });
    assert!(!steer.read_with(vcx, |c, cx| c.can_send(cx)));
}

#[gpui::test]
fn the_run_screen_shows_where_the_run_went_and_the_fallback_walk(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // The seeded builder run's first choice (Grok, too old) would not start.
    click(vcx, "recent-run_builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "routing-card"));
    let run = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.run_id().clone());
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            matches!(s.routing(&run), Some(RunRouting::Ready(Some(_))))
        })
    });
    let Some(RunRouting::Ready(Some(decision))) =
        store.read_with(vcx, |s, _| s.routing(&run).cloned())
    else {
        panic!("a decision");
    };
    let view = crate::vm::routing::view(&decision);
    assert_eq!(view.headline, "Routed to Claude Code");
    assert_eq!(view.because.as_deref(), Some("Grok would not start"));
    assert_eq!(view.walk.len(), 1);
    assert_eq!(view.walk[0].from.name, "Grok");
    assert_eq!(view.walk[0].from.secondary(), Some("acp.grok"));
    click(vcx, "routing-why");
    draw(vcx);
}

#[gpui::test]
fn the_class_chips_travel_as_bots_the_daemon_has(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    composer.update_in(vcx, |c, window, cx| {
        c.set_class(Class::Research, cx);
        c.fill("find out", window, cx);
        c.submit(window, cx);
    });
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    let run = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .unwrap()
        .read_with(vcx, |s, _| s.run_id().clone());
    let summary = store
        .read_with(vcx, |s, _| s.state().run(&run).cloned())
        .unwrap();
    assert_eq!(summary.bot.unwrap().name, "scout");
    assert_eq!(
        summary.executor.as_str(),
        "claude-code",
        "Codex is signed out"
    );
}

// --- The harness · model picker (2026-11-17) --------------------------------------------------

fn home_composer(
    vcx: &mut gpui::VisualTestContext,
    root: &gpui::Entity<crate::app::RootView>,
) -> gpui::Entity<crate::ui::composer::Composer> {
    root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone())
}

#[gpui::test]
fn a_harness_and_model_picked_on_home_go_with_the_task(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    assert!(painted(vcx, "model-chip"));
    assert!(!painted(vcx, "model-picker"), "the picker starts closed");
    assert!(
        !painted(vcx, "chip-effort"),
        "Auto has no model, so no effort"
    );
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-harness-auto"));
    assert!(
        painted(vcx, "picker-tab-favourites") && painted(vcx, "picker-tab-auto"),
        "the rail starts with Favourites and Auto on Home"
    );
    assert!(
        painted(vcx, "picker-tab-codex"),
        "every installed harness has a tab"
    );
    assert!(!painted(vcx, "picker-search"), "Auto has no search");
    click(vcx, "picker-tab-claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-sonnet-5-5"));
    click(vcx, "pick-model-claude-sonnet-5-5");
    draw(vcx);
    let composer = home_composer(vcx, &root);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().cloned()),
        Some(crate::vm::models::Pick {
            options: Default::default(),
            executor: "claude-code".parse().unwrap(),
            model: Some("claude-sonnet-5-5".into()),
            effort: None,
        })
    );
    assert!(
        painted(vcx, "model-picker"),
        "a choice keeps the picker open"
    );
    assert!(
        painted(vcx, "effort-cell-low") && painted(vcx, "effort-cell-max"),
        "the cells are the model's own levels"
    );
    click(vcx, "model-chip");
    draw(vcx);
    assert!(!painted(vcx, "model-picker"), "the chip closes it");

    composer.update_in(vcx, |c, window, cx| c.fill("pick test", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    let run = root
        .read_with(vcx, |r, cx| {
            r.run_screen().map(|s| s.read(cx).run_id().clone())
        })
        .unwrap();
    store.read_with(vcx, |s, _| {
        let summary = s.state().run(&run).unwrap();
        assert_eq!(summary.executor.as_str(), "claude-code");
        assert_eq!(summary.model.as_deref(), Some("claude-sonnet-5-5"));
    });
}

#[gpui::test]
fn a_live_claude_run_switches_model_from_its_composer(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-haiku-5-5"));
    assert!(
        !painted(vcx, "picker-tab-codex")
            && !painted(vcx, "picker-tab-auto")
            && !painted(vcx, "picker-tab-favourites"),
        "a live run keeps its harness: only its own tab is on the rail"
    );
    click(vcx, "pick-model-claude-haiku-5-5");
    let keeper = crate::tests::run_id("keeper1");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .run(&keeper)
                .and_then(|r| r.model.clone())
                .as_deref()
                == Some("claude-haiku-5-5")
        })
    });
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            s.rows()
                .iter()
                .any(|r| matches!(r, crate::transcript::Row::ModelChanged { model, .. } if model == "claude-haiku-5-5"))
        })
    });
}

#[gpui::test]
fn a_live_codex_run_shows_its_model_but_cannot_switch_it(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    click(vcx, "model-chip");
    draw(vcx);
    assert!(
        !painted(vcx, "pick-model-gpt-6-astra"),
        "Codex fixes its model per turn: no picker on a live run"
    );
}

#[gpui::test]
fn a_live_runs_picker_closes_when_the_run_ends(cx: &mut TestAppContext) {
    // Dogfood 2026-11-17: the run ended under an open picker, which turned into the next
    // task's picker with its pills shifted, and the click picked another model.
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "model-picker"));
    let keeper = crate::tests::run_id("keeper1");
    store.update(vcx, |s, cx| s.cancel_run(keeper.clone(), cx));
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .run(&keeper)
                .is_some_and(|r| r.status == RunStatus::Cancelled)
        })
    });
    wait_drawn(vcx, |vcx| !painted(vcx, "model-picker"));
    let _ = root;
}

#[gpui::test]
fn an_effort_picked_for_the_model_goes_with_the_task(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-tab-claude-code"));
    assert!(
        painted(vcx, "picker-effort") && !painted(vcx, "effort-cell-high"),
        "no levels before a model"
    );
    click(vcx, "picker-tab-claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-haiku-5-5"));
    // Haiku takes low to high: no xhigh.
    click(vcx, "pick-model-claude-haiku-5-5");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-cell-high"));
    assert!(!painted(vcx, "effort-cell-xhigh"));
    let composer = home_composer(vcx, &root);
    click(vcx, "effort-cell-high");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(reno_proto::Effort::High)
    );
    // Sonnet goes to max; high carries over, since Sonnet takes it too. The picker stays open.
    click(vcx, "pick-model-claude-sonnet-5-5");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-cell-xhigh"));
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(reno_proto::Effort::High)
    );
    // The chosen cell again goes back to the model's default; then xhigh.
    click(vcx, "effort-cell-high");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        None
    );
    click(vcx, "effort-cell-xhigh");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().cloned()),
        Some(crate::vm::models::Pick {
            options: Default::default(),
            executor: "claude-code".parse().unwrap(),
            model: Some("claude-sonnet-5-5".into()),
            effort: Some(reno_proto::Effort::XHigh),
        })
    );
    // Going back to Haiku restores the effort it was left at (high), not Sonnet's.
    click(vcx, "pick-model-claude-haiku-5-5");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(reno_proto::Effort::High)
    );
    click(vcx, "pick-model-claude-sonnet-5-5");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(reno_proto::Effort::XHigh),
        "and Sonnet its own"
    );
    composer.update_in(vcx, |c, window, cx| c.fill("effort test", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    let run = root
        .read_with(vcx, |r, cx| {
            r.run_screen().map(|s| s.read(cx).run_id().clone())
        })
        .unwrap();
    store.read_with(vcx, |s, _| {
        let summary = s.state().run(&run).unwrap();
        assert_eq!(summary.effort, Some(reno_proto::Effort::XHigh));
        assert_eq!(summary.model.as_deref(), Some("claude-sonnet-5-5"));
    });
}

#[gpui::test]
fn a_live_runs_effort_is_changed_through_the_daemon_and_an_ended_runs_goes_with_the_next(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    let keeper = crate::tests::run_id("keeper1");
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let composer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    // The demo run reports the alias `opus-5.5`; it resolves to the listed Opus 5.5, so its
    // levels and window show before anything is switched.
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-haiku-5-5"));
    assert!(painted(vcx, "effort-cell-high"));
    assert!(painted(vcx, "picker-context"));
    click(vcx, "pick-model-claude-haiku-5-5");
    // A live run: the cells show its model's levels, and a click changes nothing.
    wait_drawn(vcx, |vcx| painted(vcx, "effort-cell-high"));
    assert!(painted(vcx, "effort-word"));
    // A click asks the daemon (`run.configure`), which says when it applies; the composer's own
    // pick stays unset on a live run.
    click(vcx, "effort-cell-high");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&keeper).and_then(|r| r.effort) == Some(reno_proto::Effort::High)
        })
    });
    wait_drawn(vcx, |vcx| painted(vcx, "effort-applies"));
    assert_eq!(composer.read_with(vcx, |c, _| c.pick().cloned()), None);
    assert!(
        store.read_with(vcx, |s, _| s.config_applied(&keeper).effort.is_some()),
        "the answer says when"
    );
    // Ended: a cell picks the effort the next run starts with.
    store.update(vcx, |s, cx| s.cancel_run(keeper.clone(), cx));
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .run(&keeper)
                .is_some_and(|r| r.status == RunStatus::Cancelled)
        })
    });
    draw(vcx);
    assert!(!painted(vcx, "model-picker"), "the run ending closes it");
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-cell-high"));
    click(vcx, "effort-cell-low");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(reno_proto::Effort::Low),
        "the run asked for high; the next run starts from what is clicked"
    );
}

// --- The v2 rail (2026-11-18) ------------------------------------------------------------------

/// Home with the fake daemon's executors loaded and the picker open on `tab`.
fn open_picker_on<'a>(
    cx: &'a mut TestAppContext,
    tab: &str,
) -> (
    gpui::Entity<crate::store::Store>,
    gpui::Entity<crate::app::RootView>,
    &'a mut gpui::VisualTestContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-tab-claude-code"));
    click(vcx, &format!("picker-tab-{tab}"));
    draw(vcx);
    (store, root, vcx)
}

#[gpui::test]
fn the_search_is_scoped_to_the_tab_and_offers_other_harnesses_apart(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open_picker_on(cx, "claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-sonnet-5-5"));
    assert!(painted(vcx, "picker-search"));
    vcx.simulate_input("haiku");
    draw(vcx);
    assert!(painted(vcx, "pick-model-claude-haiku-5-5"));
    assert!(
        !painted(vcx, "pick-model-claude-sonnet-5-5"),
        "only what matches"
    );
    assert_eq!(
        home_composer(vcx, &root).read_with(vcx, |c, _| c.text().to_owned()),
        "",
        "typing goes to the search, not to the text"
    );
    vcx.simulate_keystrokes("backspace backspace backspace backspace backspace");
    draw(vcx);
    assert!(painted(vcx, "pick-model-claude-sonnet-5-5"));
    // A name only another harness has is offered under "In other harnesses".
    vcx.simulate_input("astra");
    draw(vcx);
    assert!(painted(vcx, "pick-model-gpt-6-astra"));
    assert!(!painted(vcx, "pick-model-claude-haiku-5-5"));
}

#[gpui::test]
fn escape_and_a_click_outside_close_the_picker_and_the_text_keeps_its_keyboard(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = open_picker_on(cx, "claude-code");
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "model-picker"));
    // The keyboard is back in the text.
    draw(vcx);
    vcx.simulate_input("hi");
    draw(vcx);
    assert_eq!(
        home_composer(vcx, &root).read_with(vcx, |c, _| c.text().to_owned()),
        "hi"
    );
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "model-picker"));
    vcx.simulate_click(
        gpui::point(gpui::px(6.0), gpui::px(6.0)),
        gpui::Modifiers::none(),
    );
    vcx.run_until_parked();
    draw(vcx);
    assert!(!painted(vcx, "model-picker"));
}

#[gpui::test]
fn a_starred_model_is_kept_on_the_favourites_tab(cx: &mut TestAppContext) {
    let (store, _root, vcx) = open_picker_on(cx, "favourites");
    assert!(
        !painted(vcx, "pick-model-claude-haiku-5-5"),
        "nothing is starred yet"
    );
    click(vcx, "picker-tab-claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "star-claude-haiku-5-5"));
    click(vcx, "star-claude-haiku-5-5");
    draw(vcx);
    let claude: reno_proto::ExecutorId = "claude-code".parse().unwrap();
    assert!(store.read_with(vcx, |s, _| {
        s.composer_prefs().is_favourite(&claude, "claude-haiku-5-5")
    }));
    assert!(
        !painted(vcx, "pick-harness-auto"),
        "a star is not a pick: the star's click does not reach the row"
    );
    click(vcx, "picker-tab-favourites");
    draw(vcx);
    assert!(painted(vcx, "pick-model-claude-haiku-5-5"));
    assert!(!painted(vcx, "pick-model-claude-sonnet-5-5"));
    // Starred rows lead their harness's own list.
    click(vcx, "picker-tab-claude-code");
    draw(vcx);
    click(vcx, "star-claude-haiku-5-5");
    draw(vcx);
    assert!(!store.read_with(vcx, |s, _| {
        s.composer_prefs().is_favourite(&claude, "claude-haiku-5-5")
    }));
}

#[gpui::test]
fn a_signed_out_harness_wears_a_dot_and_the_account_row_says_who_is_signed_in(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = open_picker_on(cx, "claude-code");
    assert!(painted(vcx, "picker-tab-warn-codex"), "Codex is signed out");
    assert!(!painted(vcx, "picker-tab-warn-claude-code"));
    assert!(painted(vcx, "picker-account-value"));
    click(vcx, "picker-tab-codex");
    draw(vcx);
    assert!(painted(vcx, "picker-account"));
    click(vcx, "picker-account");
    draw(vcx);
    assert!(!painted(vcx, "model-picker"), "it leaves the picker");
    assert_eq!(
        root.read_with(vcx, |r, _| r.nav()),
        crate::nav::Nav::Executors
    );
}

#[gpui::test]
fn the_keyboard_walks_the_tabs_and_the_rows_and_enter_picks(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open_picker_on(cx, "claude-code");
    vcx.simulate_keystrokes("down down enter");
    draw(vcx);
    let picked = home_composer(vcx, &root).read_with(vcx, |c, _| c.pick().cloned());
    assert_eq!(
        picked.map(|p| p.executor.to_string()),
        Some("claude-code".to_owned())
    );
    assert!(painted(vcx, "model-picker"), "a pick keeps it open");
    vcx.simulate_keystrokes("shift-tab");
    draw(vcx);
    assert_eq!(
        home_composer(vcx, &root).read_with(vcx, |c, _| c.picker_tab().clone()),
        crate::vm::picker::Tab::Harness("acp.grok".parse().unwrap()),
        "Shift-Tab goes back one tab: Grok is listed before Claude Code"
    );
}

// --- Context window and plan usage ---------------------------------------------------------------

#[gpui::test]
fn the_context_window_is_chosen_per_model_goes_with_the_task_and_shows_on_the_run(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open_picker_on(cx, "claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-opus-5-5"));
    assert!(!painted(vcx, "picker-context"), "no model, no row");
    click(vcx, "pick-model-claude-haiku-5-5");
    draw(vcx);
    assert!(
        !painted(vcx, "picker-context"),
        "Haiku offers no choice of window"
    );
    click(vcx, "pick-model-claude-opus-5-5");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-context"));
    assert!(!painted(vcx, "context-choice-200k"), "the sizes are folded");
    click(vcx, "picker-context");
    wait_drawn(vcx, |vcx| painted(vcx, "context-choice-200k"));
    click(vcx, "context-choice-200k");
    draw(vcx);
    let composer = home_composer(vcx, &root);
    let window = |vcx: &mut gpui::VisualTestContext| {
        composer.read_with(vcx, |c, _| {
            c.pick()
                .and_then(|p| p.options.get("contextWindow").cloned())
        })
    };
    assert_eq!(window(vcx).as_deref(), Some("200k"));
    // Remembered for the model: another model and back.
    click(vcx, "pick-model-claude-sonnet-5-5");
    draw(vcx);
    assert_eq!(window(vcx), None, "Sonnet has none of its own yet");
    click(vcx, "pick-model-claude-opus-5-5");
    draw(vcx);
    assert_eq!(window(vcx).as_deref(), Some("200k"), "Opus kept its own");
    let claude: reno_proto::ExecutorId = "claude-code".parse().unwrap();
    assert_eq!(
        store.read_with(vcx, |s, _| s
            .composer_prefs()
            .options_for(&claude, "claude-opus-5-5")
            .and_then(|o| o.get("contextWindow").cloned())),
        Some("200k".to_owned())
    );

    composer.update_in(vcx, |c, window, cx| c.fill("context test", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    let run = root
        .read_with(vcx, |r, cx| {
            r.run_screen().map(|s| s.read(cx).run_id().clone())
        })
        .unwrap();
    store.read_with(vcx, |s, _| {
        assert_eq!(
            s.state().run(&run).unwrap().model_options["contextWindow"],
            "200k"
        );
    });
    // The harness reports the fill: a ring appears beside the chip, with its card.
    wait_drawn(vcx, |vcx| painted(vcx, "context-ring"));
    click(vcx, "context-ring");
    wait_drawn(vcx, |vcx| painted(vcx, "context-card"));
    click(vcx, "context-ring");
    draw(vcx);
    assert!(!painted(vcx, "context-card"), "the ring toggles its card");
}

#[gpui::test]
fn the_account_row_carries_a_usage_ring_for_a_signed_in_harness_and_its_card(
    cx: &mut TestAppContext,
) {
    let (_store, _root, vcx) = open_picker_on(cx, "claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-usage"));
    assert!(!painted(vcx, "usage-card"));
    assert!(!painted(vcx, "picker-usage-meters"));
    click(vcx, "picker-usage");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-usage-meters"));
    assert!(painted(vcx, "model-picker"), "the picker stays open");
    click(vcx, "picker-usage-meters");
    draw(vcx);
    assert!(
        painted(vcx, "model-picker"),
        "a click inside does not close it"
    );
    click(vcx, "picker-usage");
    draw(vcx);
    assert!(
        !painted(vcx, "picker-usage-meters"),
        "the ring folds it away"
    );
    click(vcx, "picker-usage");
    draw(vcx);
    // Codex is signed out in the demo: no meter, whatever the daemon carries.
    click(vcx, "picker-tab-codex");
    draw(vcx);
    assert!(!painted(vcx, "picker-usage"));
    assert!(!painted(vcx, "picker-usage-meters"));
}

#[gpui::test]
fn a_live_run_with_a_reported_window_shows_the_context_ring(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "context-ring"));
    click(vcx, "context-ring");
    wait_drawn(vcx, |vcx| painted(vcx, "context-card"));
}

#[gpui::test]
fn a_live_runs_window_is_changed_through_the_daemon_and_says_when(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
    });
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    // The demo run's own model is an alias the fake daemon cannot configure; switch to a listed
    // one first, as a person would.
    let keeper = crate::tests::run_id("keeper1");
    store.update(vcx, |s, cx| {
        s.set_run_model(keeper.clone(), "claude-opus-5-5".into(), cx);
    });
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .run(&keeper)
                .and_then(|r| r.model.clone())
                .as_deref()
                == Some("claude-opus-5-5")
        })
    });
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-context"));
    assert!(painted(vcx, "context-applies"));
    click(vcx, "picker-context");
    wait_drawn(vcx, |vcx| painted(vcx, "context-choice-200k"));
    click(vcx, "context-choice-200k");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .run(&keeper)
                .and_then(|r| r.model_options.get("contextWindow").cloned())
                .as_deref()
                == Some("200k")
        })
    });
    assert!(store.read_with(vcx, |s, _| s.config_applied(&keeper).options.is_some()));
}

#[gpui::test]
fn grok_lists_its_levels_on_home_and_its_window_is_read_only(cx: &mut TestAppContext) {
    let (_store, _root, vcx) = open_picker_on(cx, "acp.grok");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-grok-4"));
    click(vcx, "pick-model-grok-4");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-cell-xhigh"));
    assert!(
        painted(vcx, "effort-cell-low")
            && painted(vcx, "effort-cell-medium")
            && painted(vcx, "effort-cell-high"),
        "Grok's four levels"
    );
    assert!(!painted(vcx, "effort-cell-max"));
    assert!(painted(vcx, "picker-context"), "the window shows its size");
    click(vcx, "picker-context");
    draw(vcx);
    assert!(
        !painted(vcx, "context-choice-256k"),
        "a read-only window has no sizes to pick"
    );
}
