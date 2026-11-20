//! ROAD-38: first run and the shell around it, against the fake daemon's first-run world: setup
//! (the wizard) and its rules, the connecting gate, restarting renod, Skip on agent updates, Copy
//! link, `open_link`, and archived threads leaving ⌘K. Each test locks one decided thing; they
//! assert state and words, never pixels. Every test that could reach the browser, Terminal or the
//! folder panel installs a recorder, so a leak fails the test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_proto::ThreadId;

use crate::app::RootView;
use crate::folder_picker::fake::Recording as Panel;
use crate::links::fake::Recording as Browser;
use crate::store::Store;
use crate::terminal::fake::Recording as Terminal;
use crate::tests::{
    click, draw, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until, window,
};
use crate::ui::gate::Phase as GatePhase;
use crate::ui::setup::flow::{CloneUi, SetupFlow};
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::daemon_restart::Phase;
use crate::vm::first_run::{Hook, Step};

/// What a test can look at afterwards: what the opener, Terminal and folder panel were asked.
struct Recorders {
    browser: Rc<Browser>,
    terminal: Rc<Terminal>,
    panel: Rc<Panel>,
}

fn config() -> FakeConfig {
    FakeConfig {
        step: Duration::ZERO,
        first_run: true,
        activity_runs: true,
        sidebar_threads: true,
        job_duration: Duration::from_millis(80),
        ..FakeConfig::default()
    }
}

/// The first-run world, connected, with every opener recorded.
fn world(cx: &mut TestAppContext, config: FakeConfig) -> (Entity<Store>, Recorders) {
    one_window_test_at_a_time();
    let recorders = Recorders {
        browser: Rc::new(Browser::default()),
        terminal: Rc::new(Terminal::default()),
        panel: Rc::new(Panel::default()),
    };
    cx.update(|cx| {
        gpui_tokio::init(cx);
        crate::actions::bind_keys(cx);
        crate::ui::restart::bind_keys(cx);
        crate::links::install(cx, recorders.browser.clone());
        crate::terminal::install(cx, recorders.terminal.clone());
        crate::folder_picker::install(cx, recorders.panel.clone());
    });
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            !s.state().executors().is_empty() && s.thread_of(&run_id("plan1")).is_some()
        })
    });
    (store, recorders)
}

/// A window the way the app opens it on a first run, the daemon already connected.
fn open(
    cx: &mut TestAppContext,
    config: FakeConfig,
    hook: Option<Hook>,
) -> (
    Entity<Store>,
    Recorders,
    Entity<RootView>,
    &mut VisualTestContext,
) {
    let (store, recorders) = world(cx, config);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| {
        root.init_first_run(hook, false, window, cx);
        // As in the app: the composer has the focus.
        root.focus_composer(window, cx);
    });
    draw(vcx);
    (store, recorders, root, vcx)
}

fn flow(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Entity<SetupFlow> {
    root.read_with(vcx, |r, _| r.setup().cloned().expect("setup is showing"))
}

fn finding(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> String {
    let flow = flow(vcx, root);
    flow.read_with(vcx, |f, cx| f.finding(cx))
}

fn step(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Step {
    let flow = flow(vcx, root);
    flow.read_with(vcx, |f, _| f.setup.step)
}

fn onboarded(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|_, cx| crate::prefs::current(cx).onboarded)
}

const FINDING: &str = "3 of 5 agents can take runs on this Mac: Claude Code, Codex and Pi. \
                       Grok needs signing in; Cursor is not installed.";

// ---- the welcome screen ------------------------------------------------------------------------

#[gpui::test]
fn the_welcome_screen_states_the_finding_from_the_executors_and_it_follows_a_sign_in_and_an_install(
    cx: &mut TestAppContext,
) {
    let (_store, _r, root, vcx) = open(cx, config(), None);
    wait_drawn(vcx, |vcx| painted(vcx, "setup-finding"));
    assert_eq!(finding(vcx, &root), FINDING);
    click(vcx, "setup-sign-in-acp.grok");
    click(vcx, "setup-install-cursor");
    wait_drawn(vcx, |vcx| {
        finding(vcx, &root).starts_with("All 5 agents can take runs on this Mac")
    });
}

#[gpui::test]
fn sign_in_and_install_sit_on_the_agent_they_name_and_use_the_recorders(cx: &mut TestAppContext) {
    let (store, recorders, root, vcx) = open(cx, config(), None);
    wait_drawn(vcx, |vcx| painted(vcx, "setup-sign-in-acp.grok"));
    // On the agent each names, and on no agent that is ready.
    assert!(painted(vcx, "setup-agent-acp.grok") && painted(vcx, "setup-agent-cursor"));
    assert!(!painted(vcx, "setup-sign-in-codex") && !painted(vcx, "setup-install-codex"));
    click(vcx, "setup-sign-in-acp.grok");
    click(vcx, "setup-install-cursor");
    draw(vcx);
    let signins = store.read_with(vcx, |s, _| s.fake().unwrap().signins_started());
    assert_eq!(signins.len(), 1);
    assert_eq!(signins[0].0.as_str(), "acp.grok");
    wait_drawn(vcx, |vcx| {
        finding(vcx, &root).starts_with("All 5 agents can take runs")
    });
    // The sign-in's page went to the recorder, not to a browser; no Terminal was opened.
    assert!(!recorders.browser.opened.borrow().is_empty());
    assert!(recorders.terminal.opened.borrow().is_empty());
}

// ---- Skip, Back and Continue -------------------------------------------------------------------

#[gpui::test]
fn skip_setup_ends_it_and_it_never_shows_again(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = open(cx, config(), None);
    assert!(root.read_with(vcx, |r, _| r.setup().is_some()));
    assert!(!onboarded(vcx));
    click(vcx, "setup-skip");
    draw(vcx);
    assert!(onboarded(vcx));
    assert!(root.read_with(vcx, |r, _| r.setup().is_none()));
    assert!(!painted(vcx, "setup-wizard"));
    // A second window of the same app: onboarded, so no setup.
    let toasts = vcx.update(|_, cx| crate::ui::toast::install(cx));
    let again = vcx.update(|_, cx| {
        cx.open_window(gpui::WindowOptions::default(), |window, cx| {
            cx.new(|cx| {
                let mut root = RootView::new(store, toasts, Some(Appearance::Dark), window, cx);
                root.init_first_run(None, false, window, cx);
                root
            })
        })
        .unwrap()
    });
    assert!(again.read_with(vcx, |r, _| r.setup().is_none()).unwrap());
}

#[gpui::test]
fn back_and_continue_move_one_step_at_a_time(cx: &mut TestAppContext) {
    let (_s, _r, root, vcx) = open(cx, config(), None);
    assert_eq!(step(vcx, &root), Step::Welcome);
    assert!(!painted(vcx, "setup-back"), "nowhere to go back to");
    for want in [Step::Look, Step::Agents, Step::Project, Step::Done] {
        click(vcx, "setup-continue");
        draw(vcx);
        assert_eq!(step(vcx, &root), want);
    }
    assert!(!painted(vcx, "setup-skip"), "nothing left to skip");
    click(vcx, "setup-back");
    assert_eq!(step(vcx, &root), Step::Project);
    click(vcx, "setup-back");
    assert_eq!(step(vcx, &root), Step::Agents);
}

// ---- the first project -------------------------------------------------------------------------

/// Setup at its project step, with `answer` for the folder panel to give (`None`: cancelled).
fn at_the_project_step<'a>(
    cx: &'a mut TestAppContext,
    no_projects: bool,
    answer: Option<&str>,
) -> (Recorders, Entity<RootView>, &'a mut VisualTestContext) {
    let (_store, recorders, root, vcx) = open(
        cx,
        FakeConfig {
            no_projects,
            ..config()
        },
        Some(Hook::At(Step::Project)),
    );
    *recorders.panel.answer.borrow_mut() = answer.map(std::path::PathBuf::from);
    draw(vcx);
    (recorders, root, vcx)
}

fn found(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<String> {
    let f = flow(vcx, root);
    f.read_with(vcx, |f, cx| f.found(cx))
}

#[gpui::test]
fn import_from_this_mac_asks_the_folder_panel_and_the_folder_becomes_the_first_project(
    cx: &mut TestAppContext,
) {
    let (recorders, root, vcx) = at_the_project_step(cx, true, Some("/demo/reno"));
    assert_eq!(
        recorders.panel.asked.get(),
        0,
        "not asked until it is pressed"
    );
    click(vcx, "setup-import");
    wait_drawn(vcx, |vcx| painted(vcx, "setup-found"));
    assert_eq!(recorders.panel.asked.get(), 1);
    let f = flow(vcx, &root);
    assert_eq!(
        f.read_with(vcx, |f, _| f.setup.project.clone()).as_deref(),
        Some("reno")
    );
    assert_eq!(
        root.read_with(vcx, |r, _| r.home_folder().cloned()),
        Some(crate::vm::projects::FolderKey::Repo("/demo/reno".into()))
    );
}

#[gpui::test]
fn what_was_found_reads_right_for_a_git_repository_and_for_a_plain_folder(cx: &mut TestAppContext) {
    let (_p, root, vcx) = at_the_project_step(cx, true, Some("/demo/reno"));
    click(vcx, "setup-import");
    wait_drawn(vcx, |vcx| painted(vcx, "setup-found"));
    assert_eq!(
        found(vcx, &root).as_deref(),
        Some("reno \u{b7} a git repository on main \u{b7} runs get their own worktree")
    );
    let (_p, root, vcx) = at_the_project_step(cx, true, Some("/demo/notes"));
    click(vcx, "setup-import");
    wait_drawn(vcx, |vcx| painted(vcx, "setup-found"));
    assert_eq!(
        found(vcx, &root).as_deref(),
        Some("notes \u{b7} not a git repository \u{b7} runs work in the folder itself")
    );
}

#[gpui::test]
fn a_refused_folder_says_why_and_is_not_the_project_and_a_cancel_changes_nothing(
    cx: &mut TestAppContext,
) {
    for (answer, words) in [
        ("/demo/todo.txt", "That is not a folder"),
        ("/demo/locked-vault", "Reno cannot read that folder"),
    ] {
        let (_p, root, vcx) = at_the_project_step(cx, true, Some(answer));
        click(vcx, "setup-import");
        wait_drawn(vcx, |vcx| painted(vcx, "setup-import-error"));
        let f = flow(vcx, &root);
        assert_eq!(
            f.read_with(vcx, |f, _| f.import_error.clone()).as_deref(),
            Some(words)
        );
        assert_eq!(f.read_with(vcx, |f, _| f.setup.project.clone()), None);
        assert!(!painted(vcx, "setup-found"));
    }
    let (recorders, root, vcx) = at_the_project_step(cx, true, None);
    click(vcx, "setup-import");
    draw(vcx);
    assert_eq!(recorders.panel.asked.get(), 1);
    let f = flow(vcx, &root);
    assert_eq!(f.read_with(vcx, |f, _| f.import_error.clone()), None);
    assert_eq!(f.read_with(vcx, |f, _| f.setup.project.clone()), None);
}

#[gpui::test]
fn known_projects_are_listed_only_when_there_are_some(cx: &mut TestAppContext) {
    let (_p, _root, vcx) = at_the_project_step(cx, true, None);
    assert!(
        !painted(vcx, "setup-project-reno"),
        "a true first run lists none"
    );
    let (_p, _root, vcx) = at_the_project_step(cx, false, None);
    for name in ["reno", "web-console", "atlas-api"] {
        assert!(painted(vcx, &format!("setup-project-{name}")), "{name}");
    }
}

#[gpui::test]
fn clone_from_url_uses_the_places_clone_and_its_failure_words(cx: &mut TestAppContext) {
    let (_p, root, vcx) = at_the_project_step(cx, true, None);
    click(vcx, "setup-clone");
    let f = flow(vcx, &root);
    f.update(vcx, |f, cx| {
        f.url
            .update(cx, |u, cx| u.set_text("https://example.com/x.git", cx));
    });
    click(vcx, "setup-clone-go");
    vcx.executor().advance_clock(Duration::from_millis(800));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        f.read_with(vcx, |f, _| f.clone.clone()),
        Some(CloneUi::Failed(
            "Could not clone: repository not found".to_owned()
        ))
    );
    f.update(vcx, |f, cx| {
        f.url.update(cx, |u, cx| {
            u.set_text("https://github.com/ada/atlas-docs.git", cx);
        });
        f.start_clone(cx);
    });
    vcx.executor().advance_clock(Duration::from_millis(1_600));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        f.read_with(vcx, |f, _| f.setup.project.clone()).as_deref(),
        Some("atlas-docs")
    );
}

#[gpui::test]
fn the_done_step_names_the_look_the_agents_and_the_first_project(cx: &mut TestAppContext) {
    let (_p, root, vcx) = at_the_project_step(cx, true, Some("/demo/notes"));
    click(vcx, "setup-import");
    wait_drawn(vcx, |vcx| painted(vcx, "setup-found"));
    click(vcx, "setup-continue");
    draw(vcx);
    assert_eq!(step(vcx, &root), Step::Done);
    assert!(painted(vcx, "setup-summary"));
    let f = flow(vcx, &root);
    let rows = f.read_with(vcx, |f, cx| f.summary(cx));
    assert_eq!(
        rows,
        [
            ("Look", "System \u{b7} Vercel \u{b7} Blue".to_owned()),
            ("Agents", "3 of 5 can take runs".to_owned()),
            ("First project", "notes".to_owned()),
        ]
    );
}

#[gpui::test]
fn the_real_daemon_has_no_project_step_and_four_dots(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = open(cx, config(), None);
    assert!(painted(vcx, "setup-dot-4"), "the fake has five steps");
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    draw(vcx);
    assert!(painted(vcx, "setup-dot-3") && !painted(vcx, "setup-dot-4"));
    // Continue goes from the agents straight to the end.
    let f = flow(vcx, &root);
    f.update(vcx, |f, cx| f.go(Step::Agents, cx));
    click(vcx, "setup-continue");
    assert_eq!(step(vcx, &root), Step::Done);
    let rows = f.read_with(vcx, |f, cx| f.summary(cx));
    assert!(rows.iter().all(|(label, _)| *label != "First project"));
}

#[gpui::test]
fn a_reviewed_install_is_sent_to_the_executors_review_not_run(cx: &mut TestAppContext) {
    let (_s, _r, root, vcx) = open(cx, config(), None);
    let f = flow(vcx, &root);
    f.update(vcx, |f, cx| {
        f.act(
            "cursor",
            &crate::vm::first_run::Action::Install { review: true },
            cx,
        );
    });
    draw(vcx);
    assert!(onboarded(vcx), "setup is over");
    assert!(root.read_with(vcx, |r, _| r.setup().is_none()));
    assert_eq!(
        root.read_with(vcx, |r, _| r.nav()),
        crate::nav::Nav::Settings
    );
}

// ---- the connecting gate -----------------------------------------------------------------------

#[gpui::test]
fn the_gate_paints_at_once_and_lifts_the_moment_the_daemon_connects(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    // A store that has not started: nothing has answered, and the first frame is still drawn.
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| {
        root.init_first_run(Some(Hook::Skip), false, window, cx);
    });
    draw(vcx);
    assert!(painted(vcx, "gate-line") && painted(vcx, "status-bar"));
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(GatePhase::Up)
    );
    // The daemon connects: the gate fades at once, with no minimum, and then goes.
    store.update(vcx, |s, cx| s.start_fake_with(config(), cx));
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.gate_phase()) == Some(GatePhase::Fading)
    });
    vcx.executor().advance_clock(Duration::from_millis(700));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.gate_phase()), None);
    assert!(!painted(vcx, "gate-splash"));
}

#[gpui::test]
fn the_gate_does_not_hide_an_offline_daemon(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| {
        root.init_first_run(Some(Hook::Skip), false, window, cx);
    });
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(GatePhase::Up)
    );
    store.update(vcx, |s, cx| s.go_offline("Daemon not running", cx));
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(GatePhase::Fading),
        "Home's notice and Retry must be reachable"
    );
}

// ---- restarting renod --------------------------------------------------------------------------

fn skipped_setup(
    cx: &mut TestAppContext,
    config: FakeConfig,
) -> (
    Entity<Store>,
    Recorders,
    Entity<RootView>,
    &mut VisualTestContext,
) {
    open(cx, config, Some(Hook::Skip))
}

fn running_version(vcx: &mut VisualTestContext, store: &Entity<Store>) -> String {
    store.read_with(vcx, |s, _| s.daemon_versions().unwrap().running)
}

#[gpui::test]
fn the_restart_pill_shows_only_when_connected_and_the_versions_differ(cx: &mut TestAppContext) {
    // Versions differ and the daemon can restart: the pill.
    let (store, _r, _root, vcx) = skipped_setup(cx, config());
    assert!(painted(vcx, "restart-pill"));
    // A daemon that cannot restart: none.
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    draw(vcx);
    assert!(!painted(vcx, "restart-pill"));
    // The same versions: none.
    let (_s, _r, _root, vcx) = skipped_setup(
        cx,
        FakeConfig {
            first_run: false,
            ..config()
        },
    );
    assert!(!painted(vcx, "restart-pill"));
    // Not connected yet: none, although the versions differ.
    one_window_test_at_a_time();
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| {
        root.init_first_run(Some(Hook::Skip), false, window, cx);
    });
    draw(vcx);
    assert!(!painted(vcx, "restart-pill"));
}

#[gpui::test]
fn the_restart_confirm_asks_first_and_says_how_many_runs_are_live(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = skipped_setup(cx, config());
    click(vcx, "restart-pill");
    draw(vcx);
    assert!(
        painted(vcx, "restart-live-words"),
        "it asks before it restarts"
    );
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Asking
    );
    let live = store.read_with(vcx, |s, _| crate::vm::shell::live_runs(s.state()).len());
    assert!(live > 1, "the demo world has live runs to be told about");
    let words = root.read_with(vcx, |r, cx| r.restart_live_words(cx));
    assert_eq!(
        words,
        format!("{live} live runs keep going; the app reconnects.")
    );
    assert_eq!(
        running_version(vcx, &store),
        "0.4.1",
        "asking changes nothing"
    );
    click(vcx, "restart-cancel");
    draw(vcx);
    assert_eq!(running_version(vcx, &store), "0.4.1");
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Idle
    );
}

#[gpui::test]
fn a_restart_runs_the_installed_version_and_the_pill_goes(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = skipped_setup(cx, config());
    click(vcx, "restart-pill");
    click(vcx, "restart-confirm");
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Restarting
    );
    vcx.executor().advance_clock(Duration::from_millis(2_100));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(running_version(vcx, &store), "0.4.2");
    assert!(!painted(vcx, "restart-pill"), "nothing newer is installed");
}

#[gpui::test]
fn a_restart_that_does_not_come_back_says_so_and_can_be_tried_again(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = skipped_setup(cx, config());
    store.read_with(vcx, |s, _| s.fail_next_restart_for_test());
    click(vcx, "restart-pill");
    click(vcx, "restart-confirm");
    vcx.executor().advance_clock(Duration::from_millis(2_100));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Failed("renod did not come back; run `reno daemon start`".to_owned())
    );
    assert_eq!(running_version(vcx, &store), "0.4.1");
    click(vcx, "restart-retry");
    vcx.executor().advance_clock(Duration::from_millis(2_100));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        running_version(vcx, &store),
        "0.4.2",
        "the second try worked"
    );
}

// ---- skip this version -------------------------------------------------------------------------

#[gpui::test]
fn skip_hides_that_version_until_a_newer_one_appears(cx: &mut TestAppContext) {
    let (store, _r, _root, vcx) = skipped_setup(cx, config());
    assert!(painted(vcx, "bar-updates"), "Pi has an update to take");
    click(vcx, "bar-updates");
    draw(vcx);
    click(vcx, "skip-pi");
    draw(vcx);
    assert!(
        !painted(vcx, "bar-updates"),
        "the island hides a skipped version"
    );
    assert!(!painted(vcx, "updates-menu"), "and the list closes with it");
    let skipped = vcx.update(|_, cx| crate::prefs::current(cx).skipped_updates);
    assert_eq!(skipped.get("pi").map(String::as_str), Some("1.6.0"));
    // A newer version of Pi comes out: the island is back.
    let fake = store.read_with(vcx, |s, _| s.fake().unwrap().clone());
    let mut executors = store.read_with(vcx, |s, _| s.state().executors().to_vec());
    for info in &mut executors {
        if info.descriptor.id.as_str() == "pi"
            && let Some(updater) = info.maintenance.as_mut().and_then(|m| m.updater.as_mut())
        {
            updater.latest = Some("1.7.0".to_owned());
        }
    }
    fake.set_executors(executors);
    wait_drawn(vcx, |vcx| painted(vcx, "bar-updates"));
}

// ---- Copy link and open_link -------------------------------------------------------------------

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

#[gpui::test]
fn copy_link_gives_the_threads_link(cx: &mut TestAppContext) {
    let (_s, _r, root, vcx) = skipped_setup(cx, config());
    wait_drawn(vcx, |vcx| painted(vcx, "recent-run_plan1"));
    let thread = ThreadId::from_suffix("plan1").unwrap();
    root.update_in(vcx, |root, window, cx| {
        root.sidebar_act(
            crate::ui::sidebar::Act::Menu(thread.clone(), None),
            window,
            cx,
        );
    });
    draw(vcx);
    click(vcx, "sidebar-menu-copy-link");
    assert_eq!(clipboard(vcx).as_deref(), Some("reno://thread/thr_plan1"));
}

#[gpui::test]
fn open_link_focuses_the_thread_and_refuses_anything_else(cx: &mut TestAppContext) {
    let (store, _r, root, vcx) = skipped_setup(cx, config());
    let focused = |vcx: &mut VisualTestContext, store: &Entity<Store>| {
        store.read_with(vcx, |s, _| s.looking_at().cloned())
    };
    assert_eq!(focused(vcx, &store), None);
    for url in [
        "reno://thread/thr_nosuch",
        "reno://thread/not-an-id",
        "https://example.com/",
        "",
    ] {
        let opened = root.update_in(vcx, |root, window, cx| root.open_link(url, window, cx));
        assert!(!opened, "{url:?}");
        assert_eq!(focused(vcx, &store), None, "{url:?}");
    }
    let opened = root.update_in(vcx, |root, window, cx| {
        root.open_link("reno://thread/thr_plan1", window, cx)
    });
    assert!(opened);
    draw(vcx);
    assert_eq!(focused(vcx, &store), Some(run_id("plan1")));
}

// ---- archived threads leave ⌘K -----------------------------------------------------------------

#[gpui::test]
fn archived_threads_are_not_in_the_palette(cx: &mut TestAppContext) {
    let (_s, _r, root, vcx) = skipped_setup(cx, config());
    wait_drawn(vcx, |vcx| painted(vcx, "recent-run_plan1"));
    vcx.simulate_keystrokes("cmd-k");
    vcx.simulate_input("trim the CI");
    draw(vcx);
    let titles: Vec<String> = root.read_with(vcx, |r, cx| {
        let sheet = r.palette().expect("open").read(cx).sheet();
        sheet
            .sections
            .iter()
            .flat_map(|s| s.items.iter())
            .filter_map(|i| match i {
                crate::vm::palette::Item::Thread(t) => Some(t.title.clone()),
                _ => None,
            })
            .collect()
    });
    assert!(
        titles.iter().all(|t| !t.contains("trim the CI cache")),
        "archived: {titles:?}"
    );
}
