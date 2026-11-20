//! ROAD-31: the composer's completions, the message queue and where a new task runs, against the
//! fake daemon's mocked contracts ("real" is the fake with the real daemon's capabilities). Each
//! test locks one decided thing; they assert words and state, never pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::places::{Checkout, Place};

use crate::activity_tests::{following, open_run};
use crate::store::Store;
use crate::tests::{approvals, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::composer::Composer;
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::places;

/// The new-thread box of a connected fake with `caps`.
fn home(
    cx: &mut TestAppContext,
    caps: Capabilities,
) -> (
    Entity<Store>,
    Entity<crate::app::RootView>,
    Entity<Composer>,
    &mut VisualTestContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    store.update(cx, |s, cx| s.set_capabilities(caps, cx));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    (store, root, composer, vcx)
}

fn type_in(vcx: &mut VisualTestContext, c: &Entity<Composer>, text: &str) {
    c.update_in(vcx, |c, window, cx| c.fill(text, window, cx));
    draw(vcx);
}

fn sentence(vcx: &mut VisualTestContext, c: &Entity<Composer>) -> String {
    c.read_with(vcx, |c, cx| {
        let p = c.place_view(cx).expect("the place chooser is on");
        places::sentence(&p.project.name, &p.branch, &p.checkout).text()
    })
}

#[gpui::test]
fn the_new_thread_sentence_says_where_it_runs_and_follows_the_pickers(cx: &mut TestAppContext) {
    let (_store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    assert_eq!(
        sentence(vcx, &c),
        "What should we build in reno on main in a new worktree?"
    );
    assert!(painted(vcx, "place-project") && painted(vcx, "place-branch"));
    assert!(painted(vcx, "place-checkout"), "each word opens its picker");
    c.update_in(vcx, |c, w, cx| {
        c.place_choose_for_test(
            "reno",
            Some("fix/model-chip"),
            Some(Checkout::Worktree("reno-model-chip".into())),
            w,
            cx,
        );
    });
    assert_eq!(
        sentence(vcx, &c),
        "What should we build in reno on fix/model-chip in the worktree reno-model-chip?"
    );
    // A new project starts again from its own default branch in a new worktree.
    c.update_in(vcx, |c, w, cx| {
        c.place_choose_for_test("atlas-api", None, None, w, cx)
    });
    assert_eq!(
        sentence(vcx, &c),
        "What should we build in atlas-api on main in a new worktree?"
    );
}

#[gpui::test]
fn the_send_passes_the_chosen_project_branch_and_checkout(cx: &mut TestAppContext) {
    let (store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    let before = store.read_with(vcx, |s, _| s.state().runs().len());
    c.update_in(vcx, |c, w, cx| {
        c.place_choose_for_test(
            "atlas-api",
            Some("release/2.1"),
            Some(Checkout::ThisCheckout),
            w,
            cx,
        );
        c.fill("bump the lockfile", w, cx);
        c.submit(w, cx);
    });
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| s.state().runs().len() > before)
    });
    let run = store.read_with(vcx, |s, _| {
        s.state()
            .runs()
            .into_iter()
            .find(|r| r.prompt.as_deref() == Some("bump the lockfile"))
            .map(|r| (*r).clone())
            .expect("the task started")
    });
    assert_eq!(
        run.cwd.as_deref(),
        Some(std::path::Path::new("/demo/atlas-api")),
        "the project is the run's folder"
    );
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| s.run_place(&run.run_id).is_some())
    });
    assert_eq!(
        store.read_with(vcx, |s, _| s.run_place(&run.run_id)),
        Some(Place {
            project: "atlas-api".into(),
            branch: "release/2.1".into(),
            checkout: Checkout::ThisCheckout,
        })
    );
}

#[gpui::test]
fn a_failed_clone_says_why_inline_and_keeps_the_url(cx: &mut TestAppContext) {
    let (_store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    let url = "https://example.com/nope.git";
    c.update_in(vcx, |c, w, cx| c.place_clone_for_test(url, w, cx));
    assert_eq!(
        c.read_with(vcx, |c, _| c.place_clone_state_for_test()),
        Some((url.to_owned(), None)),
        "cloning, with the URL still on show"
    );
    vcx.executor().advance_clock(Duration::from_millis(800));
    vcx.run_until_parked();
    assert_eq!(
        c.read_with(vcx, |c, _| c.place_clone_state_for_test()),
        Some((
            url.to_owned(),
            Some("Could not clone: repository not found".to_owned())
        ))
    );
    draw(vcx);
    assert!(
        painted(vcx, "clone-failed"),
        "the failure is drawn in the picker"
    );
    // The sentence did not move.
    assert_eq!(
        sentence(vcx, &c),
        "What should we build in reno on main in a new worktree?"
    );
}

#[gpui::test]
fn at_lists_files_in_the_popover_and_never_takes_focus_or_loses_text(cx: &mut TestAppContext) {
    let (_store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    vcx.update(|window, cx| window.focus(&c.read(cx).focus_handle_ref().clone(), cx));
    type_in(vcx, &c, "look at @src/");
    let labels = c.read_with(vcx, |c, _| c.completion_labels()).unwrap();
    assert!(labels.iter().any(|l| l == "src/lib.rs"), "{labels:?}");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_groups()), ["Files"]);
    assert!(painted(vcx, "complete-popover"));
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "look at @src/"
    );
    vcx.update(|window, cx| assert!(c.read(cx).is_focused(window)));

    // Esc closes it and keeps the text; typing on in the same token does not reopen it.
    assert!(c.update(vcx, |c, cx| c.completion_dismiss_for_test(cx)));
    assert_eq!(c.read_with(vcx, |c, _| c.completion_labels()), None);
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "look at @src/"
    );
    type_in(vcx, &c, "look at @src/l");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_labels()), None);

    // Enter puts the row in place of the token.
    type_in(vcx, &c, "read @src/li");
    assert!(c.update_in(vcx, |c, w, cx| c.completion_accept_for_test(w, cx)));
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "read @src/lib.rs "
    );
}

#[gpui::test]
fn slash_lists_the_harness_commands_and_the_app_commands_in_their_groups(cx: &mut TestAppContext) {
    let (_store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    type_in(vcx, &c, "/");
    let labels = c.read_with(vcx, |c, _| c.completion_labels()).unwrap();
    assert!(labels.contains(&"/compact".to_owned()), "{labels:?}");
    assert!(labels.contains(&"/model".to_owned()), "{labels:?}");
    assert_eq!(
        c.read_with(vcx, |c, _| c.completion_groups()),
        ["Claude Code", "Reno"]
    );
    // Only what does something is listed: no command that would say "not wired".
    assert!(
        !labels
            .iter()
            .any(|l| l == "/new" || l == "/diff" || l == "/rename")
    );
    // A path is not a command.
    type_in(vcx, &c, "src/main.rs");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_labels()), None);
    // `$` lists the skills in their own group.
    type_in(vcx, &c, "$");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_groups()), ["Skills"]);
}

#[gpui::test]
fn unreadable_harness_commands_show_the_failure_line_and_still_list_the_app_commands(
    cx: &mut TestAppContext,
) {
    let (store, _root, c, vcx) = home(cx, Capabilities::FAKE);
    store.update(vcx, |s, _| s.set_commands_unreadable(true));
    type_in(vcx, &c, "/");
    assert_eq!(
        c.read_with(vcx, |c, _| c.completion_failure_line())
            .as_deref(),
        Some("Claude Code's commands could not be read")
    );
    assert!(painted(vcx, "complete-failure"));
    let labels = c.read_with(vcx, |c, _| c.completion_labels()).unwrap();
    assert!(labels.contains(&"/model".to_owned()) && !labels.contains(&"/compact".to_owned()));
}

#[gpui::test]
fn the_queue_card_counts_what_waits_in_words(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let steer = root.read_with(vcx, |r, cx| {
        r.run_screen().unwrap().read(cx).steer_input().clone()
    });
    assert_eq!(
        steer
            .read_with(vcx, |c, cx| c.queue_finding_for_test(cx))
            .as_deref(),
        Some("2 messages wait for this turn to end")
    );
    wait_drawn(vcx, |vcx| painted(vcx, "queue-card"));
    assert!(painted(vcx, "queue-finding"));
    assert!(painted(vcx, "queue-row-0") && painted(vcx, "queue-row-1"));
    assert!(painted(vcx, "queue-handle-0") && painted(vcx, "queue-edit-btn-0"));
    assert!(painted(vcx, "queue-remove-0") && painted(vcx, "queue-send-now-0"));
}

#[gpui::test]
fn add_edit_remove_and_send_now_each_change_the_queue(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let steer = root.read_with(vcx, |r, cx| {
        r.run_screen().unwrap().read(cx).steer_input().clone()
    });
    let texts =
        |vcx: &mut VisualTestContext| steer.read_with(vcx, |c, cx| c.queue_texts_for_test(cx));
    let first = texts(vcx)[0].clone();
    let second = texts(vcx)[1].clone();

    // Add: the draft joins the end and the box empties.
    steer.update_in(vcx, |c, w, cx| {
        c.fill("Then tag the release", w, cx);
        c.queue_add_for_test(w, cx);
    });
    assert_eq!(
        texts(vcx),
        [
            first.clone(),
            second.clone(),
            "Then tag the release".to_owned()
        ]
    );
    assert_eq!(steer.read_with(vcx, |c, _| c.text().to_owned()), "");
    assert_eq!(
        steer
            .read_with(vcx, |c, cx| c.queue_finding_for_test(cx))
            .as_deref(),
        Some("3 messages wait for this turn to end")
    );

    // Edit: the message keeps its place.
    steer.update_in(vcx, |c, w, cx| {
        c.queue_edit_for_test(1, "Then update the fixtures", w, cx)
    });
    assert_eq!(texts(vcx)[1], "Then update the fixtures");
    assert_eq!(texts(vcx)[0], first);

    // Remove: gone, and not sent.
    steer.update_in(vcx, |c, w, cx| c.queue_remove_for_test(0, w, cx));
    assert_eq!(
        texts(vcx),
        ["Then update the fixtures", "Then tag the release"]
    );

    // Send now: it leaves the queue and reaches the transcript at once.
    steer.update_in(vcx, |c, w, cx| c.queue_send_now_for_test(1, w, cx));
    assert_eq!(texts(vcx), ["Then update the fixtures"]);
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().transcript(&run_id("plan1")).is_some_and(|t| {
                t.events.iter().any(|e| {
                    matches!(&e.1, reno_proto::ExecEvent::UserMessage { text } if text == "Then tag the release")
                })
            })
        })
    });
    assert_eq!(
        steer
            .read_with(vcx, |c, cx| c.queue_finding_for_test(cx))
            .as_deref(),
        Some("1 message waits for this turn to end")
    );
}

#[gpui::test]
fn an_add_on_an_ended_run_shows_the_error(cx: &mut TestAppContext) {
    let store = following(cx);
    let ended = store.read_with(cx, |s, _| {
        s.state()
            .runs()
            .iter()
            .find(|r| r.status.is_terminal())
            .map(|r| r.run_id.clone())
            .expect("the demo data has a finished run")
    });
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    let steer = vcx.new(|cx| Composer::steer(store.clone(), ended, cx));
    steer.update_in(vcx, |c, w, cx| {
        c.fill("one more thing", w, cx);
        c.queue_add_for_test(w, cx);
    });
    assert_eq!(
        steer
            .read_with(vcx, |c, _| c.queue_error_for_test())
            .as_deref(),
        Some("The run ended; send it as a new message instead")
    );
    assert_eq!(
        steer.read_with(vcx, |c, _| c.text().to_owned()),
        "one more thing",
        "the owner's words stay in the box"
    );
}

#[gpui::test]
fn the_real_daemon_hides_the_mocked_parts_and_keeps_the_real_ones(cx: &mut TestAppContext) {
    let (store, root, c, vcx) = home(cx, Capabilities::REAL);
    // New-thread page: the heading and the chip strip as they always were; no place chooser.
    assert!(c.read_with(vcx, |c, cx| c.place_view(cx).is_none()));
    assert_eq!(
        root.read_with(vcx, |r, cx| r.home().read(cx).heading().text()),
        "What should we build?"
    );
    assert!(painted(vcx, "new-thread-chip-this-mac") && !painted(vcx, "place-project"));
    // `/` offers the app's commands and none of the harness's; `$` and a new thread's `@` offer
    // nothing.
    type_in(vcx, &c, "/");
    let labels = c.read_with(vcx, |c, _| c.completion_labels()).unwrap();
    assert!(labels.contains(&"/model".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"/compact".to_owned()), "{labels:?}");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_failure_line()), None);
    type_in(vcx, &c, "$");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_labels()), None);
    type_in(vcx, &c, "@src/");
    assert_eq!(c.read_with(vcx, |c, _| c.completion_labels()), None);
    // The store offers no queue, skills or places.
    let run = run_id("plan1");
    store.read_with(vcx, |s, _| {
        assert!(s.queue(&run).is_none());
        assert!(s.place_desk().is_none());
        assert!(s.harness_commands("claude-code").is_none());
        assert!(s.new_thread_files("reno", "", 5).is_none());
    });
    // A live run's `@` is the real file search, and `/stop` is there to stop it.
    let live = store.read_with(vcx, |s, _| {
        s.state()
            .runs()
            .iter()
            .find(|r| !r.status.is_terminal())
            .map(|r| r.run_id.clone())
            .expect("a live demo run")
    });
    let steer = vcx.new(|cx| Composer::steer(store.clone(), live, cx));
    steer.update_in(vcx, |c, w, cx| c.fill("@src/", w, cx));
    wait_until(vcx, |cx| {
        steer.read_with(cx, |c, _| {
            c.completion_labels()
                .is_some_and(|l| l.iter().any(|f| f == "src/main.rs"))
        })
    });
    // And the queue card, with no queue, says nothing.
    assert_eq!(
        steer.read_with(vcx, |c, cx| c.queue_finding_for_test(cx)),
        None
    );
}
