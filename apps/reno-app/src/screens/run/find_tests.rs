//! ROAD-33: the transcript as a flat list of items, find in the conversation, and the jump to
//! the live end, on the seeded "fix the ACP v2 handshake" thread of the fake daemon.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{AppContext as _, Entity, ListOffset, TestAppContext, VisualTestContext, px};
use reno_client::Update;
use reno_client::fake::FakeConfig;
use reno_proto::{ActionClass, ExecEvent, RunId, SessionRef, StopReason, ToolCall};

use super::RunScreen;
use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    approvals, click, draw, fake_store, one_window_test_at_a_time, painted, run_id, wait_until,
    window,
};
use crate::transcript::{Key, Row};
use crate::ui::theme::Appearance;
use crate::vm::find::Part;

/// `wait_drawn` that says what it was waiting for.
fn wait_for(
    vcx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    vcx.executor().allow_parking();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        draw(vcx);
        if done(vcx) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

type Opened<'a> = (
    Entity<Store>,
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
);

fn bind(cx: &mut TestAppContext) {
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        crate::ui::composer::bind_keys(cx);
        super::bind_keys(cx);
    });
}

/// The fake daemon with the seeded threads, and a window over it.
fn setup(cx: &mut TestAppContext) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                step: Duration::ZERO,
                transcript_threads: true,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.thread_of(&run_id("acp8")).is_some())
    });
    bind(cx);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    (store, root, vcx)
}

/// The handshake thread's last run opened, with every turn's fold in.
fn open(cx: &mut TestAppContext) -> Opened<'_> {
    let (store, root, vcx) = setup(cx);
    root.update_in(vcx, |r, window, cx| r.open_run(run_id("acp8"), window, cx));
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    wait_for(vcx, "folds(vcx, &screen).len() == 8", |vcx| {
        folds(vcx, &screen).len() == 8
    });
    (store, root, vcx, screen)
}

/// The keys of the top-level fold rows, and how many rows each holds.
fn folds(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Vec<(Key, usize)> {
    screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .filter_map(|r| match r {
                Row::Worked { key, rows, .. } => Some((*key, rows.len())),
                _ => None,
            })
            .collect()
    })
}

fn counts(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> (usize, usize, usize) {
    screen.read_with(vcx, |s, _| {
        (s.rows().len(), s.items().len(), s.item_count())
    })
}

fn type_query(vcx: &mut VisualTestContext, query: &str) {
    let keys: Vec<String> = query.chars().map(String::from).collect();
    vcx.simulate_keystrokes(&keys.join(" "));
    draw(vcx);
}

/// Opens find (⌘F), types `supervisor.rs` and waits for the count.
fn find_the_needle(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>) {
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    // The query is kept when the bar closes: typing replaces it.
    vcx.simulate_keystrokes("cmd-a");
    type_query(vcx, "supervisor.rs");
    assert_eq!(count(vcx, screen).as_deref(), Some("1 of 7"));
}

fn count(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Option<String> {
    screen.read_with(vcx, |s, _| s.find_count_label())
}

// --- the flat list --------------------------------------------------------------------------

fn an_open_fold_puts_its_rows_in_the_list_as_items_of_their_own(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    let (rows, items, list) = counts(vcx, &screen);
    assert_eq!(items, rows, "closed, every fold is one item");
    assert_eq!(list, items, "the list draws the items");

    let (fold, inside) = *folds(vcx, &screen).last().unwrap();
    wait_for(
        vcx,
        "painted(vcx, &format!(\"worked-{{}}\", fold.0))",
        |vcx| painted(vcx, &format!("worked-{}", fold.0)),
    );
    click(vcx, &format!("worked-{}", fold.0));
    draw(vcx);
    let (rows_open, items_open, list_open) = counts(vcx, &screen);
    assert_eq!(rows_open, rows, "the nested rows do not change");
    assert_eq!(items_open, rows + inside, "the fold's rows are items now");
    assert_eq!(list_open, items_open);
    screen.read_with(vcx, |s, _| {
        let head = s.items().item_of(fold).unwrap();
        let first = match &s.rows()[s.items().items[head].top] {
            Row::Worked { rows, .. } => rows[0].key(),
            _ => unreachable!(),
        };
        // Keys map to items; the rows come right after the head, in the fold's own row.
        assert_eq!(s.items().item_of(first), Some(head + 1));
        assert_eq!(
            s.items().top_of(head + 1, s.rows().len()),
            s.items().items[head].top
        );
        assert_eq!(s.items().items[head + 1].inner, Some(0));
    });

    // The view left the fold's head behind when it opened, so the owner's click is a toggle here.
    screen.update(vcx, |s, cx| s.toggle(fold, cx));
    draw(vcx);
    assert_eq!(
        counts(vcx, &screen),
        (rows, items, list),
        "closing puts it back"
    );
}

fn the_top_row_stays_a_top_level_row_index_while_a_fold_is_open(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    let (fold, _) = *folds(vcx, &screen).last().unwrap();
    wait_for(
        vcx,
        "painted(vcx, &format!(\"worked-{{}}\", fold.0))",
        |vcx| painted(vcx, &format!("worked-{}", fold.0)),
    );
    click(vcx, &format!("worked-{}", fold.0));
    draw(vcx);
    screen.update(vcx, |s, cx| {
        let head = s.items().item_of(fold).unwrap();
        let top = s.items().items[head].top;
        s.list.scroll_to(ListOffset {
            item_ix: head + 40,
            offset_in_item: px(0.0),
        });
        assert_eq!(s.top_row(), top, "an item inside a fold is its fold's row");
        s.list.scroll_to(ListOffset {
            item_ix: s.items().item_of_top(top + 1),
            offset_in_item: px(0.0),
        });
        assert_eq!(s.top_row(), top + 1);
        cx.notify();
    });
}

fn a_reply_that_grows_is_measured_in_place_and_the_view_does_not_move(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    let (fold, _) = *folds(vcx, &screen).last().unwrap();
    // The previous check left the fold open.
    assert!(screen.read_with(vcx, |s, _| s.is_open(fold)));
    // The view rests 7 px into an item inside the fold, and that item's text grows.
    let before = screen.update(vcx, |s, cx| {
        let head = s.items().item_of(fold).unwrap();
        let ix = head + 3;
        s.list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(7.0),
        });
        cx.notify();
        ix
    });
    draw(vcx);
    let at = screen.read_with(vcx, |s, _| s.list.logical_scroll_top());
    assert_eq!(at.item_ix, before);
    screen.update(vcx, |s, cx| {
        let from = s.prefix_len;
        let mut tail = s.rows()[from..].to_vec();
        let top = s.items().items[before].top - from;
        let inner = s.items().items[before].inner.unwrap();
        let Row::Worked { rows, .. } = &mut tail[top] else {
            unreachable!()
        };
        match &mut rows[inner] {
            Row::Prose { text, .. } | Row::Reasoning { text, .. } => text.push_str(" and more"),
            Row::Tools { calls, .. } => calls[0].label.push_str(" --more"),
            other => panic!("{other:?}"),
        }
        let count = s.item_count();
        s.replace_rows(from, Some(tail), cx);
        assert_eq!(s.item_count(), count, "no item came or went");
    });
    let after = screen.read_with(vcx, |s, _| s.list.logical_scroll_top());
    assert_eq!(after, at, "the item under the anchor was not replaced");
}

fn a_turn_that_folds_keeps_the_reading_place_below_it_and_lands_on_the_head(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    screen.update(vcx, |s, cx| {
        let from = s.prefix_len;
        let folded = s.rows()[from..].to_vec();
        // The same turn before it finished: its interim rows loose, and no Done.
        let mut live: Vec<Row> = Vec::new();
        for row in folded.iter().cloned() {
            match row {
                Row::Worked { rows, .. } => live.extend(rows),
                Row::Done { .. } => {}
                other => live.push(other),
            }
        }
        s.replace_rows(from, Some(live.clone()), cx);
        let loose = s.item_count();
        let fold_top = from
            + folded
                .iter()
                .position(|r| matches!(r, Row::Worked { .. }))
                .unwrap();
        let fold_item = s.items().item_of_top(fold_top);
        // The view rests inside what is about to fold.
        s.list.scroll_to(ListOffset {
            item_ix: fold_item + 20,
            offset_in_item: px(0.0),
        });
        assert_eq!(s.top_row(), fold_top + 20, "loose rows are top-level rows");
        s.replace_rows(from, Some(folded.clone()), cx);
        assert!(s.item_count() < loose, "the interim rows folded");
        // It landed on the fold's head, where the rows that folded were.
        assert_eq!(s.list.logical_scroll_top().item_ix, fold_item);
        assert_eq!(s.top_row(), fold_top);
        // The final message and the Done are items after it, as they were.
        assert_eq!(s.item_count(), s.items().len());
    });
}

/// A fold's head stays the one line "Worked for <duration>" and its chevron: no count, no chips,
/// no card around it.
fn a_fold_head_is_the_single_line_worked_for_its_duration(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let (fold, _) = *folds(vcx, screen).last().unwrap();
    let selector: &'static str = Box::leak(format!("worked-{}", fold.0).into_boxed_str());
    wait_for(vcx, "the last fold's head is painted", |vcx| {
        painted(vcx, selector)
    });
    let (label, head) = screen.read_with(vcx, |s, _| {
        let Some(Row::Worked { secs, .. }) = s.rows().iter().find(|r| r.key() == fold) else {
            panic!("a fold")
        };
        (
            crate::vm::convo::worked_label(*secs),
            s.painted_head(fold).expect("the head's words are painted"),
        )
    });
    assert!(label.starts_with("Worked for "), "{label}");
    assert_eq!(head.0, label);
    // Its words are the head's only text: no count, no tool names with them.
    let texts: Vec<_> = screen.read_with(vcx, |s, _| {
        s.painted_pieces()
            .into_iter()
            .filter(|(key, _, _)| *key == fold)
            .map(|(_, slot, _)| slot)
            .collect()
    });
    assert_eq!(texts, [crate::vm::selection::Slot::HEAD]);
}

// --- find -------------------------------------------------------------------------------------

fn find_counts_seven_and_the_first_is_turn_threes_cargo_test_step(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    find_the_needle(vcx, &screen);
    screen.read_with(vcx, |s, _| {
        let found = s.find_found();
        assert_eq!(found.hits.len(), 7);
        let first = &found.hits[0];
        assert_eq!(first.turn, 3, "turn 3's cargo test step");
        assert!(matches!(first.part, Part::Output(_)));
        assert!(first.fold.is_some(), "it is inside a closed fold");
        // Nothing opened by counting.
        assert!(!s.is_open(first.fold.unwrap()));
        assert!(!s.search_holds());
    });
}

fn enter_opens_the_fold_at_the_step_and_escape_closes_exactly_what_search_opened(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    find_the_needle(vcx, &screen);
    let (fold, call) = screen.read_with(vcx, |s, _| {
        let hit = &s.find_found().hits[0];
        let fold = hit.fold.unwrap();
        let Some(Row::Tools { calls, .. }) = s.hit_row(hit) else {
            panic!("the first hit is in a tool group")
        };
        let Part::Output(n) = hit.part else {
            unreachable!()
        };
        (fold, calls[n].id.clone())
    });
    // The first Enter goes to the first hit rather than past it.
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    screen.read_with(vcx, |s, _| {
        assert!(s.is_open(fold), "the fold that hides the hit is open");
        assert!(s.output_is_open(&call), "and so is the step's output");
        assert!(s.search_holds());
        assert!(s.items().item_of(s.find_found().hits[0].key).is_some());
    });
    // Enter twice more: "3 of 7".
    vcx.simulate_keystrokes("enter enter");
    assert_eq!(count(vcx, &screen).as_deref(), Some("3 of 7"));
    // ⇧Enter goes back, ⌘G forward; both wrap.
    vcx.simulate_keystrokes("shift-enter");
    assert_eq!(count(vcx, &screen).as_deref(), Some("2 of 7"));
    vcx.simulate_keystrokes("cmd-g");
    assert_eq!(count(vcx, &screen).as_deref(), Some("3 of 7"));
    vcx.simulate_keystrokes("shift-cmd-g shift-cmd-g shift-cmd-g");
    assert_eq!(count(vcx, &screen).as_deref(), Some("7 of 7"));
    // Escape: the bar closes and the fold and the output close again.
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    screen.read_with(vcx, |s, _| {
        assert!(!s.find_is_open());
        assert!(!s.is_open(fold), "the fold search opened is closed again");
        assert!(!s.output_is_open(&call));
        assert!(!s.search_holds());
    });
    assert!(!painted(vcx, "find-bar"));
    assert_eq!(count(vcx, &screen), None);
}

fn a_fold_the_owner_opened_stays_open_and_a_row_the_owner_touches_becomes_theirs(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    // Turn 8's fold is on screen: the owner opens it. Turn 3's fold is search's to open.
    screen.update(vcx, |s, cx| s.jump_to_latest(cx));
    draw(vcx);
    let (turn8, _) = *folds(vcx, &screen).last().unwrap();
    wait_for(
        vcx,
        "painted(vcx, &format!(\"worked-{{}}\", turn8.0))",
        |vcx| painted(vcx, &format!("worked-{}", turn8.0)),
    );
    click(vcx, &format!("worked-{}", turn8.0));
    draw(vcx);
    find_the_needle(vcx, &screen);
    let turn3 = screen.read_with(vcx, |s, _| s.find_found().hits[0].fold.unwrap());
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert!(screen.read_with(vcx, |s, _| s.is_open(turn3) && s.is_open(turn8)));
    // The owner closes the fold search opened: it is theirs now, and stays closed.
    screen.update(vcx, |s, cx| s.toggle(turn3, cx));
    draw(vcx);
    screen.read_with(vcx, |s, _| {
        assert!(!s.is_open(turn3));
    });
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    screen.read_with(vcx, |s, _| {
        assert!(s.is_open(turn8), "what the owner opened stays open");
        assert!(!s.is_open(turn3), "what the owner closed stays closed");
        assert!(!s.search_holds());
    });
}

fn the_count_follows_changes_once_per_150_ms_and_keeps_the_current_match(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    find_the_needle(vcx, &screen);
    vcx.simulate_keystrokes("enter enter enter");
    let (recounts, current) =
        screen.read_with(vcx, |s, _| (s.find_recounts(), s.find_found().current));
    assert_eq!(current, Some(2));
    // Events arrive, rows open and close: each arms one recount, not one each.
    screen.update(vcx, |s, cx| {
        for _ in 0..5 {
            s.find_note_change(cx);
        }
    });
    vcx.executor().advance_clock(Duration::from_millis(149));
    vcx.run_until_parked();
    assert_eq!(screen.read_with(vcx, |s, _| s.find_recounts()), recounts);
    vcx.executor().advance_clock(Duration::from_millis(2));
    vcx.run_until_parked();
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.find_recounts(), recounts + 1, "once");
        assert_eq!(
            s.find_found().current,
            Some(2),
            "the current match did not move"
        );
        assert_eq!(s.find_found().hits.len(), 7);
    });
    // Nothing is armed when nothing changed.
    vcx.executor().advance_clock(Duration::from_millis(1000));
    vcx.run_until_parked();
    assert_eq!(
        screen.read_with(vcx, |s, _| s.find_recounts()),
        recounts + 1
    );
}

// --- Escape ------------------------------------------------------------------------------------

#[gpui::test]
fn with_the_bar_open_escape_closes_it_and_neither_denies_an_approval_nor_stops_the_run(
    cx: &mut TestAppContext,
) {
    bind(cx);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_for(vcx, "painted(vcx, \"approve\")", |vcx| {
        painted(vcx, "approve")
    });
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    // Open: Escape is the bar's, and the approval is still waiting. Closed: the panel that has
    // the keyboard denies on Escape (3.13).
    // The approval panel is on screen in the box's place: the keyboard starts there.
    let panel = screen.read_with(vcx, |s, _| s.approval_focus.clone());
    vcx.update(|window, cx| window.focus(&panel, cx));
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    assert!(screen.read_with(vcx, |s, _| s.find_is_open()));
    vcx.simulate_keystrokes("cmd-j");
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(
        !screen.read_with(vcx, |s, _| s.find_is_open()),
        "the bar took it"
    );
    assert_eq!(approvals(vcx, &store), 2, "the approval was not denied");
    // Now the bar is closed: the same key denies, as it always did.
    vcx.simulate_keystrokes("cmd-j");
    vcx.simulate_keystrokes("escape");
    wait_for(vcx, "approvals(vcx, &store) == 1", |vcx| {
        approvals(vcx, &store) == 1
    });
}

#[gpui::test]
fn escape_from_the_composer_closes_the_bar_before_it_stops_the_run(cx: &mut TestAppContext) {
    bind(cx);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let run = run_id("builder1");
    root.update_in(vcx, |r, window, cx| r.open_run(run.clone(), window, cx));
    draw(vcx);
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.escape_stops = true));
    let live = |vcx: &mut VisualTestContext| {
        store.read_with(vcx, |s, _| {
            s.state().run(&run).is_some_and(|r| !r.status.is_terminal())
        })
    };
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let steer = screen.read_with(vcx, |s, cx| {
        s.steer_input().clone().read(cx).focus_handle_ref().clone()
    });
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    // The keyboard goes back to the box while the bar stays open.
    vcx.update(|window, cx| window.focus(&steer, cx));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!screen.read_with(vcx, |s, _| s.find_is_open()));
    assert!(live(vcx), "the first Escape was the bar's");
    vcx.update(|window, cx| window.focus(&steer, cx));
    vcx.simulate_keystrokes("escape");
    wait_for(vcx, "!live(vcx)", |vcx| !live(vcx));
}

// --- jump to latest ----------------------------------------------------------------------------

fn jump_to_latest_shows_off_the_tail_and_takes_the_view_back_to_following(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
    assert!(
        !painted(vcx, "jump-latest"),
        "at the tail there is nothing to jump to"
    );
    screen.update(vcx, |s, cx| {
        s.list.scroll_by(px(-2000.0));
        cx.notify();
    });
    wait_for(vcx, "painted(vcx, \"jump-latest\")", |vcx| {
        painted(vcx, "jump-latest")
    });
    assert!(!screen.read_with(vcx, |s, _| s.is_following_tail()));
    // A round button: its box is square, and small beside the transcript.
    let button = vcx.debug_bounds("jump-latest").unwrap();
    assert_eq!(button.size.width, button.size.height, "round, so square");
    let transcript = vcx.debug_bounds("transcript").unwrap();
    assert!(button.size.width < transcript.size.width / 4.0, "a button");
    click(vcx, "jump-latest");
    draw(vcx);
    assert!(
        screen.read_with(vcx, |s, _| s.is_following_tail()),
        "it follows again"
    );
    wait_for(vcx, "!painted(vcx, \"jump-latest\")", |vcx| {
        !painted(vcx, "jump-latest")
    });
}

fn opening_a_fold_while_following_stops_following(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
    let (fold, _) = *folds(vcx, &screen).last().unwrap();
    wait_for(
        vcx,
        "painted(vcx, &format!(\"worked-{{}}\", fold.0))",
        |vcx| painted(vcx, &format!("worked-{}", fold.0)),
    );
    click(vcx, &format!("worked-{}", fold.0));
    draw(vcx);
    assert!(
        !screen.read_with(vcx, |s, _| s.is_following_tail()),
        "what grew stays in view"
    );
    wait_for(vcx, "painted(vcx, \"jump-latest\")", |vcx| {
        painted(vcx, "jump-latest")
    });
}

// --- the find strip ------------------------------------------------------------------------

/// ⌘F opens a strip pinned at the top of the transcript with the query field, "N of M",
/// previous and next, and close; the buttons act and Escape closes it.
fn the_find_strip_sits_above_the_transcript_with_its_field_count_and_buttons(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) {
    let screen = screen.clone();
    assert!(!painted(vcx, "find-bar"), "closed until asked for");
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    assert!(painted(vcx, "find-bar"), "⌘F opens the strip");
    assert!(painted(vcx, "find-query"), "with the query field");
    let (bar, transcript) = (
        vcx.debug_bounds("find-bar").unwrap(),
        vcx.debug_bounds("transcript").unwrap(),
    );
    assert!(
        bar.bottom() <= transcript.top(),
        "pinned above the transcript, not over it"
    );
    vcx.simulate_keystrokes("cmd-a");
    type_query(vcx, "supervisor.rs");
    wait_for(
        vcx,
        "count(vcx, &screen).as_deref() == Some(\"1 of 7\")",
        |vcx| count(vcx, &screen).as_deref() == Some("1 of 7"),
    );
    assert!(painted(vcx, "find-count"), "the count is on the strip");
    for button in ["find-prev", "find-next", "find-close"] {
        assert!(painted(vcx, button), "{button}");
    }
    click(vcx, "find-next");
    click(vcx, "find-next");
    assert_eq!(count(vcx, &screen).as_deref(), Some("2 of 7"));
    click(vcx, "find-prev");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    click(vcx, "find-close");
    draw(vcx);
    assert!(!painted(vcx, "find-bar"), "the close button closes it");
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    assert!(painted(vcx, "find-bar"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "find-bar"), "Escape closes it");
}

#[gpui::test]
fn find_opens_a_strip_pinned_above_the_transcript(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    the_find_strip_sits_above_the_transcript_with_its_field_count_and_buttons(vcx, &screen);
}

// The seeded world takes a while to load, so the checks that need it run in two sequences, one
// world each, in an order that leaves the screen ready for the next.

#[gpui::test]
fn the_flat_list_keeps_the_anchor_and_follows_the_tail(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    an_open_fold_puts_its_rows_in_the_list_as_items_of_their_own(vcx, &screen);
    a_fold_head_is_the_single_line_worked_for_its_duration(vcx, &screen);
    the_top_row_stays_a_top_level_row_index_while_a_fold_is_open(vcx, &screen);
    a_reply_that_grows_is_measured_in_place_and_the_view_does_not_move(vcx, &screen);
    // Close the fold again, back to the tail, and the checks that need a settled list first.
    let (fold, _) = *folds(vcx, &screen).last().unwrap();
    screen.update(vcx, |s, cx| {
        s.toggle(fold, cx);
        s.jump_to_latest(cx);
    });
    draw(vcx);
    jump_to_latest_shows_off_the_tail_and_takes_the_view_back_to_following(vcx, &screen);
    opening_a_fold_while_following_stops_following(vcx, &screen);
    // The fold that check opened closes; the turn then folds as it finishes.
    screen.update(vcx, |s, cx| s.toggle(fold, cx));
    draw(vcx);
    a_turn_that_folds_keeps_the_reading_place_below_it_and_lands_on_the_head(vcx, &screen);
}

/// Hands `events` to the store as the daemon would send them, one after the other.
fn stream<C: gpui::AppContext>(
    cx: &mut C,
    store: &Entity<Store>,
    run: &RunId,
    events: Vec<ExecEvent>,
) {
    for event in events {
        store.update(cx, |s, cx| {
            let seq = s
                .state()
                .transcript(run)
                .and_then(|t| t.highest)
                .and_then(reno_proto::Seq::next)
                .unwrap_or(reno_proto::Seq(1));
            s.apply(
                Update::RunEvent {
                    run: run.clone(),
                    seq,
                    event,
                },
                cx,
            );
        });
    }
}

fn text(s: &str) -> ExecEvent {
    ExecEvent::Text { text: s.into() }
}

/// How often the rows were built whole, how many items `diff` was given and how many were
/// measured again, all so far.
fn work_so_far(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> (usize, usize, usize) {
    screen.read_with(vcx, |s, _| {
        (s.rebuilds(), crate::vm::items::compared(), s.remeasured())
    })
}

#[gpui::test]
fn a_streamed_delta_rebuilds_nothing_and_diffs_only_the_changed_tail(cx: &mut TestAppContext) {
    let (store, _root, vcx, screen) = open(cx);
    let run = run_id("acp8");
    let items = screen.read_with(vcx, |s, _| s.items().len());
    assert!(items > 20, "a long thread: {items} items");

    // The view rests in the middle of the thread, and nothing that streams in must move it.
    let anchor = screen.update(vcx, |s, cx| {
        s.list.scroll_to(ListOffset {
            item_ix: 5,
            offset_in_item: px(7.0),
        });
        cx.notify();
        s.list.logical_scroll_top()
    });
    draw(vcx);

    // The run goes on with a new turn, which takes a few events to open.
    stream(
        vcx,
        &store,
        &run,
        vec![
            ExecEvent::Resumed {
                from_attempt: 0,
                session: SessionRef("s".into()),
            },
            text("Look"),
        ],
    );
    draw(vcx);
    let opened = screen.read_with(vcx, |s, _| s.items().len());
    assert_eq!(opened, items + 2, "the resume line and the reply came in");
    screen.read_with(vcx, |s, _| {
        let reply = s.rows().last().unwrap().key();
        assert!(
            !s.markdown.contains_key(&reply),
            "a reply out of view is not parsed until its item is drawn"
        );
    });
    let (rebuilt, compared, measured) = work_so_far(vcx, &screen);

    stream(vcx, &store, &run, vec![text(" closer")]);
    draw(vcx);
    let (rebuilt_now, compared_now, measured_now) = work_so_far(vcx, &screen);
    assert_eq!(rebuilt_now, rebuilt, "a delta builds no rows again");
    assert!(
        compared_now - compared <= 3,
        "a delta compares the changed tail, not the list: {} items",
        compared_now - compared
    );
    assert!(
        measured_now - measured <= 2,
        "a delta measures its own item again: {}",
        measured_now - measured
    );
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.items().len(), opened, "no item came or went");
        assert!(
            matches!(s.rows().last(),
            Some(Row::Prose { text, .. }) if text == "Look closer"),
            "{:?}",
            s.rows().last()
        );
        assert_eq!(s.list.logical_scroll_top(), anchor, "the view did not move");
    });

    // A step is one new item at the end, and its result patches it in place.
    stream(
        vcx,
        &store,
        &run,
        vec![
            ExecEvent::Tool {
                id: "late".into(),
                call: ToolCall::new(
                    "bash",
                    serde_json::json!({ "command": "cargo test" }),
                    ActionClass::Command,
                ),
            },
            ExecEvent::ToolResult {
                id: "late".into(),
                ok: true,
                summary: Some("ok".into()),
                output: None,
            },
        ],
    );
    draw(vcx);
    let (rebuilt_end, compared_end, _) = work_so_far(vcx, &screen);
    assert_eq!(rebuilt_end, rebuilt, "a step builds no rows again");
    assert!(
        compared_end - compared_now <= 6,
        "{}",
        compared_end - compared_now
    );
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.items().len(), opened + 1);
        assert_eq!(s.item_count(), s.items().len());
        assert_eq!(s.list.logical_scroll_top(), anchor, "the view did not move");
    });

    // The turn finishes: its loose rows fold in one splice, and a view resting inside them
    // lands on the fold's head.
    // Where the view stands right after the screen has taken the events in (the next frame may
    // move it to keep the end of a short list at the bottom).
    let landed = std::rc::Rc::new(std::cell::Cell::new(None));
    let list = screen.read_with(vcx, |s, _| s.list.clone());
    let _watch = vcx.update(|_, cx| {
        let landed = landed.clone();
        cx.observe(&store, move |_, _| {
            landed.set(Some(list.logical_scroll_top()));
        })
    });
    let (loose_at, loose_key) = screen.update(vcx, |s, cx| {
        let key = s
            .rows()
            .iter()
            .rev()
            .find_map(|r| {
                matches!(r, Row::Prose { text, .. } if text == "Look closer").then(|| r.key())
            })
            .unwrap();
        let ix = s.items().item_of(key).unwrap();
        s.list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(3.0),
        });
        // The events reach the screen once this returns, before any frame is drawn.
        stream(
            cx,
            &store,
            &run,
            vec![
                text(" Done."),
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: None,
                    diagnostic: None,
                    reason: None,
                },
            ],
        );
        (ix, key)
    });
    let (rebuilt_done, ..) = work_so_far(vcx, &screen);
    assert_eq!(
        rebuilt_done, rebuilt,
        "a finished turn builds no rows again"
    );
    let (fold, _) = *folds(vcx, &screen).last().unwrap();
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.item_count(), s.items().len());
        assert!(s.items().item_of(loose_key).is_none(), "the reply folded");
        let head = s.items().item_of(fold).unwrap();
        assert!(head <= loose_at, "the fold stands where its first row was");
        assert_eq!(
            landed.get(),
            Some(ListOffset {
                item_ix: head,
                offset_in_item: px(0.0)
            }),
            "the view lands on the head"
        );
    });
}

#[gpui::test]
fn a_rows_height_is_held_until_its_render_settles(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    // The view rests at the top of the thread, where the first reply is in view.
    screen.update(vcx, |s, cx| {
        s.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });
        cx.notify();
    });
    draw(vcx);
    // The first reply in view and its height.
    let (ix, held, top) = screen.read_with(vcx, |s, _| {
        let top = s.list.logical_scroll_top();
        let ix = (top.item_ix..s.items().len())
            .find(|&ix| {
                matches!(s.items().row(s.rows(), ix), Some(Row::Prose { .. }))
                    && s.list.bounds_for_item(ix).is_some()
            })
            .expect("a reply in view");
        (ix, s.list.bounds_for_item(ix).unwrap().size.height, top)
    });
    let (_, _, measured) = work_so_far(vcx, &screen);

    // Nothing the store says outside the run changes it.
    for _ in 0..3 {
        screen.update(vcx, |_, cx| cx.notify());
        draw(vcx);
    }
    let (_, _, quiet) = work_so_far(vcx, &screen);
    assert_eq!(
        quiet, measured,
        "no renderer settled: no row is measured again"
    );
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.list.bounds_for_item(ix).unwrap().size.height, held);
        assert_eq!(s.list.logical_scroll_top(), top);
    });

    // A render lands: the rows in view that hold its output are measured again, once, and
    // the view stays where it was.
    vcx.update(|_, cx| crate::ui::markdown_view::settled(cx));
    draw(vcx);
    let (_, _, settled) = work_so_far(vcx, &screen);
    assert!(
        settled > quiet,
        "the settled render measures its rows again"
    );
    draw(vcx);
    assert_eq!(
        work_so_far(vcx, &screen).2,
        settled,
        "once per landing, not once per frame"
    );
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.list.bounds_for_item(ix).unwrap().size.height, held);
        assert_eq!(s.list.logical_scroll_top(), top, "the list did not jump");
    });
}

#[gpui::test]
fn find_goes_to_hits_opens_what_hides_them_and_closes_only_that(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    find_counts_seven_and_the_first_is_turn_threes_cargo_test_step(vcx, &screen);
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    enter_opens_the_fold_at_the_step_and_escape_closes_exactly_what_search_opened(vcx, &screen);
    a_fold_the_owner_opened_stays_open_and_a_row_the_owner_touches_becomes_theirs(vcx, &screen);
    the_count_follows_changes_once_per_150_ms_and_keeps_the_current_match(vcx, &screen);
}

/// The thread's runs arrive one after another: a query typed while they do is still "1 of 7"
/// once they are all in, because no hit was gone to (TRANSCRIPT-3.2, 3.3).
#[gpui::test]
fn a_query_typed_while_the_thread_loads_ends_on_one_of_seven(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let all = screen.read_with(vcx, |s, _| s.rows().to_vec());
    // Only the later runs are in when the query is typed...
    let later = all[all.len() / 2..].to_vec();
    screen.update(vcx, |s, cx| s.replace_rows(0, Some(later), cx));
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    vcx.simulate_keystrokes("cmd-a");
    type_query(vcx, "supervisor.rs");
    let partial = screen.read_with(vcx, |s, _| s.find_found().hits.len());
    assert!(
        (1..7).contains(&partial),
        "{partial} hits with half the thread"
    );
    // ...then the earlier ones arrive and the count follows.
    screen.update(vcx, |s, cx| {
        s.replace_rows(0, Some(all), cx);
        s.find_recount(cx);
    });
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
}

/// What the owner's own order does on the seed: every fold open, ⌘F, `supervisor.rs`, Enter, Esc.
#[gpui::test]
fn the_cards_own_order_with_every_fold_open(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    screen.update(vcx, |s, cx| s.open_all_folds(cx));
    draw(vcx);
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    vcx.simulate_keystrokes("cmd-a");
    type_query(vcx, "supervisor.rs");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    // The first hit: the fold it is in and the call whose output holds it.
    let (fold, call) = screen.read_with(vcx, |s, _| {
        let hit = s
            .find_found()
            .current_hit()
            .cloned()
            .expect("a current hit");
        let Part::Output(n) = hit.part else {
            panic!("the first hit is in an output: {:?}", hit.part)
        };
        let call = s
            .rows()
            .iter()
            .flat_map(|r| match r {
                Row::Worked { rows, .. } => rows.iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .find_map(|r| match r {
                Row::Tools { key, calls } if *key == hit.key => Some(calls[n].id.clone()),
                _ => None,
            })
            .expect("the call");
        (hit.fold.expect("inside a fold"), call)
    });
    // (the hit's fold open, its output open, every fold open, search holds something open)
    let state = |vcx: &VisualTestContext| {
        screen.read_with(vcx, |s, _| {
            let all = folds(vcx, &screen).iter().all(|(k, _)| s.is_open(*k));
            (
                s.is_open(fold),
                s.output_is_open(&call),
                all,
                s.search_holds(),
            )
        })
    };
    let typed = state(vcx);
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    let entered = state(vcx);
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    let escaped = state(vcx);
    // Typing opens nothing: turn 3's fold is the owner's, the step's output is closed.
    assert_eq!(typed, (true, false, true, false), "after typing");
    // Enter opens the `cargo test` step's output (search holds it); the fold was already the
    // owner's, so search opens nothing else.
    assert_eq!(entered, (true, true, true, true), "after Enter");
    // Esc closes the output search opened and nothing more: every fold the owner opened stays
    // open, turn 3's included.
    assert_eq!(escaped, (true, false, true, false), "after Esc");
}

/// Every hit row is a tick on the prompt tick rail while the strip holds a query, the current hit
/// marked; they go when the strip closes, and the prompt ticks stay.
#[gpui::test]
fn match_ticks_show_on_the_rail_while_find_holds_a_query(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let match_ticks = |vcx: &mut VisualTestContext| {
        draw(vcx);
        painted(vcx, "find-tick") || painted(vcx, "find-tick-current")
    };
    wait_for(vcx, "the prompt ticks", |vcx| painted(vcx, "tick-rail"));
    assert!(painted(vcx, "tick-current"), "the prompt ticks are there");
    assert!(!match_ticks(vcx), "no query, no match ticks");
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    assert!(!match_ticks(vcx), "an open strip with no query has none");
    vcx.simulate_keystrokes("cmd-a");
    type_query(vcx, "supervisor.rs");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    assert!(match_ticks(vcx), "a query puts ticks on the rail");
    assert!(
        painted(vcx, "find-tick-current"),
        "the current hit is marked"
    );
    assert!(painted(vcx, "tick-current"), "beside the prompt ticks");
    // One for each row that holds a hit (a hit inside a fold is its fold's row).
    let (ticks, rows) = screen.read_with(vcx, |s, _| {
        let mut holders: Vec<_> = s
            .find_found()
            .hits
            .iter()
            .map(|h| h.fold.unwrap_or(h.key))
            .collect();
        holders.dedup();
        (
            s.find_rail_ticks(&crate::ui::theme::Theme::for_appearance(Appearance::Dark))
                .len(),
            holders.len(),
        )
    });
    assert_eq!(ticks, rows);
    assert!(ticks > 1);
    vcx.simulate_keystrokes("escape");
    assert!(
        !match_ticks(vcx),
        "the match ticks go when the strip closes"
    );
    assert!(painted(vcx, "tick-rail"), "the prompt ticks stay");
}

/// The rows the whole path gives for `run`'s own events, folded as the screen folds them.
fn whole_rows(
    vcx: &VisualTestContext,
    store: &Entity<Store>,
    run: &RunId,
    pending: &[reno_proto::Approval],
    segment: usize,
) -> Vec<Row> {
    store.read_with(vcx, |s, _| {
        let state = s.state();
        let events = state
            .transcript(run)
            .map_or(&[][..], |t| t.events.as_slice());
        let summary = state.run(run).unwrap();
        let built = crate::transcript::build_rows(None, events, pending);
        let elapsed = crate::vm::convo::run_elapsed_ms(
            summary.status.is_terminal(),
            summary.created_at_ms,
            summary.updated_at_ms,
            events,
        );
        crate::transcript::fold_turns(built.rows, elapsed)
            .into_iter()
            .map(|row| row.in_segment(segment))
            .collect()
    })
}

#[gpui::test]
fn an_approval_that_waits_and_resolves_takes_the_screen_off_the_incremental_path_and_back(
    cx: &mut TestAppContext,
) {
    use reno_proto::{Approval, ApprovalId, ApprovalSubject};
    let (store, _root, vcx, screen) = open(cx);
    let run = run_id("acp8");
    let segment = screen.read_with(vcx, |s, _| s.runs.len() - 1);
    let waiting = Approval {
        id: ApprovalId::from_suffix("held").unwrap(),
        run_id: run.clone(),
        subject: ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some("git push".into()),
            paths: vec![],
            raw: serde_json::Value::Null,
        },
        requested_at_ms: 1,
    };
    // The screen's rows end with the run's own: the lead before them is the divider and prompt.
    let ends_with = |vcx: &VisualTestContext, pending: &[Approval], what: &str| {
        let want = whole_rows(vcx, &store, &run, pending, segment);
        screen.read_with(vcx, |s, _| {
            assert!(s.rows().ends_with(&want), "{what}: {:?}", s.rows());
        });
    };

    // The finished run goes on with a new attempt, whose reply streams in.
    stream(
        vcx,
        &store,
        &run,
        vec![
            ExecEvent::Resumed {
                from_attempt: 0,
                session: SessionRef("s".into()),
            },
            text("One"),
        ],
    );
    draw(vcx);
    ends_with(vcx, &[], "incremental");
    let (rebuilt, ..) = work_so_far(vcx, &screen);
    stream(vcx, &store, &run, vec![text(" two")]);
    draw(vcx);
    assert_eq!(work_so_far(vcx, &screen).0, rebuilt, "incremental");

    // The approval is held: the rows are built whole, with it, and stay so while it waits.
    store.update(vcx, |s, cx| {
        s.apply(Update::Approvals(vec![waiting.clone()]), cx);
    });
    draw(vcx);
    let held = work_so_far(vcx, &screen).0;
    assert!(held > rebuilt, "the approval builds the rows whole");
    ends_with(vcx, std::slice::from_ref(&waiting), "held");
    stream(vcx, &store, &run, vec![text(" three")]);
    draw(vcx);
    assert!(work_so_far(vcx, &screen).0 > held, "whole while it waits");
    ends_with(vcx, std::slice::from_ref(&waiting), "held, with more");

    // It resolves: one whole build, and the next delta goes in without any.
    store.update(vcx, |s, cx| s.apply(Update::Approvals(vec![]), cx));
    draw(vcx);
    ends_with(vcx, &[], "resolved");
    let resolved = work_so_far(vcx, &screen).0;
    stream(vcx, &store, &run, vec![text(" four")]);
    draw(vcx);
    assert_eq!(
        work_so_far(vcx, &screen).0,
        resolved,
        "back on the incremental path"
    );
    ends_with(vcx, &[], "incremental again");
    screen.read_with(vcx, |s, _| {
        assert!(matches!(s.rows().last(),
            Some(Row::Prose { text, .. }) if text == "One two three four"));
    });
}

fn reply(s: &mut RunScreen, key: Key, text: &str, live: bool) {
    RunScreen::parse_reply(&mut s.markdown, &mut s.live_reply, key, text, live);
}

#[gpui::test]
fn the_parsed_trees_stop_at_their_bound_and_keep_the_live_reply(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let first = Key(1 << 50);
    screen.update(vcx, |s, _| {
        reply(s, first, "the reply still arriving", true);
        for n in 1..=700 {
            reply(s, Key((1 << 50) + n), "an older reply", false);
        }
    });
    screen.read_with(vcx, |s, _| {
        assert!(s.markdown.len() <= 512, "{} parsed trees", s.markdown.len());
        assert!(
            s.markdown.peek(&first).is_some(),
            "the live reply is the oldest and stays"
        );
        assert!(s.markdown.peek(&Key((1 << 50) + 700)).is_some());
    });

    screen.update(vcx, |s, _| {
        reply(s, Key((1 << 50) + 701), "the next reply", true);
        for n in 702..=1300 {
            reply(s, Key((1 << 50) + n), "an older reply", false);
        }
    });
    screen.read_with(vcx, |s, _| {
        assert!(s.markdown.len() <= 512, "{} parsed trees", s.markdown.len());
        assert!(
            s.markdown.peek(&first).is_none(),
            "a reply that stopped being the live one can go"
        );
        assert!(s.markdown.peek(&Key((1 << 50) + 701)).is_some());
    });
}

#[gpui::test]
fn the_directory_probes_stop_at_their_bound(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    screen.update(vcx, |s, cx| {
        for n in 0..300_u64 {
            let text = format!("see `probe{n}/lib.rs`");
            let tree = reno_markdown::parse_full(&text);
            s.probe_files(Key((1 << 50) + n), text.len(), &tree, cx);
        }
    });
    screen.read_with(vcx, |s, _| {
        assert!(s.probed.len() <= 256, "{} replies probed", s.probed.len());
        assert!(
            s.probed_dirs() <= 256,
            "{} directories held",
            s.probed_dirs()
        );
    });
    // The answers arrive and are kept within the same bound.
    for _ in 0..50 {
        draw(vcx);
        std::thread::sleep(Duration::from_millis(10));
    }
    screen.read_with(vcx, |s, _| assert!(s.probed_dirs() <= 256));
}

// --- match case, whole word, regex -------------------------------------------------------------

fn hits(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> usize {
    screen.read_with(vcx, |s, _| s.find_found().hits.len())
}

/// Replaces the query in the open bar (⌘F puts the keyboard back in the field after a click).
fn requery(vcx: &mut VisualTestContext, query: &str) {
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    vcx.simulate_keystrokes("cmd-a");
    vcx.simulate_input(query);
    draw(vcx);
}

#[gpui::test]
fn toggles_and_their_keys_recount_and_an_invalid_regex_shows_no_count(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    requery(vcx, "SUPERVISOR.RS");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    assert!(painted(vcx, "find-case") && painted(vcx, "find-word") && painted(vcx, "find-regex"));

    // Match case, by its key: the capitals match nothing, and the key undoes it.
    vcx.simulate_keystrokes("cmd-alt-c");
    draw(vcx);
    assert_eq!(count(vcx, &screen).as_deref(), Some("No results"));
    assert!(screen.read_with(vcx, |s, _| s.find_query().case));
    vcx.simulate_keystrokes("cmd-alt-c");
    draw(vcx);
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    // And by its button.
    click(vcx, "find-case");
    draw(vcx);
    assert_eq!(count(vcx, &screen).as_deref(), Some("No results"));
    click(vcx, "find-case");
    draw(vcx);
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));

    // Whole word: `visor` is inside `supervisor`, so fewer places count.
    requery(vcx, "visor");
    let inside = hits(vcx, &screen);
    assert!(inside > 0, "`visor` is in the thread");
    vcx.simulate_keystrokes("cmd-alt-w");
    draw(vcx);
    let whole = hits(vcx, &screen);
    assert!(whole < inside, "{whole} whole words of {inside}");
    assert!(screen.read_with(vcx, |s, _| s.find_query().word));
    click(vcx, "find-word");
    draw(vcx);
    assert_eq!(hits(vcx, &screen), inside);

    // Regex: `.` is any character, and a pattern that does not parse says why, with no count.
    requery(vcx, "supervisor.rs");
    let plain = hits(vcx, &screen);
    vcx.simulate_keystrokes("cmd-alt-x");
    draw(vcx);
    assert!(screen.read_with(vcx, |s, _| s.find_query().regex));
    assert!(hits(vcx, &screen) >= plain);
    requery(vcx, "(");
    screen.read_with(vcx, |s, _| {
        assert!(s.find_error().is_some_and(|why| why.contains("unclosed")));
        assert_eq!(s.find_count_label(), None, "no count");
        assert!(s.find_found().hits.is_empty());
    });
    assert!(painted(vcx, "find-error"));
    click(vcx, "find-regex");
    draw(vcx);
    screen.read_with(vcx, |s, _| {
        assert_eq!(s.find_error(), None, "a plain `(` is not a pattern");
        assert!(s.find_count_label().is_some(), "counted again");
        assert!(
            !s.find_found().hits.is_empty(),
            "the thread has parentheses"
        );
    });
    assert!(!painted(vcx, "find-error"));
}

#[gpui::test]
fn a_recount_while_streaming_rescans_only_the_live_run(cx: &mut TestAppContext) {
    let (store, _root, vcx, screen) = open(cx);
    let run = run_id("acp8");
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    requery(vcx, "supervisor.rs");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    let scanned = |vcx: &VisualTestContext| screen.read_with(vcx, |s, _| s.find_runs_scanned());
    let before = scanned(vcx);

    stream(
        vcx,
        &store,
        &run,
        vec![
            ExecEvent::Resumed {
                from_attempt: 0,
                session: SessionRef("s".into()),
            },
            text("now supervisor.rs"),
        ],
    );
    draw(vcx);
    vcx.executor().advance_clock(Duration::from_millis(151));
    vcx.run_until_parked();
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 8"));
    assert_eq!(
        scanned(vcx) - before,
        1,
        "the seven older runs were not read"
    );

    stream(vcx, &store, &run, vec![text(" and supervisor.rs again")]);
    draw(vcx);
    vcx.executor().advance_clock(Duration::from_millis(151));
    vcx.run_until_parked();
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 9"));
    assert_eq!(scanned(vcx) - before, 2, "one more read of the live run");
}

#[gpui::test]
fn the_hit_cache_stops_at_its_bound(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    // Every key typed is another query, and each is kept for each older run.
    requery(vcx, "supervisor.rs handshake negotiation");
    assert_eq!(
        screen.read_with(vcx, |s, _| s.find_hit_lists()),
        64,
        "the least recently used lists went"
    );
    // What was dropped is found again, correctly.
    requery(vcx, "supervisor.rs");
    assert_eq!(count(vcx, &screen).as_deref(), Some("1 of 7"));
    assert_eq!(screen.read_with(vcx, |s, _| s.find_hit_lists()), 64);
}

#[gpui::test]
fn closing_search_leaves_streaming_reasoning_open_and_a_stream_ending_keeps_search_opened_rows(
    cx: &mut TestAppContext,
) {
    bind(cx);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let run = run_id("builder1");
    root.update_in(vcx, |r, window, cx| r.open_run(run.clone(), window, cx));
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    wait_for(vcx, "painted(vcx, \"thinking\")", |vcx| {
        painted(vcx, "thinking")
    });
    let reply = |s: &str| ExecEvent::Text { text: s.into() };
    stream(
        vcx,
        &store,
        &run,
        vec![ExecEvent::Reasoning {
            text: "weighing the mirror".into(),
        }],
    );
    draw(vcx);
    let key = screen.read_with(vcx, |s, _| s.rows().last().unwrap().key());
    let open = |vcx: &VisualTestContext| screen.read_with(vcx, |s, cx| s.reasoning_open(key, cx));
    assert!(open(vcx), "open while it streams");

    // Search goes to a match inside it, and closing search leaves it open: the stream holds it.
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    requery(vcx, "mirror");
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert!(open(vcx));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!screen.read_with(vcx, |s, _| s.find_is_open()));
    assert!(open(vcx), "closing search does not close what streams");
    assert!(!screen.read_with(vcx, |s, _| s.search_holds()));

    // With the bar open, a row search opened stays open when the stream ends.
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    stream(vcx, &store, &run, vec![reply("Going with the mirror.")]);
    draw(vcx);
    assert!(open(vcx), "search's row did not close when the stream did");
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!open(vcx), "and closes with search once the stream is over");
}
