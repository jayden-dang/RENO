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
    assert!(painted(vcx, "route-name"));
    assert!(!painted(vcx, "why-panel"));
    click(vcx, "why");
    wait_drawn(vcx, |vcx| painted(vcx, "why-panel"));
    assert!(composer.read_with(vcx, |c, _| c.why_is_open()));
    assert!(painted(vcx, "rejected-0"));
    let Preview::Ready(d) = composer.read_with(vcx, |c, _| c.preview().clone()) else {
        panic!("ready");
    };
    let view = crate::vm::routing::view(&d);
    assert_eq!(view.rejected[0].who.name, "Codex");
    assert_eq!(view.rejected[0].reason, "Signed out. Run `codex login`.");
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
    wait_drawn(vcx, |vcx| !painted(vcx, "stop") && !painted(vcx, "send"));
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
