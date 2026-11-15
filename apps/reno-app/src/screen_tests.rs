//! The Inbox, Runs, Triggers and Settings screens against the fake daemon: loading, empty,
//! failed and live states, and the actions on each.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{ConnectionStatus, Feed, FeedState, RunPage};
use reno_proto::defs::{DefError, DefsSnapshot};
use reno_proto::{ErrorCode, ErrorObject, RunStatus};

use crate::app::RootView;
use crate::nav::Nav;
use crate::screens::settings::Tab;
use crate::store::Store;
use crate::tests::{click, draw, fake_store, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::runs::RunFilter;
use crate::vm::{Section, inbox, settings, triggers};

fn store_with(cx: &mut TestAppContext, config: FakeConfig) -> Entity<Store> {
    cx.update(gpui_tokio::init);
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    })
}

fn daemon(cx: &TestAppContext, store: &Entity<Store>) -> std::sync::Arc<FakeDaemon> {
    store.read_with(cx, |s, _| s.fake().cloned().expect("a fake daemon"))
}

fn go(vcx: &mut VisualTestContext, root: &Entity<RootView>, nav: Nav) {
    root.update_in(vcx, |r, window, cx| r.navigate(nav, window, cx));
    draw(vcx);
}

fn connected(cx: &mut TestAppContext, store: &Entity<Store>) {
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.connection() == ConnectionStatus::Connected
                && s.state().feed(Feed::Runs) == FeedState::Live
                && s.state().feed(Feed::Triggers) == FeedState::Live
                && s.state().feed(Feed::Defs) == FeedState::Live
        })
    });
}

#[gpui::test]
fn inbox_loads_before_the_daemon_answers(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    let view = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
    assert_eq!(view.held, Section::Loading);
    assert!(view.banner.is_some());
}

#[gpui::test]
fn inbox_shows_held_actions_and_approving_moves_them_to_history(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "approve-appr_keeperpush"));
    let view = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
    assert_eq!(view.lede, "2 things need you");
    assert_eq!(
        view.held.rows()[0].approval.id.to_string(),
        "appr_keeperpush",
        "the longest-waiting action is first"
    );
    // The history was read when the screen opened.
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        matches!(v.history, Section::Rows(_))
    });

    click(vcx, "approve-appr_keeperpush");
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        v.held.rows().len() == 1
            && v.history
                .rows()
                .first()
                .is_some_and(|h| h.verdict == inbox::Verdict::Approved && h.by == "you")
    });
}

#[gpui::test]
fn inbox_is_empty_and_then_live(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            empty: true,
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        v.held == Section::Empty && v.history == Section::Empty
    });
    // A run that needs a human appears without any refresh.
    store.update(vcx, |s, cx| s.run_task("push it".into(), "claude-code", cx));
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        v.held.rows().len() == 1 && v.lede == "1 thing needs you"
    });
}

#[gpui::test]
fn inbox_says_when_history_fails(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    daemon(cx, &store).fail(
        "approval.history",
        ErrorObject::new(ErrorCode::Unavailable, "busy"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        matches!(v.history, Section::Failed(_))
    });
}

#[gpui::test]
fn runs_filter_page_and_open(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Runs);
    wait_drawn(vcx, |vcx| painted(vcx, "run-row-run_keeper1"));
    let view = root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx));
    assert_eq!(view.rows.rows().len(), 6);

    // The first page for All was read when the screen opened; the daemon has no more.
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.state().run_page(&[]) == RunPage::Done)
    });

    click(vcx, "filter-Failed");
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx));
        v.filter == RunFilter::Failed && v.rows.rows().len() == 1
    });
    assert!(!painted(vcx, "run-row-run_keeper1"));
    click(vcx, "filter-Held");
    wait_drawn(vcx, |vcx| painted(vcx, "run-row-run_keeper1"));

    click(vcx, "run-row-run_keeper1");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
}

#[gpui::test]
fn runs_empty_and_live(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            empty: true,
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Runs);
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx)).rows == Section::Empty
    });
    store.update(vcx, |s, cx| s.run_task("hello".into(), "claude-code", cx));
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx))
            .rows
            .rows()
            .len()
            == 1
    });
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx));
        v.rows.rows()[0].status == RunStatus::Succeeded
    });
}

#[gpui::test]
fn triggers_list_fire_and_live_updates(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Triggers);
    wait_drawn(vcx, |vcx| painted(vcx, "fire-hotfix-check"));
    // Schedules come from the definitions, read after the snapshot.
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.triggers().read(cx).view(cx));
        v.rows.rows().iter().all(|r| !r.schedule.is_empty())
    });
    let view = root.read_with(vcx, |r, cx| r.triggers().read(cx).view(cx));
    assert_eq!(view.rows.rows().len(), 4);
    assert_eq!(view.backing_off, 1);
    // Last runs outside the window are read with `run.get`.
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.triggers().read(cx).view(cx));
        v.rows
            .rows()
            .iter()
            .filter_map(|r| r.last_run.as_ref())
            .all(|l| l.status.is_some())
    });

    click(vcx, "fire-hotfix-check");
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.triggers().read(cx).view(cx));
        v.rows
            .rows()
            .iter()
            .find(|r| r.name == "hotfix-check")
            .and_then(|r| r.last_run.as_ref())
            .is_some_and(|l| l.run.to_string().starts_with("run_fake"))
    });

    // A change made elsewhere shows up on its own.
    let d = daemon(vcx, &store);
    let mut list = store.read_with(vcx, |s, _| s.state().triggers().to_vec());
    list.retain(|t| t.name != "pr-review");
    d.set_triggers(list);
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.triggers().read(cx).view(cx))
            .rows
            .rows()
            .len()
            == 3
    });
}

#[gpui::test]
fn a_failing_fire_keeps_the_screen_usable(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    daemon(cx, &store).fail(
        "trigger.fire",
        ErrorObject::new(ErrorCode::Unavailable, "scheduler busy"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Triggers);
    wait_drawn(vcx, |vcx| painted(vcx, "fire-hotfix-check"));
    click(vcx, "fire-hotfix-check");
    wait_drawn(vcx, |vcx| {
        !store.read_with(vcx, |s, _| s.is_firing("hotfix-check"))
    });
    // The button works again, and nothing was fired.
    assert!(painted(vcx, "fire-hotfix-check"));
}

#[gpui::test]
fn triggers_empty_and_unavailable(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            empty: true,
            without_features: vec![reno_proto::FEATURE_DEFS.to_owned()],
            ..FakeConfig::default()
        },
    );
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().feed(Feed::Triggers) == FeedState::Live)
    });
    let view = store.read_with(cx, |s, _| triggers::build(s.state(), s.now_ms()));
    assert_eq!(view.rows, Section::Empty);
    let defs = store.read_with(cx, |s, _| settings::defs(s.state()));
    assert_eq!(defs.bots, Section::Unavailable);
}

#[gpui::test]
fn settings_definitions_problems_validation_and_daemon(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Settings);
    wait_drawn(vcx, |vcx| painted(vcx, "validate-bot-keeper"));
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.settings().read(cx).defs_view(cx));
        v.bots.rows().iter().all(|r| !r.detail.is_empty())
    });
    let view = root.read_with(vcx, |r, cx| r.settings().read(cx).defs_view(cx));
    assert_eq!(view.bots.rows().len(), 5);
    assert_eq!(view.problem_count, 0);

    // Validate re-checks a stored definition.
    click(vcx, "validate-bot-keeper");
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.settings().read(cx).defs_view(cx));
        v.bots.rows()[0].name == "architect"
            || v.bots
                .rows()
                .iter()
                .find(|r| r.name == "keeper")
                .is_some_and(|r| r.validation.as_ref().is_some_and(|x| x.valid))
    });

    // A broken file arrives through defs.watch.
    let d = daemon(vcx, &store);
    let defs = store.read_with(vcx, |s, _| s.state().defs().defs.clone());
    d.set_defs(DefsSnapshot {
        defs,
        errors: vec![
            DefError::new("bots/keeper.toml", "executors[0]", "unknown executor"),
            DefError::new("bots/new.toml", "", "not valid TOML"),
        ],
    });
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.settings().read(cx).defs_view(cx))
            .problem_count
            == 2
    });
    let view = root.read_with(vcx, |r, cx| r.settings().read(cx).defs_view(cx));
    let keeper = view
        .bots
        .rows()
        .iter()
        .find(|r| r.name == "keeper")
        .unwrap();
    assert!(keeper.has_problems());
    assert_eq!(view.orphan_problems.len(), 1);

    // The daemon tab.
    click(vcx, "tab-Daemon");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.settings().read(cx).daemon_view(cx))
            .uptime
            .is_some()
    });
    let daemon_view = root.read_with(vcx, |r, cx| r.settings().read(cx).daemon_view(cx));
    assert_eq!(daemon_view.version.as_deref(), Some("0.0.0-fake"));
    assert_eq!(daemon_view.active, Some(3));
    assert_eq!(
        root.read_with(vcx, |r, cx| r.settings().read(cx).tab()),
        Tab::Daemon
    );
    click(vcx, "tab-Policy");
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| settings::policy(s.state()).is_some())
    });
    let policy = store.read_with(vcx, |s, _| settings::policy(s.state()).unwrap());
    assert_eq!(policy.rules.len(), 3);
}

#[gpui::test]
fn the_app_reconnects_after_the_daemon_drops(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Runs);
    let before = store.read_with(vcx, |s, _| s.state().runs().len());

    daemon(vcx, &store).sever();
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.connection() != ConnectionStatus::Connected)
    });
    let view = root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx));
    assert!(view.banner.is_some(), "the screen says it is offline");
    assert_eq!(view.rows.rows().len(), before, "stale rows stay on screen");

    // It reconnects by itself, with the backoff, and the banner goes away.
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.connection() == ConnectionStatus::Connected)
    });
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.runs().read(cx).view(cx))
            .banner
            .is_none()
    });
}
