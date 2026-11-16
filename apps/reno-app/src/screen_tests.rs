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

fn store_with(cx: &mut TestAppContext, mut config: FakeConfig) -> Entity<Store> {
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
fn a_held_action_offers_how_far_the_answer_reaches_and_the_rule_is_listed_and_revoked(
    cx: &mut TestAppContext,
) {
    use reno_proto::{RememberScope, StandingEffect};
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "approve-appr_keeperpush"));

    // The default is once: no hint, and the button says so.
    let keeper = "appr_keeperpush".parse().unwrap();
    assert_eq!(
        root.read_with(vcx, |r, cx| r.inbox().read(cx).scope_of(&keeper)),
        None
    );
    assert!(!painted(vcx, "scope-hint-appr_keeperpush"));
    // keeper's run has a bot, so the bot scope is offered.
    assert!(painted(vcx, "scope-This-bot-appr_keeperpush"));
    click(vcx, "scope-This-bot-appr_keeperpush");
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, cx| r.inbox().read(cx).scope_of(&keeper)),
        Some(RememberScope::Bot)
    );
    assert!(painted(vcx, "scope-hint-appr_keeperpush"));

    click(vcx, "approve-appr_keeperpush");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| matches!(s.rules(), Some(Ok(rules)) if rules.iter().any(|r| r.source_approval.as_ref() == Some(&keeper))))
    });
    let rule = store.read_with(vcx, |s, _| {
        s.rules()
            .and_then(|r| r.as_ref().ok())
            .and_then(|rules| {
                rules
                    .iter()
                    .find(|r| r.source_approval.as_ref() == Some(&keeper))
            })
            .cloned()
            .unwrap()
    });
    assert_eq!(rule.effect, StandingEffect::Allow);
    // The card is gone and its choice with it.
    wait_drawn(vcx, |vcx| !painted(vcx, "approve-appr_keeperpush"));

    // Settings, Policy: the rule is listed with a Revoke button, and revoking removes it.
    go(vcx, &root, Nav::Settings);
    click(vcx, "tab-Policy");
    let revoke = format!("revoke-{}", rule.id);
    wait_drawn(vcx, |vcx| painted(vcx, &revoke));
    click(vcx, &revoke);
    wait_drawn(vcx, |vcx| !painted(vcx, &revoke));
    assert!(store.read_with(
        vcx,
        |s, _| !matches!(s.rules(), Some(Ok(rules)) if rules.iter().any(|r| r.id == rule.id))
    ));
}

#[gpui::test]
fn an_older_daemon_without_rules_offers_no_scope(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_RULES.to_owned()],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "approve-appr_keeperpush"));
    assert!(!painted(vcx, "scope-Always-appr_keeperpush"));
    go(vcx, &root, Nav::Settings);
    click(vcx, "tab-Policy");
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| settings::policy(s.state()).is_some())
    });
    assert!(painted(vcx, "standing-rules"));
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
        ["codex", "acp.grok", "acp.hermes", "claude-code", "mock"],
        "signed out, too old, needs install, then the ready ones"
    );
    assert_eq!(view.lede, "3 executors need you");
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
        view.needing == 2 && !painted(vcx, "install-acp.hermes")
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
        3,
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

#[gpui::test]
fn deny_asks_for_a_reason_and_enter_sends_it(cx: &mut TestAppContext) {
    cx.update(crate::ui::composer::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "deny-appr_scoutfetch"));
    assert!(
        root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx))
            .reasons
    );

    // Deny opens the field and decides nothing yet.
    click(vcx, "deny-appr_scoutfetch");
    wait_drawn(vcx, |vcx| painted(vcx, "reason-appr_scoutfetch"));
    assert_eq!(crate::tests::approvals(vcx, &store), 2);

    let input = root
        .read_with(vcx, |r, cx| r.inbox().read(cx).reason_input().cloned())
        .expect("the reason field is open");
    input.update_in(vcx, |c, window, cx| {
        c.fill("not from that host, use the mirror", window, cx);
    });
    vcx.simulate_keystrokes("enter");
    wait_drawn(vcx, |vcx| {
        history_of(&store, vcx, "appr_scoutfetch")
            .is_some_and(|r| r.message.as_deref() == Some("not from that host, use the mirror"))
    });
    assert_eq!(crate::tests::approvals(vcx, &store), 1);
    assert!(
        root.read_with(vcx, |r, cx| r.inbox().read(cx).reason_input().is_none()),
        "the field closed"
    );
    // The Decided list shows the reason.
    wait_drawn(vcx, |vcx| {
        let v = root.read_with(vcx, |r, cx| r.inbox().read(cx).view(cx));
        v.history
            .rows()
            .iter()
            .any(|h| h.message.as_deref() == Some("not from that host, use the mirror"))
    });
}

#[gpui::test]
fn a_blank_reason_denies_without_one_and_escape_cancels(cx: &mut TestAppContext) {
    cx.update(crate::ui::composer::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Light);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "deny-appr_keeperpush"));

    // Escape: the field closes and the action is still held.
    click(vcx, "deny-appr_keeperpush");
    wait_drawn(vcx, |vcx| painted(vcx, "reason-appr_keeperpush"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(root.read_with(vcx, |r, cx| r.inbox().read(cx).reason_input().is_none()));
    assert_eq!(crate::tests::approvals(vcx, &store), 2);

    // Enter with nothing typed: denied, with no message.
    click(vcx, "deny-appr_keeperpush");
    wait_drawn(vcx, |vcx| painted(vcx, "reason-appr_keeperpush"));
    vcx.simulate_keystrokes("enter");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        history_of(&store, vcx, "appr_keeperpush").is_some()
    });
    let record = history_of(&store, vcx, "appr_keeperpush").unwrap();
    assert_eq!(record.status, reno_proto::ApprovalStatus::Denied);
    assert_eq!(record.message, None);
}

#[gpui::test]
fn on_an_older_daemon_deny_is_one_click(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_APPROVAL_MESSAGE.to_owned()],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    wait_until(cx, |cx| crate::tests::approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    go(vcx, &root, Nav::Inbox);
    wait_drawn(vcx, |vcx| painted(vcx, "deny-appr_scoutfetch"));
    assert!(
        !root
            .read_with(vcx, |r, cx| r.inbox().read(cx).view(cx))
            .reasons
    );
    click(vcx, "deny-appr_scoutfetch");
    wait_until(vcx, |cx| crate::tests::approvals(cx, &store) == 1);
}
