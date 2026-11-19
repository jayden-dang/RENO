//! The Run, Project, Runs, Triggers and Settings screens against the fake daemon: loading, empty,
//! failed and live states, and the actions on each.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::rc::Rc;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{ConnectionStatus, Feed, FeedState};
use reno_proto::defs::{DefError, DefsSnapshot};
use reno_proto::{ErrorCode, ErrorObject};

use crate::app::RootView;
use crate::nav::Nav;
use crate::store::Store;
use crate::tests::{click, draw, fake_store, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::executors::Group;
use crate::vm::settings_nav::Category;
use crate::vm::{Section, settings, triggers};

fn store_with(cx: &mut TestAppContext, mut config: FakeConfig) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    config.step = std::time::Duration::ZERO;
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
    if nav == Nav::Executors {
        // The roster's two-line rows run past the default window; tall enough to click them all.
        vcx.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(2400.0)));
    }
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
fn all_runs_has_a_lane_per_live_run_and_a_lane_opens_its_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Runs);
    wait_drawn(vcx, |vcx| painted(vcx, "lane-run_keeper1"));
    let lanes = root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx));
    let live = store.read_with(vcx, |s, _| crate::vm::shell::live_runs(s.state()).len());
    assert_eq!(lanes.len(), live, "one lane per live run");
    assert!(lanes.len() >= 3, "the demo has a running run and two held");
    // A held run's lane has its stripe and says what it waits for.
    let held = lanes
        .iter()
        .find(|l| l.run.to_string() == "run_keeper1")
        .unwrap();
    assert_eq!(held.status_label, "held for you");
    assert!(held.asks.is_some());
    assert!(painted(vcx, "lane-stripe-run_keeper1"));
    // Finished runs are not lanes.
    assert!(lanes.iter().all(|l| !l.status.is_terminal()));

    click(vcx, "lane-run_keeper1");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
}

#[gpui::test]
fn all_runs_says_when_nothing_is_live(cx: &mut TestAppContext) {
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
    wait_drawn(vcx, |vcx| painted(vcx, "no-live-runs"));
    assert!(
        root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx))
            .is_empty()
    );
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

fn category(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Category {
    root.read_with(vcx, |r, cx| r.settings().read(cx).category())
}

#[gpui::test]
fn the_settings_row_opens_settings_in_the_sidebars_place_and_back_returns(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Runs);
    assert!(painted(vcx, "sidebar"));
    // Executors and Settings are no longer top rows; Settings is the footer row.
    assert!(!painted(vcx, "nav-Executors"));
    click(vcx, "nav-Settings");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
    assert!(painted(vcx, "settings-nav") && !painted(vcx, "sidebar"));
    for key in [
        "executors",
        "registry",
        "definitions",
        "policy",
        "notifications",
        "daemon",
    ] {
        assert!(painted(vcx, &format!("settings-{key}")), "{key}");
    }
    // Executors is the first page, with its own controls.
    assert_eq!(category(vcx, &root), Category::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "check-all"));

    // A category changes the highlighted row and the page.
    click(vcx, "settings-daemon");
    assert_eq!(category(vcx, &root), Category::Daemon);
    wait_drawn(vcx, |vcx| painted(vcx, "daemon-facts"));
    assert!(!painted(vcx, "check-all"));

    // Back returns to the screen Settings opened over.
    click(vcx, "settings-back");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    draw(vcx);
    assert!(painted(vcx, "sidebar") && !painted(vcx, "settings-nav"));
}

#[gpui::test]
fn back_from_settings_returns_to_the_open_run_or_home(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    // Opened from Home, Back goes Home.
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-back");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    // Opened over a run, Back shows that run again; a category change in between does not
    // forget it.
    root.update_in(vcx, |r, window, cx| {
        r.open_run(crate::tests::run_id("keeper1"), window, cx);
    });
    draw(vcx);
    let kept = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("the run");
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-policy");
    go(vcx, &root, Nav::Executors);
    click(vcx, "settings-back");
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("the run");
    assert_eq!(
        screen.read_with(vcx, |s, _| s.run_id().clone()),
        crate::tests::run_id("keeper1")
    );
    // The very screen that was showing, so a draft in its box and its panel are as they were.
    assert_eq!(screen.entity_id(), kept.entity_id());
}

#[gpui::test]
fn executors_is_a_settings_page_and_the_footer_counts_what_needs_you(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let needing = executor_view(vcx, &root).needing;
    assert!(needing > 0, "the fake has executors that need you");
    assert!(painted(vcx, "settings-count"));
    go(vcx, &root, Nav::Executors);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
    assert_eq!(category(vcx, &root), Category::Executors);
    assert!(painted(vcx, "settings-executors-count"));
    wait_drawn(vcx, |vcx| painted(vcx, "line-codex"));
    // The roster has no registry group; that is its own page.
    assert!(!painted(vcx, "group-registry"));
}

#[gpui::test]
fn searching_settings_filters_the_categories(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-search");
    vcx.simulate_input("dock");
    draw(vcx);
    assert!(painted(vcx, "settings-notifications"));
    assert!(!painted(vcx, "settings-executors") && !painted(vcx, "settings-daemon"));
    assert_eq!(category(vcx, &root), Category::Notifications);
    wait_drawn(vcx, |vcx| painted(vcx, "notify-dockBadge"));
    vcx.simulate_input("zzz");
    draw(vcx);
    assert!(painted(vcx, "settings-nothing"));
    assert_eq!(
        category(vcx, &root),
        Category::Notifications,
        "the page stays"
    );
}

#[gpui::test]
fn settings_opened_again_from_elsewhere_starts_with_an_empty_search(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-search");
    vcx.simulate_input("dock");
    draw(vcx);
    assert!(!painted(vcx, "settings-executors"));
    click(vcx, "settings-back");
    // A deep link (a run's "Open Executors") lands on a navigation that lists everything.
    go(vcx, &root, Nav::Executors);
    assert_eq!(category(vcx, &root), Category::Executors);
    assert!(painted(vcx, "settings-executors") && painted(vcx, "settings-daemon"));
}

#[gpui::test]
fn while_the_daemon_connects_the_gear_is_marked_and_settings_says_why(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Light);
    draw(vcx);
    assert!(painted(vcx, "settings-pip"));
    assert!(
        !painted(vcx, "settings-count"),
        "nothing is known to need you"
    );
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-definitions");
    assert!(painted(vcx, "daemon-banner"));
}

#[gpui::test]
fn with_no_executor_that_needs_you_there_is_no_count(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    assert!(painted(vcx, "settings-count"));
    assert!(
        !painted(vcx, "settings-pip"),
        "connected: no mark on the gear"
    );
    let healthy: Vec<_> = store.read_with(vcx, |s, _| {
        s.state()
            .executors()
            .iter()
            .filter(|e| e.descriptor.id.as_str() == "claude-code")
            .cloned()
            .collect()
    });
    daemon(vcx, &store).set_executors(healthy);
    wait_drawn(vcx, |vcx| !painted(vcx, "settings-count"));
    go(vcx, &root, Nav::Settings);
    assert!(!painted(vcx, "settings-executors-count"));
}

#[gpui::test]
fn policy_opened_before_the_daemon_answers_reads_the_standing_rules_once_it_does(
    cx: &mut TestAppContext,
) {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    // Reno opened on Settings › Policy while the daemon is still starting: no features yet.
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |r, window, cx| {
        r.open_settings(Category::Policy, window, cx);
    });
    draw(vcx);
    assert!(store.read_with(vcx, |s, _| s.rules().is_none()));
    // The daemon answers; the list the page asked for is read then, not left loading.
    store.update(vcx, |s, cx| {
        s.start_fake_with(crate::tests::instant_script(), cx)
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.rules().is_some_and(|r| r.is_ok()))
    });
}

#[gpui::test]
fn settings_definitions_problems_validation_and_daemon(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-definitions");
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

    // The Daemon page.
    click(vcx, "settings-daemon");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.settings().read(cx).daemon_view(cx))
            .uptime
            .is_some()
    });
    let daemon_view = root.read_with(vcx, |r, cx| r.settings().read(cx).daemon_view(cx));
    assert_eq!(daemon_view.version.as_deref(), Some("0.0.0-fake"));
    assert_eq!(daemon_view.active, Some(3));
    assert_eq!(category(vcx, &root), Category::Daemon);
    click(vcx, "settings-policy");
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
    wait_drawn(vcx, |vcx| painted(vcx, "lane-run_keeper1"));
    let before = root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx).len());

    daemon(vcx, &store).sever();
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.connection() != ConnectionStatus::Connected)
    });
    wait_drawn(vcx, |vcx| painted(vcx, "daemon-banner"));
    assert_eq!(
        root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx).len()),
        before,
        "stale lanes stay on screen"
    );

    // It reconnects by itself, with the backoff, and the banner goes away.
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.connection() == ConnectionStatus::Connected)
    });
    wait_drawn(vcx, |vcx| !painted(vcx, "daemon-banner"));
}

// --- Executors -------------------------------------------------------------------------------

fn executors_live(cx: &mut TestAppContext, store: &Entity<Store>) {
    connected(cx, store);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().feed(Feed::Executors) == FeedState::Live
        })
    });
}

fn executor_view(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
) -> crate::vm::executors::ExecutorsView {
    root.read_with(vcx, |r, cx| r.executors().read(cx).view(cx))
}

#[gpui::test]
fn executors_load_before_the_daemon_answers(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    let view = executor_view(vcx, &root);
    assert_eq!(view.rows, Section::Loading);
    assert!(view.banner.is_some());
}

#[gpui::test]
fn executors_group_by_what_needs_you_and_check_again_probes(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "line-codex"));
    let view = executor_view(vcx, &root);
    let ids = |g: Group| -> Vec<String> { view.group(g).iter().map(|r| r.id.clone()).collect() };
    assert_eq!(ids(Group::NeedsYou), ["codex", "cursor", "acp.grok"]);
    assert_eq!(
        ids(Group::Ready),
        ["acp.antigravity", "claude-code", "muse", "opencode", "pi"]
    );
    assert_eq!(
        ids(Group::NotInstalled),
        ["acp.devin", "acp.fx", "acp.hermes"]
    );
    assert_eq!(ids(Group::Test), ["mock"]);
    assert_eq!(view.lede, "3 executors can't take runs");
    assert_eq!(
        view.detail,
        "Codex is signed out \u{b7} Cursor is signed out \u{b7} Grok is too old"
    );
    // Cursor's fix is an environment variable: the line says which, and offers no sign-in
    // command (Reno never starts the SDK's browser login), and being a managed install it has
    // no Auto-update switch.
    let cursor = row_of(vcx, &root, "cursor");
    let fix = cursor
        .middle
        .as_ref()
        .expect("a sign-in fact")
        .detail
        .clone();
    assert!(fix.unwrap_or_default().contains("CURSOR_API_KEY"));
    assert!(cursor.sign_in.is_none() && cursor.managed && cursor.switch.is_none());
    // The mock is collapsed until its group is opened.
    assert!(!painted(vcx, "line-mock"));
    click(vcx, "group-Test executors");
    wait_drawn(vcx, |vcx| painted(vcx, "line-mock"));

    // What fixes each line is on the line: Sign in… for Codex, Update for the too-old Grok,
    // Install… for a first install; the Auto-update switch where there is an updater.
    assert!(painted(vcx, "sign-in-codex"));
    assert!(painted(vcx, "update-acp.grok"));
    assert!(painted(vcx, "install-acp.devin"));
    assert!(painted(vcx, "auto-claude-code") && painted(vcx, "auto-codex"));
    assert!(!painted(vcx, "auto-acp.antigravity"), "managed: no switch");
    assert!(
        !painted(vcx, "auto-pi"),
        "no updater is declared for Pi: no switch"
    );
    assert!(
        !painted(vcx, "auto-muse"),
        "no updater is declared for Muse: no switch"
    );
    assert!(
        !painted(vcx, "auto-cursor") && !painted(vcx, "sign-in-cursor"),
        "a managed install with no sign-in command: no switch, no button"
    );
    assert!(!painted(vcx, "update-codex"), "auto-update is on");

    // "Check again" is in the opened line and probes that executor and nothing else.
    click(vcx, "line-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "check-codex"));
    let stamp = |vcx: &mut VisualTestContext, id: &str| {
        store.read_with(vcx, |s, _| {
            s.state()
                .executors()
                .iter()
                .find(|e| e.descriptor.id.as_str() == id)
                .and_then(|e| e.health.as_ref())
                .map(|h| h.checked_at_ms)
        })
    };
    let (codex, mock) = (stamp(vcx, "codex"), stamp(vcx, "mock"));
    click(vcx, "check-codex");
    wait_drawn(vcx, |vcx| stamp(vcx, "codex") > codex);
    assert_eq!(stamp(vcx, "mock"), mock);
    wait_drawn(vcx, |vcx| {
        !store.read_with(vcx, |s, _| s.is_checking(Some(&"codex".parse().unwrap())))
    });
    click(vcx, "check-all");
    wait_drawn(vcx, |vcx| stamp(vcx, "mock") > mock);
    wait_drawn(vcx, |vcx| !store.read_with(vcx, |s, _| s.is_checking(None)));
}

fn slow_jobs(cx: &mut TestAppContext, millis: u64) -> Entity<Store> {
    store_with(
        cx,
        FakeConfig {
            job_duration: std::time::Duration::from_millis(millis),
            ..FakeConfig::default()
        },
    )
}

fn row_of(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
    id: &str,
) -> crate::vm::executors::ExecutorRow {
    executor_view(vcx, root)
        .rows
        .rows()
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .unwrap()
}

#[gpui::test]
fn a_first_install_is_reviewed_then_runs_then_the_executor_moves_to_ready(cx: &mut TestAppContext) {
    let store = slow_jobs(cx, 600);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "install-acp.devin"));

    // [Install…] shows what will run and runs nothing.
    click(vcx, "install-acp.devin");
    wait_drawn(vcx, |vcx| painted(vcx, "run-acp.devin"));
    assert!(row_of(vcx, &root, "acp.devin").job.is_none());
    assert!(painted(vcx, "review-cancel-acp.devin"));
    click(vcx, "review-cancel-acp.devin");
    wait_drawn(vcx, |vcx| !painted(vcx, "run-acp.devin"));
    assert!(row_of(vcx, &root, "acp.devin").job.is_none());

    click(vcx, "install-acp.devin");
    wait_drawn(vcx, |vcx| painted(vcx, "run-acp.devin"));
    click(vcx, "run-acp.devin");
    // Running: the dot is hollow, the line offers Show log and Cancel, never Install.
    wait_drawn(vcx, |vcx| {
        row_of(vcx, &root, "acp.devin")
            .job
            .is_some_and(|j| j.running())
    });
    assert_eq!(
        row_of(vcx, &root, "acp.devin").dot,
        crate::vm::executors::Dot::Running
    );
    wait_drawn(vcx, |vcx| {
        painted(vcx, "cancel-acp.devin") && painted(vcx, "log-acp.devin")
    });
    assert!(!painted(vcx, "install-acp.devin"));
    assert_eq!(executor_view(vcx, &root).jobs_running, 1);

    // Show log reads executor.job.log and opens the block.
    click(vcx, "log-acp.devin");
    wait_drawn(vcx, |vcx| {
        painted(vcx, "job-log-acp.devin")
            && store.read_with(vcx, |s, _| {
                s.job_log(&"acp.devin".parse().unwrap())
                    .is_some_and(|l| !l.lines.is_empty())
            })
    });

    // Done: Reno probed again, so the executor is installed and listed as Ready; the toast says so.
    wait_drawn(vcx, |vcx| row_of(vcx, &root, "acp.devin").job.is_none());
    let devin = row_of(vcx, &root, "acp.devin");
    assert_eq!(devin.group, Group::Ready);
    assert_eq!(devin.version_text.as_deref(), Some("1.0.0"));
    wait_drawn(vcx, |vcx| {
        vcx.update(|_, cx| crate::ui::toast::shown(cx))
            .iter()
            .any(|(_, m)| m == "Devin installed.")
    });
}

#[gpui::test]
fn a_queued_update_waits_for_runs_until_update_now_anyway(cx: &mut TestAppContext) {
    let store = slow_jobs(cx, 300);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "auto-codex"));
    assert!(row_of(vcx, &root, "codex").available.as_deref() == Some("0.171.0"));
    assert!(!painted(vcx, "update-codex"));

    // Turning auto-update off is the switch: Update appears.
    click(vcx, "auto-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "update-codex"));
    assert!(!row_of(vcx, &root, "codex").switch.unwrap().on);

    // The executor has a run in flight: the update queues.
    click(vcx, "update-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "force-codex"));
    let job = row_of(vcx, &root, "codex").job.unwrap();
    assert_eq!(
        job.words("Codex"),
        "Update queued \u{2014} waits for 1 running task on Codex"
    );
    assert!(painted(vcx, "cancel-codex"));

    // Update now anyway starts it at once and it finishes on the new version.
    click(vcx, "force-codex");
    wait_drawn(vcx, |vcx| {
        row_of(vcx, &root, "codex").version_text.as_deref() == Some("0.171.0")
    });
    assert!(row_of(vcx, &root, "codex").job.is_none());
    wait_drawn(vcx, |vcx| {
        vcx.update(|_, cx| crate::ui::toast::shown(cx))
            .iter()
            .any(|(_, m)| m == "Codex updated to 0.171.0.")
    });
}

#[gpui::test]
fn cancelling_a_queued_update_leaves_it_stopped_and_dismissable(cx: &mut TestAppContext) {
    let store = slow_jobs(cx, 300);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "auto-codex"));
    click(vcx, "auto-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "update-codex"));
    click(vcx, "update-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "cancel-codex"));
    click(vcx, "cancel-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "retry-codex"));
    let job = row_of(vcx, &root, "codex").job.unwrap();
    assert_eq!(job.words("Codex"), "Stopped");
    assert!(
        !painted(vcx, "terminal-codex"),
        "a stopped job is not an error to rerun"
    );
    click(vcx, "dismiss-codex");
    wait_drawn(vcx, |vcx| row_of(vcx, &root, "codex").job.is_none());
}

#[gpui::test]
fn a_failed_install_says_why_and_retry_runs_it_again(cx: &mut TestAppContext) {
    let store = slow_jobs(cx, 60);
    executors_live(cx, &store);
    daemon(cx, &store).fail_next_job(
        reno_proto::JobFailure::NeedsPassword,
        "brew asked for your password (sudo). Reno never types a password for you.",
    );
    let recording = Rc::new(crate::terminal::fake::Recording::default());
    cx.update(|cx| crate::terminal::install(cx, recording.clone()));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "install-acp.devin"));
    click(vcx, "install-acp.devin");
    wait_drawn(vcx, |vcx| painted(vcx, "run-acp.devin"));
    click(vcx, "run-acp.devin");
    wait_drawn(vcx, |vcx| painted(vcx, "retry-acp.devin"));
    let job = row_of(vcx, &root, "acp.devin").job.unwrap();
    assert_eq!(job.words("Devin"), "Install failed");
    assert!(job.failed());
    assert_eq!(row_of(vcx, &root, "acp.devin").group, Group::NotInstalled);
    assert!(painted(vcx, "log-acp.devin") && painted(vcx, "terminal-acp.devin"));

    // Run in Terminal hands the Reno-run steps to the opener, which is the fake: nothing real.
    click(vcx, "terminal-acp.devin");
    assert_eq!(
        recording.opened.borrow().as_slice(),
        [(
            "Run in Terminal: Devin".to_owned(),
            "brew install --cask devin-cli".to_owned()
        )]
    );
    wait_drawn(vcx, |vcx| painted(vcx, "check-now-acp.devin"));
    click(vcx, "wait-cancel-acp.devin");
    wait_drawn(vcx, |vcx| painted(vcx, "retry-acp.devin"));

    // Retry starts the install again (the next one succeeds).
    click(vcx, "retry-acp.devin");
    wait_drawn(vcx, |vcx| {
        row_of(vcx, &root, "acp.devin").group == Group::Ready
    });
    assert!(row_of(vcx, &root, "acp.devin").job.is_none());
}

#[gpui::test]
fn the_switch_stores_auto_update_and_the_verified_only_box_follows_the_updater(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "auto-claude-code"));
    assert!(row_of(vcx, &root, "claude-code").switch.unwrap().on);
    click(vcx, "auto-claude-code");
    wait_drawn(vcx, |vcx| {
        !row_of(vcx, &root, "claude-code").switch.unwrap().on
    });
    let auto = |vcx: &mut VisualTestContext| {
        store.read_with(vcx, |s, _| {
            s.state()
                .executors()
                .iter()
                .find(|e| e.descriptor.id.as_str() == "claude-code")
                .and_then(|e| e.maintenance.as_ref())
                .map(|m| m.auto_update)
        })
    };
    assert_eq!(auto(vcx).map(|a| a.on), Some(false), "the daemon stored it");

    // Expanded, Claude Code (which can pin) offers "Only versions Reno has verified"; Codex does not.
    click(vcx, "line-claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "verified-claude-code"));
    click(vcx, "verified-claude-code");
    wait_drawn(vcx, |vcx| auto(vcx).is_some_and(|a| a.verified_only));
    assert_eq!(
        auto(vcx).map(|a| a.on),
        Some(false),
        "the box does not turn the switch"
    );
    click(vcx, "line-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "check-codex"));
    assert!(!painted(vcx, "verified-codex"));
}

#[gpui::test]
fn sign_in_opens_terminal_through_the_injected_opener_and_checking_back_probes(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let recording = Rc::new(crate::terminal::fake::Recording::default());
    cx.update(|cx| crate::terminal::install(cx, recording.clone()));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "sign-in-codex"));
    click(vcx, "sign-in-codex");
    assert_eq!(
        recording.opened.borrow().as_slice(),
        [("Sign in to Codex".to_owned(), "codex login".to_owned())]
    );
    // The line waits and offers Check now.
    wait_drawn(vcx, |vcx| painted(vcx, "check-now-codex"));
    let stamp = |vcx: &mut VisualTestContext| {
        store.read_with(vcx, |s, _| {
            s.state()
                .executors()
                .iter()
                .find(|e| e.descriptor.id.as_str() == "codex")
                .and_then(|e| e.health.as_ref())
                .map(|h| h.checked_at_ms)
        })
    };
    let before = stamp(vcx);
    click(vcx, "check-now-codex");
    wait_drawn(vcx, |vcx| stamp(vcx) > before);
    // The fake is still signed out there: the line is back to Sign in…, and the toast says so.
    wait_drawn(vcx, |vcx| painted(vcx, "sign-in-codex"));
    wait_drawn(vcx, |vcx| {
        vcx.update(|_, cx| crate::ui::toast::shown(cx))
            .iter()
            .any(|(_, m)| m == "Codex is still signed out.")
    });

    // Coming back to the window checks too.
    click(vcx, "sign-in-codex");
    wait_drawn(vcx, |vcx| painted(vcx, "check-now-codex"));
    let before = stamp(vcx);
    root.update(vcx, |r, cx| {
        r.executors()
            .update(cx, |screen, cx| screen.on_window_activated(cx));
    });
    wait_drawn(vcx, |vcx| stamp(vcx) > before);
    assert_eq!(recording.opened.borrow().len(), 2);
}

#[gpui::test]
#[should_panic(expected = "a test reached the real Terminal opener")]
fn a_test_that_forgets_to_inject_a_terminal_opener_fails(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "sign-in-codex"));
    click(vcx, "sign-in-codex");
}

#[gpui::test]
fn an_older_daemon_keeps_check_again_and_the_managed_install_and_hides_the_rest(
    cx: &mut TestAppContext,
) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_EXECUTOR_MAINTAIN.to_owned()],
            ..FakeConfig::default()
        },
    );
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "check-codex"));
    assert!(!executor_view(vcx, &root).maintain);
    assert!(!painted(vcx, "auto-codex"), "no switch");
    assert!(!painted(vcx, "update-acp.grok"), "no Update");
    assert!(!painted(vcx, "sign-in-codex"));
    assert!(
        painted(vcx, "install-acp.hermes"),
        "the daemon's own install stays"
    );
    assert!(!painted(vcx, "run-acp.hermes"));

    // Install is `executor.install`, at once, with no review.
    click(vcx, "install-acp.hermes");
    wait_drawn(vcx, |vcx| !painted(vcx, "install-acp.hermes"));
    assert_eq!(row_of(vcx, &root, "acp.hermes").installed.text, "Installed");
}

#[gpui::test]
fn a_failed_legacy_install_keeps_the_screen_and_its_button_usable(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_EXECUTOR_MAINTAIN.to_owned()],
            ..FakeConfig::default()
        },
    );
    executors_live(cx, &store);
    daemon(cx, &store).fail(
        "executor.install",
        ErrorObject::new(ErrorCode::Unavailable, "registry unreachable"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "install-acp.hermes"));
    click(vcx, "install-acp.hermes");
    wait_drawn(vcx, |vcx| {
        !store.read_with(vcx, |s, _| s.is_installing(&"acp.hermes".parse().unwrap()))
    });
    assert_eq!(
        row_of(vcx, &root, "acp.hermes").installed.text,
        "Needs install"
    );
    assert!(painted(vcx, "install-acp.hermes"));
}

#[gpui::test]
fn executors_unavailable_and_empty(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_EXECUTORS.to_owned()],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    let view = store.read_with(cx, |s, _| {
        crate::vm::executors::build(s.state(), s.now_ms())
    });
    assert_eq!(view.rows, Section::Unavailable);

    let store = fake_store(cx);
    executors_live(cx, &store);
    daemon(cx, &store).set_executors(vec![]);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().executors().is_empty())
    });
    let view = store.read_with(cx, |s, _| {
        crate::vm::executors::build(s.state(), s.now_ms())
    });
    assert_eq!(view.rows, Section::Empty);
}

#[gpui::test]
fn a_run_refused_for_its_executor_shows_the_executors_health(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    store.update(vcx, |s, cx| s.run_task("/refused".into(), "codex", cx));
    crate::tests::show_session(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "run-open-executors"));
    // The signed-out reason is on the run's own screen, with the way to Settings › Executors.
    assert!(painted(vcx, "run-check-executor"));
    click(vcx, "run-open-executors");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
    assert_eq!(category(vcx, &root), Category::Executors);
}

// --- Deny with a reason ----------------------------------------------------------------------

fn history_of(
    store: &Entity<Store>,
    cx: &TestAppContext,
    id: &str,
) -> Option<reno_proto::ApprovalRecord> {
    store.read_with(cx, |s, _| {
        s.state()
            .approval_history()
            .iter()
            .find(|r| r.id.as_str() == id)
            .cloned()
    })
}

// --- Answering inside the run (ui-brief 2026-11-17) ------------------------------------------

fn open_run_screen(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
    run: &str,
) -> Entity<crate::screens::run::RunScreen> {
    click(vcx, &format!("recent-run_{run}"));
    draw(vcx);
    root.read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen")
}

#[gpui::test]
fn the_approve_menu_keeps_the_answer_for_the_bot(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open_run_screen(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-more"));
    assert!(!painted(vcx, "approve-scope-1"), "the menu starts closed");
    click(vcx, "approve-more");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-scope-1"));
    click(vcx, "approve-scope-1");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_rules(cx));
        store.read_with(vcx, |s, _| {
            s.rules()
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|rules| {
                    rules.iter().any(|rule| {
                        rule.scope
                            == reno_proto::StandingScope::Bot {
                                bot: "keeper".to_owned(),
                            }
                    })
                })
        })
    });

    // Settings, Policy: the kept rule is listed with a Revoke button, and revoking removes it.
    let rule = store.read_with(vcx, |s, _| {
        s.rules()
            .and_then(|r| r.as_ref().ok())
            .and_then(|rules| rules.first())
            .cloned()
            .unwrap()
    });
    go(vcx, &root, Nav::Settings);
    click(vcx, "settings-policy");
    let revoke = format!("revoke-{}", rule.id);
    wait_drawn(vcx, |vcx| painted(vcx, &revoke));
    click(vcx, &revoke);
    wait_drawn(vcx, |vcx| !painted(vcx, &revoke));
}

// --- The Project page (ui-brief 2026-11-17) ---------------------------------------------------

fn project_of(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
) -> Option<crate::vm::projects::FolderKey> {
    root.read_with(vcx, |r, cx| {
        r.project_screen().map(|p| p.read(cx).key().clone())
    })
}

#[gpui::test]
fn a_projects_history_holds_only_its_own_decisions(cx: &mut TestAppContext) {
    use crate::vm::projects::FolderKey;
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // keeper's push is in atlas-api; approve it so atlas-api has a decision.
    open_run_screen(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve"));
    click(vcx, "approve");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);

    // The run's header, and its crumb, are gone: the project opens by its folder.
    let atlas = FolderKey::Repo(std::path::PathBuf::from("/demo/atlas-api"));
    root.update_in(vcx, |r, window, cx| {
        r.open_project(atlas.clone(), window, cx);
    });
    assert_eq!(project_of(vcx, &root), Some(atlas));
    let project = root
        .read_with(vcx, |r, _| r.project_screen().cloned())
        .unwrap();
    project.update_in(vcx, |p, _, cx| {
        p.set_tab(crate::screens::project::Tab::History, cx);
    });
    wait_drawn(vcx, |vcx| {
        let view = project.read_with(vcx, |p, cx| p.view(cx));
        view.history
            .rows()
            .iter()
            .any(|h| h.line.contains("git push"))
    });

    // web-console's project page does not show atlas-api's decision.
    click(vcx, "project-web-console");
    let web = FolderKey::Repo(std::path::PathBuf::from("/demo/web-console"));
    assert_eq!(project_of(vcx, &root), Some(web));
    let project = root
        .read_with(vcx, |r, _| r.project_screen().cloned())
        .unwrap();
    let view = project.read_with(vcx, |p, cx| p.view(cx));
    assert!(
        !view
            .history
            .rows()
            .iter()
            .any(|h| h.line.contains("reno/keeper")),
        "{:?}",
        view.history.rows()
    );
}

#[gpui::test]
fn always_in_the_project_keeps_the_answer_there_and_the_allowlist_lists_and_removes_it(
    cx: &mut TestAppContext,
) {
    use crate::vm::projects::FolderKey;
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open_run_screen(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-more"));
    click(vcx, "approve-more");
    // Once, for keeper, in atlas-api, every project: the project choice is the third.
    wait_drawn(vcx, |vcx| painted(vcx, "approve-scope-2"));
    click(vcx, "approve-scope-2");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);

    let atlas = std::path::PathBuf::from("/demo/atlas-api");
    root.update_in(vcx, |r, window, cx| {
        r.open_project(FolderKey::Repo(atlas.clone()), window, cx);
    });
    let project = root
        .read_with(vcx, |r, _| r.project_screen().cloned())
        .unwrap();
    wait_drawn(vcx, |vcx| {
        project.read_with(vcx, |p, cx| p.view(cx)).here.len() == 1
    });
    let rule = project.read_with(vcx, |p, cx| p.view(cx)).here[0].clone();
    assert!(rule.matches.contains("git push"), "{rule:?}");

    // Remove takes it off the list.
    let remove = format!("remove-{}", rule.id);
    wait_drawn(vcx, |vcx| painted(vcx, &remove));
    click(vcx, &remove);
    wait_drawn(vcx, |vcx| {
        project.read_with(vcx, |p, cx| p.view(cx)).here.is_empty()
    });
}

#[gpui::test]
fn a_command_added_on_the_allowlist_is_kept_for_that_project(cx: &mut TestAppContext) {
    use crate::vm::projects::FolderKey;
    cx.update(crate::ui::composer::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_project(
            FolderKey::Repo(std::path::PathBuf::from("/demo/reno")),
            window,
            cx,
        );
    });
    let project = root
        .read_with(vcx, |r, _| r.project_screen().cloned())
        .unwrap();
    let field = project
        .read_with(vcx, |p, _| p.add_field().cloned())
        .expect("an add field for a repository");
    field.update_in(vcx, |c, window, cx| {
        c.fill("cargo clippy --workspace", window, cx);
        c.submit(window, cx);
    });
    wait_drawn(vcx, |vcx| {
        project
            .read_with(vcx, |p, cx| p.view(cx))
            .here
            .iter()
            .any(|r| r.matches.contains("cargo clippy --workspace"))
    });
}

#[gpui::test]
fn an_older_daemon_offers_only_what_it_keeps(cx: &mut TestAppContext) {
    use crate::vm::projects::FolderKey;
    // No project rules: the menu has no "Always in", the Allowlist says to update renod.
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_PROJECT_RULES.to_owned()],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open_run_screen(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-more"));
    click(vcx, "approve-more");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-scope-2"));
    assert!(
        !painted(vcx, "approve-scope-3"),
        "once, keeper, every project: no project choice"
    );
    root.update_in(vcx, |r, window, cx| {
        r.open_project(
            FolderKey::Repo(std::path::PathBuf::from("/demo/atlas-api")),
            window,
            cx,
        );
    });
    let project = root
        .read_with(vcx, |r, _| r.project_screen().cloned())
        .unwrap();
    assert!(!project.read_with(vcx, |p, cx| p.view(cx)).project_rules);
    draw(vcx);
    assert!(painted(vcx, "project-rules-here"));
}

#[gpui::test]
fn without_rules_the_panel_is_allow_once_and_deny(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![
                reno_proto::FEATURE_RULES.to_owned(),
                reno_proto::FEATURE_APPROVAL_MESSAGE.to_owned(),
            ],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open_run_screen(vcx, &root, "scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "deny"));
    assert!(
        !painted(vcx, "approve-more"),
        "no menu without standing rules"
    );
    click(vcx, "deny");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
}

// --- The ACP registry group ------------------------------------------------------------------

fn registry_config(job_millis: u64) -> FakeConfig {
    FakeConfig {
        registry_agents: true,
        job_duration: std::time::Duration::from_millis(job_millis),
        ..FakeConfig::default()
    }
}

/// Settings › Add from registry on a window tall enough to click every line.
fn registry_page(
    cx: &mut TestAppContext,
    config: FakeConfig,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let store = store_with(cx, config);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(3200.0)));
    registry_tab(vcx, &root);
    (store, root, vcx)
}

fn registry_tab(vcx: &mut VisualTestContext, root: &Entity<RootView>) {
    root.update_in(vcx, |r, window, cx| {
        r.open_settings(Category::Registry, window, cx);
    });
    draw(vcx);
}

/// Settings › Executors, where an added registry agent's line is.
fn roster_tab(vcx: &mut VisualTestContext, root: &Entity<RootView>) {
    go(vcx, root, Nav::Executors);
}

fn registry_of(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
) -> Option<crate::vm::registry::RegistryView> {
    root.read_with(vcx, |r, cx| r.executors().read(cx).registry_view(cx))
}

fn line_names(view: &crate::vm::registry::RegistryView) -> Vec<String> {
    view.lines.iter().map(|l| l.name.clone()).collect()
}

fn wait_registry(vcx: &mut VisualTestContext) {
    wait_drawn(vcx, |vcx| painted(vcx, "registry-agent-goose"));
}

/// What the daemon holds as an added agent's environment, read through the client.
fn env_of(vcx: &mut VisualTestContext, store: &Entity<Store>, id: &'static str) -> String {
    let task = store.read_with(vcx, |s, cx| {
        s.registry_task(
            move |client| async move { client.registry_env_get(id).await },
            cx,
        )
    });
    let slot = Rc::new(std::cell::RefCell::new(None));
    let out = slot.clone();
    vcx.spawn(async move |_| {
        *out.borrow_mut() = Some(task.await);
    })
    .detach();
    wait_drawn(vcx, |_| slot.borrow().is_some());
    let result = slot.borrow_mut().take().unwrap();
    result.unwrap()
}

fn toasts(vcx: &mut VisualTestContext) -> Vec<String> {
    vcx.update(|_, cx| crate::ui::toast::shown(cx))
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

#[gpui::test]
fn an_old_daemon_has_no_registry_group_and_makes_no_call(cx: &mut TestAppContext) {
    let (store, root, vcx) = registry_page(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_EXECUTOR_REGISTRY.to_owned()],
            ..registry_config(50)
        },
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    draw(vcx);
    assert!(registry_of(vcx, &root).is_none());
    assert!(!painted(vcx, "group-registry"));
    store.read_with(vcx, |s, _| {
        assert!(s.registry().listing.is_none() && !s.registry().fetching);
        assert!(s.registry().failure.is_none(), "no call, so no failure");
    });
    assert!(toasts(vcx).is_empty(), "no toast either");
}

#[gpui::test]
fn the_registry_group_lists_what_can_be_added_after_the_roster(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(50));
    wait_registry(vcx);
    let view = registry_of(vcx, &root).unwrap();
    assert_eq!(
        line_names(&view),
        [
            "Claude Agent",
            "Codex CLI",
            "Devin",
            "fast-agent",
            "Gemini CLI",
            "goose",
            "VT Code",
            "Windows Only"
        ]
    );
    // Built-in agents and the added ones are not counted: 5 can be looked at, 3 cannot run.
    assert_eq!(view.count, 5);
    assert_eq!(
        view.finding,
        "5 agents in the registry \u{b7} 1 need a sign-in or a key \u{b7} 4 can't run on this Mac"
    );
    assert_eq!(view.checked.as_deref(), Some("registry checked just now"));
    for id in ["claude-acp", "codex-acp", "devin"] {
        assert!(painted(vcx, &format!("registry-agent-{id}")));
        assert!(
            !painted(vcx, &format!("registry-line-{id}")),
            "{id} is built in: it does not open"
        );
    }
    assert!(painted(vcx, "registry-line-goose") && painted(vcx, "registry-add-goose"));
    assert!(painted(vcx, "registry-search") && painted(vcx, "registry-refresh"));
    assert!(painted(vcx, "registry-own-line"));
    // The added agents are ordinary lines of the roster, with where they came from.
    // The added agents are on the roster, Settings › Executors.
    roster_tab(vcx, &root);
    let cline = row_of(vcx, &root, "acp.cline");
    assert_eq!(cline.group, Group::Ready);
    assert_eq!(cline.registry.unwrap().id, "cline");
    assert!(
        cline.update,
        "the index lists 1.1.0 over the installed 1.0.0"
    );
    assert!(painted(vcx, "update-acp.cline") && painted(vcx, "remove-acp.cline"));
    assert_eq!(
        row_of(vcx, &root, "acp.old-agent").registry.unwrap().note,
        crate::vm::executors::RegistryNote::Delisted
    );
    assert!(!painted(vcx, "update-acp.old-agent") && painted(vcx, "remove-acp.old-agent"));
}

#[gpui::test]
fn search_and_the_filter_narrow_the_list_and_nothing_matches_says_so(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(50));
    wait_registry(vcx);
    // Typed through the field, as a person does.
    click(vcx, "registry-search");
    vcx.simulate_input("gem");
    draw(vcx);
    assert_eq!(
        line_names(&registry_of(vcx, &root).unwrap()),
        ["Gemini CLI"]
    );
    assert!(!painted(vcx, "registry-agent-goose"));
    vcx.simulate_input("zzz");
    draw(vcx);
    assert!(painted(vcx, "registry-nothing"));
    assert!(registry_of(vcx, &root).unwrap().nothing_matches);
    // Clear the search; the filter keeps what cannot run here.
    let search = root.read_with(vcx, |r, cx| r.executors().read(cx).search_field().clone());
    search.update(vcx, |f, cx| f.set_text("", cx));
    draw(vcx);
    click(vcx, "registry-filter-cant-run");
    let view = registry_of(vcx, &root).unwrap();
    assert_eq!(
        line_names(&view),
        ["fast-agent", "Gemini CLI", "VT Code", "Windows Only"]
    );
    assert!(
        view.segments
            .iter()
            .any(|s| s.filter == crate::vm::registry::Filter::CantRun && s.pressed)
    );
    assert!(
        !painted(vcx, "registry-agent-claude-acp"),
        "built-in agents stay under All only"
    );
}

#[gpui::test]
fn an_opened_line_shows_the_facts_and_the_three_steps_and_cannot_run_shows_why(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = registry_page(cx, registry_config(50));
    wait_registry(vcx);
    click(vcx, "registry-line-goose");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-step-goose-3"));
    assert!(painted(vcx, "registry-step-goose-1") && painted(vcx, "registry-step-goose-2"));
    // While open, Add is step 1 inside the line and not on the line itself.
    assert!(painted(vcx, "registry-step-add-goose") && !painted(vcx, "registry-add-goose"));
    let line = registry_of(vcx, &root)
        .unwrap()
        .lines
        .into_iter()
        .find(|l| l.id == "goose")
        .unwrap();
    assert!(line.steps[0].enabled && !line.steps[1].enabled && !line.steps[2].enabled);
    // Closing it brings Add back.
    click(vcx, "registry-line-goose");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-add-goose"));
    assert!(!painted(vcx, "registry-step-goose-1"));

    // A line that cannot run has no steps: its reason, in danger, and no Add.
    click(vcx, "registry-line-fast-agent");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-cant-run-fast-agent"));
    assert!(!painted(vcx, "registry-step-fast-agent-1"));
    assert!(!painted(vcx, "registry-add-fast-agent"));
    let line = registry_of(vcx, &root)
        .unwrap()
        .lines
        .into_iter()
        .find(|l| l.id == "fast-agent")
        .unwrap();
    assert_eq!(
        line.cant_run.as_deref(),
        Some("needs uv: brew install uv. Add stays off until this Mac has it.")
    );
}

#[gpui::test]
fn adding_shows_progress_on_the_line_then_the_agent_joins_the_roster(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(250));
    wait_registry(vcx);
    click(vcx, "registry-add-goose");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-cancel-goose"));
    let line = registry_of(vcx, &root)
        .unwrap()
        .lines
        .into_iter()
        .find(|l| l.id == "goose")
        .unwrap();
    assert_eq!(line.progress_words().as_deref(), Some("Adding"));
    assert!(
        !painted(vcx, "registry-add-goose"),
        "Add is spent while it runs"
    );
    // It is on its registry line only, not also under Not installed.
    assert!(
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .all(|r| r.id != "acp.goose")
    );
    // Done: the agent leaves the group, joins Ready, and the toast says it can take runs.
    wait_drawn(vcx, |vcx| {
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .any(|r| r.id == "acp.goose" && r.group == Group::Ready)
    });
    wait_drawn(vcx, |vcx| {
        toasts(vcx)
            .iter()
            .any(|m| m == "goose added. It can take runs.")
    });
    wait_drawn(vcx, |vcx| !painted(vcx, "registry-agent-goose"));
    assert_eq!(registry_of(vcx, &root).unwrap().count, 4);
}

#[gpui::test]
fn a_failed_add_says_why_with_the_log_and_retry_adds_it(cx: &mut TestAppContext) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    daemon(vcx, &store).fail_next_job(
        reno_proto::JobFailure::ExitCode,
        "the download failed its sha256 check",
    );
    click(vcx, "registry-add-goose");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-retry-goose"));
    let line = registry_of(vcx, &root)
        .unwrap()
        .lines
        .into_iter()
        .find(|l| l.id == "goose")
        .unwrap();
    assert_eq!(line.progress_words().as_deref(), Some("Add failed"));
    assert!(painted(vcx, "registry-log-goose"));
    click(vcx, "registry-log-goose");
    wait_drawn(vcx, |vcx| painted(vcx, "job-log-acp.goose"));
    click(vcx, "registry-retry-goose");
    wait_drawn(vcx, |vcx| {
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .any(|r| r.id == "acp.goose" && r.group == Group::Ready)
    });
}

#[gpui::test]
fn update_adds_the_new_version_and_says_so(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    roster_tab(vcx, &root);
    assert_eq!(
        row_of(vcx, &root, "acp.cline").available.as_deref(),
        Some("1.1.0")
    );
    click(vcx, "update-acp.cline");
    wait_drawn(vcx, |vcx| {
        row_of(vcx, &root, "acp.cline").version_text.as_deref() == Some("1.1.0")
    });
    wait_drawn(vcx, |vcx| {
        toasts(vcx).iter().any(|m| m == "Cline updated to 1.1.0.")
    });
    assert!(!painted(vcx, "update-acp.cline"));
}

#[gpui::test]
fn remove_is_confirmed_on_the_line_then_the_agent_is_listed_again(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    assert!(!painted(vcx, "registry-agent-cline"));
    roster_tab(vcx, &root);
    click(vcx, "remove-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "remove-dialog-acp.cline"));
    assert!(painted(vcx, "remove-confirm-acp.cline") && painted(vcx, "remove-cancel-acp.cline"));
    // Cancel puts the line back.
    click(vcx, "remove-cancel-acp.cline");
    wait_drawn(vcx, |vcx| !painted(vcx, "remove-dialog-acp.cline"));
    assert!(painted(vcx, "remove-acp.cline"));
    // Confirm deletes it: it is a registry line again.
    click(vcx, "remove-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "remove-confirm-acp.cline"));
    click(vcx, "remove-confirm-acp.cline");
    wait_drawn(vcx, |vcx| !painted(vcx, "line-acp.cline"));
    // Listed again on the registry page.
    registry_tab(vcx, &root);
    wait_drawn(vcx, |vcx| painted(vcx, "registry-agent-cline"));
    assert!(
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .all(|r| r.id != "acp.cline")
    );
}

#[gpui::test]
fn a_removal_the_daemon_refuses_is_said_on_the_line(cx: &mut TestAppContext) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    roster_tab(vcx, &root);
    daemon(vcx, &store).set_active_runs(&"acp.cline".parse().unwrap(), 1);
    click(vcx, "remove-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "remove-confirm-acp.cline"));
    click(vcx, "remove-confirm-acp.cline");
    wait_drawn(vcx, |vcx| !painted(vcx, "remove-confirm-acp.cline"));
    assert!(painted(vcx, "remove-dialog-acp.cline"));
    assert!(
        store.read_with(vcx, |s, _| s.registry_refusal("cline").is_some()),
        "the refusal is on the line, in the daemon's words"
    );
    assert!(toasts(vcx).is_empty(), "not a toast");
    assert_eq!(
        row_of(vcx, &root, "acp.cline").group,
        Group::Ready,
        "it is still there"
    );
    click(vcx, "remove-cancel-acp.cline");
    wait_drawn(vcx, |vcx| !painted(vcx, "remove-dialog-acp.cline"));
}

#[gpui::test]
fn the_keys_editor_reads_and_saves_an_agents_environment_and_never_shows_it_elsewhere(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    roster_tab(vcx, &root);
    click(vcx, "line-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "env-card-acp.cline"));
    // Its environment is read once; typing before that would be overwritten by the answer.
    wait_drawn(vcx, |vcx| !painted(vcx, "env-status-acp.cline"));
    let field = root.read_with(vcx, |r, cx| {
        r.executors().read(cx).env_field("cline").cloned()
    });
    let field = field.expect("the editor opened with the line");
    // The value is typed and saved; it reaches the daemon and nowhere else.
    field.update(vcx, |f, cx| f.set_text("CLINE_API_KEY=sk-secret-value", cx));
    draw(vcx);
    click(vcx, "env-save-acp.cline");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.executors().read(cx).env_saved("cline"))
    });
    assert_eq!(
        env_of(vcx, &store, "cline"),
        "CLINE_API_KEY=sk-secret-value"
    );
    assert!(toasts(vcx).iter().all(|m| !m.contains("sk-secret")));

    // A bad line is refused by the daemon; the failure is in fixed words, not the value.
    field.update(vcx, |f, cx| f.set_text("not a pair sk-secret-value", cx));
    draw(vcx);
    click(vcx, "env-save-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "env-status-acp.cline"));
    let status = root.read_with(vcx, |r, cx| r.executors().read(cx).env_failure("cline"));
    let status = status.expect("it failed");
    assert!(!status.contains("sk-secret") && status.contains("KEY=VALUE"));
    assert!(toasts(vcx).iter().all(|m| !m.contains("sk-secret")));
}

#[gpui::test]
fn your_own_agent_has_a_form_and_joins_the_roster(cx: &mut TestAppContext) {
    let (_store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    click(vcx, "registry-own-line");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-own-add"));
    assert!(painted(vcx, "registry-own-name") && painted(vcx, "registry-own-command"));
    assert!(painted(vcx, "registry-own-env"));
    // Not ready: nothing is sent.
    click(vcx, "registry-own-add");
    assert!(
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .all(|r| r.id != "acp.own.zed-fork")
    );
    let (name, command) = root.read_with(vcx, |r, cx| r.executors().read(cx).own_fields());
    name.update(vcx, |f, cx| f.set_text("Zed Fork", cx));
    command.update(vcx, |f, cx| f.set_text("zed-fork --acp", cx));
    draw(vcx);
    click(vcx, "registry-own-add");
    wait_drawn(vcx, |vcx| {
        executor_view(vcx, &root)
            .rows
            .rows()
            .iter()
            .any(|r| r.id == "acp.own.zed-fork")
    });
    let own = row_of(vcx, &root, "acp.own.zed-fork");
    assert_eq!(
        own.registry.unwrap().note,
        crate::vm::executors::RegistryNote::Own
    );
    wait_drawn(vcx, |vcx| !painted(vcx, "registry-own-add"));
}

#[gpui::test]
fn an_added_agent_that_needs_a_sign_in_opens_terminal_with_its_own_command_through_the_opener(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    let recording = Rc::new(crate::terminal::fake::Recording::default());
    vcx.update(|_, cx| crate::terminal::install(cx, recording.clone()));
    wait_registry(vcx);
    roster_tab(vcx, &root);
    // The agent offers a terminal method (its `authMethods`); the daemon's generic maintenance
    // step says another command, which must not be the one that runs.
    let mut list = store.read_with(vcx, |s, _| s.state().executors().to_vec());
    let old = list
        .iter_mut()
        .find(|e| e.descriptor.id.as_str() == "acp.old-agent")
        .unwrap();
    old.health.as_mut().unwrap().signed_in = reno_proto::SignedIn::No {
        hint: "run its login".into(),
    };
    old.maintenance.as_mut().unwrap().sign_in = Some(reno_proto::PersonStep {
        label: "Sign in".into(),
        command: "generic login".into(),
    });
    daemon(vcx, &store).set_executors(list);
    wait_drawn(vcx, |vcx| painted(vcx, "sign-in-acp.old-agent"));
    assert_eq!(row_of(vcx, &root, "acp.old-agent").group, Group::NeedsYou);
    click(vcx, "sign-in-acp.old-agent");
    let own = "/fake/agents/old-agent/bin login".to_owned();
    assert_eq!(
        recording.opened.borrow().as_slice(),
        [("Sign in to Old Agent".to_owned(), own.clone())]
    );
    // The opened line has the same step with the same command, and no browser sign-in: the
    // agent offers none.
    wait_drawn(vcx, |vcx| painted(vcx, "check-now-acp.old-agent"));
    click(vcx, "wait-cancel-acp.old-agent");
    click(vcx, "line-acp.old-agent");
    wait_drawn(vcx, |vcx| {
        painted(vcx, "registry-open-terminal-acp.old-agent")
    });
    click(vcx, "registry-open-terminal-acp.old-agent");
    assert_eq!(recording.opened.borrow().len(), 2);
    assert_eq!(recording.opened.borrow()[1].1, own);
    assert!(!painted(vcx, "registry-browser-signin-acp.old-agent"));
}

// --- Signing an added agent in, in the browser ---------------------------------------------------

/// The Executors page with Cline's opened line, both openers recording, and its sign-in scripted.
fn browser_signin_page(
    cx: &mut TestAppContext,
    script: Vec<reno_proto::SigninEvent>,
) -> (
    Entity<Store>,
    &mut VisualTestContext,
    Rc<crate::links::fake::Recording>,
    Rc<crate::terminal::fake::Recording>,
) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    let pages = Rc::new(crate::links::fake::Recording::default());
    let terminals = Rc::new(crate::terminal::fake::Recording::default());
    vcx.update(|_, cx| {
        crate::links::install(cx, pages.clone());
        crate::terminal::install(cx, terminals.clone());
    });
    wait_registry(vcx);
    roster_tab(vcx, &root);
    daemon(vcx, &store).script_signin(&"acp.cline".parse().unwrap(), script);
    click(vcx, "line-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-browser-signin-acp.cline"));
    (store, vcx, pages, terminals)
}

fn cline() -> reno_proto::ExecutorId {
    "acp.cline".parse().unwrap()
}

#[gpui::test]
fn browser_sign_in_opens_the_page_shows_the_code_and_ends_signed_in(cx: &mut TestAppContext) {
    use reno_proto::SigninEvent;
    let (store, vcx, pages, terminals) = browser_signin_page(
        cx,
        vec![
            SigninEvent::Page {
                url: "https://example.com/device".into(),
            },
            SigninEvent::Output {
                line: "Your code: ABCD-1234".into(),
            },
        ],
    );
    click(vcx, "registry-browser-signin-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-signin-lines-acp.cline"));
    // The page went to the opener, the code is on the line, and the button became Cancel.
    assert_eq!(
        pages.opened.borrow().as_slice(),
        ["https://example.com/device"]
    );
    assert!(painted(vcx, "registry-browser-cancel-acp.cline"));
    assert!(!painted(vcx, "registry-browser-signin-acp.cline"));
    assert!(store.read_with(vcx, |s, _| {
        s.signin(&cline())
            .is_some_and(|v| v.running() && v.lines == ["Your code: ABCD-1234"])
    }));
    // The agent's own first method was asked for by name.
    assert_eq!(
        daemon(vcx, &store).signins_started(),
        [(cline(), Some("browser".to_owned()))]
    );
    assert!(terminals.opened.borrow().is_empty());
    assert!(daemon(vcx, &store).finish_signin(&cline(), true, None));
    wait_drawn(vcx, |vcx| {
        toasts(vcx).iter().any(|m| m == "Cline signed in")
    });
    wait_drawn(vcx, |vcx| painted(vcx, "registry-browser-signin-acp.cline"));
    assert!(!painted(vcx, "registry-signin-lines-acp.cline"));
    assert!(store.read_with(vcx, |s, _| s.signin(&cline()).is_none()));
}

#[gpui::test]
fn browser_sign_in_never_opens_a_page_that_is_not_a_web_link(cx: &mut TestAppContext) {
    use reno_proto::SigninEvent;
    let (store, vcx, pages, _terminals) = browser_signin_page(
        cx,
        vec![
            SigninEvent::Page {
                url: "file:///Applications/Calculator.app".into(),
            },
            SigninEvent::Output {
                line: "still going".into(),
            },
        ],
    );
    click(vcx, "registry-browser-signin-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-signin-lines-acp.cline"));
    assert!(pages.opened.borrow().is_empty());
    assert!(store.read_with(vcx, |s, _| s.signin(&cline()).is_some_and(|v| !v.page)));
}

#[gpui::test]
fn browser_sign_in_that_needs_a_terminal_opens_it_with_the_command_and_says_so(
    cx: &mut TestAppContext,
) {
    use reno_proto::SigninEvent;
    let (_store, vcx, pages, terminals) = browser_signin_page(
        cx,
        vec![
            SigninEvent::NeedsTerminal {
                command: "cline auth".into(),
            },
            SigninEvent::Done {
                signed_in: false,
                message: Some("Finish signing in in Terminal.".into()),
            },
        ],
    );
    click(vcx, "registry-browser-signin-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-signin-failed-acp.cline"));
    assert_eq!(
        terminals.opened.borrow().as_slice(),
        [("Sign in to Cline".to_owned(), "cline auth".to_owned())]
    );
    assert!(pages.opened.borrow().is_empty());
    assert!(toasts(vcx).iter().all(|m| !m.contains("signed in")));
    // It can be tried again: the button is back.
    assert!(painted(vcx, "registry-browser-signin-acp.cline"));
}

#[gpui::test]
fn a_browser_sign_in_that_fails_says_why_on_the_line(cx: &mut TestAppContext) {
    use reno_proto::SigninEvent;
    let (store, vcx, pages, _terminals) = browser_signin_page(
        cx,
        vec![
            SigninEvent::Page {
                url: "http://localhost:8123/cb".into(),
            },
            SigninEvent::Done {
                signed_in: false,
                message: Some("the code expired".into()),
            },
        ],
    );
    click(vcx, "registry-browser-signin-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-signin-failed-acp.cline"));
    assert_eq!(
        pages.opened.borrow().as_slice(),
        ["http://localhost:8123/cb"]
    );
    assert!(store.read_with(vcx, |s, _| {
        s.signin(&cline()).is_some_and(|v| {
            v.phase == crate::store::SigninPhase::Failed("the code expired".to_owned())
        })
    }));
}

#[gpui::test]
fn cancel_stops_a_browser_sign_in_and_leaves_nothing_on_the_line(cx: &mut TestAppContext) {
    use reno_proto::SigninEvent;
    let (store, vcx, _pages, _terminals) = browser_signin_page(
        cx,
        vec![SigninEvent::Output {
            line: "Your code: WXYZ-9876".into(),
        }],
    );
    click(vcx, "registry-browser-signin-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-browser-cancel-acp.cline"));
    wait_drawn(vcx, |vcx| painted(vcx, "registry-signin-lines-acp.cline"));
    click(vcx, "registry-browser-cancel-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "registry-browser-signin-acp.cline"));
    assert!(store.read_with(vcx, |s, _| s.signin(&cline()).is_none()));
    assert!(!painted(vcx, "registry-signin-failed-acp.cline"));
    assert!(toasts(vcx).iter().all(|m| !m.contains("signed in")));
}

#[gpui::test]
fn an_agent_that_needs_a_key_offers_add_key_which_opens_its_editor(cx: &mut TestAppContext) {
    let (store, root, vcx) = registry_page(cx, registry_config(60));
    wait_registry(vcx);
    roster_tab(vcx, &root);
    let mut list = store.read_with(vcx, |s, _| s.state().executors().to_vec());
    let cline = list
        .iter_mut()
        .find(|e| e.descriptor.id.as_str() == "acp.cline")
        .unwrap();
    cline.health.as_mut().unwrap().signed_in = reno_proto::SignedIn::No {
        hint: "needs CLINE_API_KEY".into(),
    };
    daemon(vcx, &store).set_executors(list);
    wait_drawn(vcx, |vcx| painted(vcx, "add-key-acp.cline"));
    assert_eq!(row_of(vcx, &root, "acp.cline").group, Group::NeedsYou);
    click(vcx, "add-key-acp.cline");
    wait_drawn(vcx, |vcx| painted(vcx, "env-card-acp.cline"));
}

#[gpui::test]
fn the_registry_failing_says_so_with_retry_and_offline_keeps_the_copy(cx: &mut TestAppContext) {
    let store = store_with(cx, registry_config(50));
    executors_live(cx, &store);
    daemon(cx, &store).fail(
        "executor.registry.list",
        ErrorObject::new(ErrorCode::Internal, "the registry could not be read"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(3200.0)));
    registry_tab(vcx, &root);
    wait_drawn(vcx, |vcx| painted(vcx, "registry-failed"));
    assert!(matches!(
        registry_of(vcx, &root).unwrap().state,
        crate::vm::registry::ListState::Failed(_)
    ));
    assert!(toasts(vcx).is_empty(), "the group says it, not a toast");
    daemon(vcx, &store).clear_failures();
    click(vcx, "registry-retry");
    wait_registry(vcx);

    // Offline: the daemon serves the cached copy and says when it was fetched.
    daemon(vcx, &store).set_registry_offline(true);
    click(vcx, "registry-refresh");
    wait_drawn(vcx, |vcx| {
        registry_of(vcx, &root).is_some_and(|v| v.offline)
    });
    let checked = registry_of(vcx, &root).unwrap().checked.unwrap();
    assert!(checked.starts_with("offline \u{b7} showing the copy from"));
    assert!(painted(vcx, "registry-agent-goose"), "the copy still lists");
}
