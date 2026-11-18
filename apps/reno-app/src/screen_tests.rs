//! The Run, Project, Runs, Triggers and Settings screens against the fake daemon: loading, empty,
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
fn executors_lead_with_what_needs_you_and_check_again_probes(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "check-codex"));
    let view = executor_view(vcx, &root);
    let order: Vec<&str> = view.rows.rows().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        order,
        [
            "codex",
            "acp.grok",
            "acp.hermes",
            "acp.devin",
            "acp.fx",
            "acp.antigravity",
            "claude-code",
            "mock"
        ],
        "signed out, too old, needs install or not found, then the ready ones"
    );
    assert_eq!(view.lede, "5 executors need you");
    assert!(
        painted(vcx, "install-acp.hermes"),
        "only one can be installed"
    );
    assert!(!painted(vcx, "install-codex"));

    // "Check again" probes that executor and nothing else.
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

    // "Check all again" probes every one.
    click(vcx, "check-all");
    wait_drawn(vcx, |vcx| stamp(vcx, "mock") > mock);
    wait_drawn(vcx, |vcx| !store.read_with(vcx, |s, _| s.is_checking(None)));
}

#[gpui::test]
fn installing_an_executor_fetches_its_adapter_and_it_leaves_the_needs_list(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    executors_live(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Executors);
    wait_drawn(vcx, |vcx| painted(vcx, "install-acp.hermes"));
    click(vcx, "install-acp.hermes");
    wait_drawn(vcx, |vcx| {
        let view = executor_view(vcx, &root);
        view.needing == 4 && !painted(vcx, "install-acp.hermes")
    });
    let view = executor_view(vcx, &root);
    let hermes = view
        .rows
        .rows()
        .iter()
        .find(|r| r.id == "acp.hermes")
        .unwrap();
    assert_eq!(hermes.installed.text, "Installed");
    assert!(!store.read_with(vcx, |s, _| s.is_installing(&"acp.hermes".parse().unwrap())));
}

#[gpui::test]
fn a_failed_install_keeps_the_screen_and_its_button_usable(cx: &mut TestAppContext) {
    let store = fake_store(cx);
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
        executor_view(vcx, &root).needing,
        5,
        "nothing was installed"
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
    wait_drawn(vcx, |vcx| painted(vcx, "run-open-executors"));
    // The signed-out reason is on the run's own screen, with the way to the Executors screen.
    assert!(painted(vcx, "run-check-executor"));
    click(vcx, "run-open-executors");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Executors);
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
fn deny_in_the_run_sends_what_was_typed_beside_it(cx: &mut TestAppContext) {
    cx.update(crate::ui::composer::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open_run_screen(vcx, &root, "scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "deny"));
    let approval = reno_proto::ApprovalId::from_suffix("scoutfetch").unwrap();
    let input = screen
        .read_with(vcx, |s, _| s.reason_input(&approval).cloned())
        .expect("the reason field sits under the card");
    input.update_in(vcx, |c, window, cx| {
        c.fill("use the mirror instead", window, cx);
    });
    click(vcx, "deny");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        history_of(&store, vcx, "appr_scoutfetch")
            .is_some_and(|r| r.message.as_deref() == Some("use the mirror instead"))
    });
}

#[gpui::test]
fn enter_in_the_reason_field_denies_too(cx: &mut TestAppContext) {
    cx.update(crate::ui::composer::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open_run_screen(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "deny"));
    let approval = reno_proto::ApprovalId::from_suffix("keeperpush").unwrap();
    let input = screen
        .read_with(vcx, |s, _| s.reason_input(&approval).cloned())
        .expect("the reason field");
    input.update_in(vcx, |c, window, cx| {
        c.fill("not tonight", window, cx);
        c.submit(window, cx);
    });
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        history_of(&store, vcx, "appr_keeperpush")
            .is_some_and(|r| r.message.as_deref() == Some("not tonight"))
    });
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
    click(vcx, "tab-Policy");
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
fn a_runs_crumb_opens_its_project_whose_history_holds_only_its_decisions(cx: &mut TestAppContext) {
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

    click(vcx, "crumb-project");
    let atlas = FolderKey::Repo(std::path::PathBuf::from("/demo/atlas-api"));
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
fn without_rules_or_reasons_the_card_is_approve_and_deny(cx: &mut TestAppContext) {
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
    let screen = open_run_screen(vcx, &root, "scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "deny"));
    assert!(
        !painted(vcx, "approve-more"),
        "no menu without standing rules"
    );
    let approval = reno_proto::ApprovalId::from_suffix("scoutfetch").unwrap();
    assert!(
        !screen.read_with(vcx, |s, cx| s.shows_reason(&approval, cx)),
        "no reason field the daemon would drop"
    );
    click(vcx, "deny");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
}
