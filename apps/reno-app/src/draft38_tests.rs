//! ROAD-38: first run and the shell around it (the draft's three variants), against the fake
//! daemon's first-run world: setup and its rules, the connecting gate, restarting renod, Skip on
//! agent updates, Copy link, `open_link`, and archived threads leaving ⌘K. They assert state and
//! words, never pixels. Every test that could reach the browser or Terminal installs a recorder,
//! so a leak fails the test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_proto::ThreadId;

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    click, draw, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until, window,
};
use crate::ui::draft38::{self, Hooks, flow::SetupFlow};
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::daemon_restart::Phase;
use crate::vm::draft38::Variant;
use crate::vm::first_run::{AgentState, Step};

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

/// The first-run world, connected, with the draft on and every opener recorded.
fn world(cx: &mut TestAppContext, variant: Variant, hooks: Hooks) -> Entity<Store> {
    world_with(cx, variant, hooks, config())
}

fn world_with(
    cx: &mut TestAppContext,
    variant: Variant,
    hooks: Hooks,
    config: FakeConfig,
) -> Entity<Store> {
    one_window_test_at_a_time();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        crate::actions::bind_keys(cx);
        crate::links::install(cx, Rc::new(crate::links::fake::Recording::default()));
        crate::terminal::install(cx, Rc::new(crate::terminal::fake::Recording::default()));
        crate::folder_picker::install(
            cx,
            Rc::new(crate::folder_picker::fake::Recording::default()),
        );
        draft38::install(variant, hooks, cx);
    });
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().executors().len() == 5 && s.thread_of(&run_id("plan1")).is_some()
        })
    });
    store
}

/// A window with the draft's parts turned on, as the app does, the gate still up.
fn open_gated(
    cx: &mut TestAppContext,
    variant: Variant,
    hooks: Hooks,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let store = world(cx, variant, hooks);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| root.init_draft38(false, window, cx));
    draw(vcx);
    (store, root, vcx)
}

/// Waits the gate out: it lifts once the daemon has answered and it has been up long enough.
fn let_the_gate_go(vcx: &mut VisualTestContext) {
    vcx.executor().advance_clock(Duration::from_millis(2_000));
    vcx.run_until_parked();
    draw(vcx);
}

/// [`open_gated`], with the gate gone: the app as it is a moment after launch.
fn open(
    cx: &mut TestAppContext,
    variant: Variant,
    hooks: Hooks,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let (store, root, vcx) = open_gated(cx, variant, hooks);
    let_the_gate_go(vcx);
    (store, root, vcx)
}

fn flow(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Entity<SetupFlow> {
    root.read_with(vcx, |r, _| r.setup().cloned().expect("setup is showing"))
}

fn finding(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> String {
    let flow = flow(vcx, root);
    flow.read_with(vcx, |f, cx| f.finding(cx))
}

fn onboarded(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|_, cx| crate::prefs::current(cx).onboarded)
}

const FINDING: &str = "3 of 5 agents can take runs on this Mac: Claude Code, Codex and Pi. \
                       Grok needs signing in; Cursor is not installed.";

// ---- the switch and the options ----------------------------------------------------------------

#[gpui::test]
fn the_keys_cycle_the_variants_and_the_status_bar_names_the_one_showing(cx: &mut TestAppContext) {
    let (_store, _root, vcx) = open(cx, Variant::A, Hooks::default());
    assert!(painted(vcx, "draft38-label"));
    assert_eq!(vcx.update(|_, cx| draft38::current(cx)), Some(Variant::A));
    vcx.simulate_keystrokes("cmd-alt-right");
    draw(vcx);
    assert_eq!(vcx.update(|_, cx| draft38::current(cx)), Some(Variant::B));
    vcx.simulate_keystrokes("cmd-alt-left cmd-alt-left");
    assert_eq!(vcx.update(|_, cx| draft38::current(cx)), Some(Variant::C));
}

// ---- setup -------------------------------------------------------------------------------------

#[gpui::test]
fn every_variant_states_the_finding_and_keeps_the_jobs_control_on_the_agent_it_names(
    cx: &mut TestAppContext,
) {
    for variant in Variant::ALL {
        let (_store, root, vcx) = open(cx, variant, Hooks::default());
        wait_drawn(vcx, |vcx| painted(vcx, "setup-finding"));
        assert_eq!(finding(vcx, &root), FINDING, "{variant:?}");
        // One click away on the agent that is named: Sign in on Grok, Install on Cursor.
        assert!(painted(vcx, "setup-sign-in-acp.grok"), "{variant:?}");
        assert!(painted(vcx, "setup-install-cursor"), "{variant:?}");
        assert!(
            !painted(vcx, "setup-sign-in-codex") && !painted(vcx, "setup-install-codex"),
            "a ready agent has no job: {variant:?}"
        );
    }
}

#[gpui::test]
fn sign_in_and_install_are_the_executor_actions_and_the_finding_follows(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::C, Hooks::default());
    wait_drawn(vcx, |vcx| painted(vcx, "setup-sign-in-acp.grok"));
    click(vcx, "setup-sign-in-acp.grok");
    click(vcx, "setup-install-cursor");
    draw(vcx);
    // The daemon was asked, through the existing seams.
    let signins = store.read_with(vcx, |s, _| s.fake().unwrap().signins_started());
    assert_eq!(signins.len(), 1);
    assert_eq!(signins[0].0.as_str(), "acp.grok");
    // Both are on their way (or, on a fast machine, already there): neither is still blocked.
    let now = finding(vcx, &root);
    assert!(
        !now.contains("Grok needs signing in") && !now.contains("Cursor is not installed"),
        "{now}"
    );
    // Both land; the sentence says all five can take runs.
    wait_drawn(vcx, |vcx| {
        finding(vcx, &root).starts_with("All 5 agents can take runs on this Mac")
    });
    assert!(!painted(vcx, "setup-sign-in-acp.grok") && !painted(vcx, "setup-install-cursor"));
}

#[gpui::test]
fn skipping_setup_leaves_nothing_half_done(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Hooks::default());
    wait_drawn(vcx, |vcx| painted(vcx, "setup-install-cursor"));
    assert!(!onboarded(vcx));
    // An install is under way, the look was changed, a project was chosen: then Skip.
    click(vcx, "setup-install-cursor");
    click(vcx, "setup-continue");
    click(vcx, "setup-mode-light");
    click(vcx, "setup-continue");
    click(vcx, "setup-continue");
    click(vcx, "setup-project-atlas-api");
    click(vcx, "setup-skip");
    draw(vcx);
    // Over, and never shown again.
    assert!(onboarded(vcx));
    assert!(root.read_with(vcx, |r, _| r.setup().is_none()));
    assert!(!painted(vcx, "setup-wizard"));
    // What was chosen is kept whole: the look, and Home working in that project.
    assert_eq!(
        vcx.update(|_, cx| crate::prefs::current(cx).mode),
        crate::prefs::Mode::Light
    );
    assert_eq!(
        root.read_with(vcx, |r, _| r.home_folder().cloned()),
        Some(crate::vm::projects::FolderKey::Repo(
            "/demo/atlas-api".into()
        ))
    );
    // And the install that was started was not cancelled by leaving: Cursor lands.
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.state().executors().iter().any(|e| {
                e.descriptor.id.as_str() == "cursor"
                    && e.job.is_none()
                    && e.health
                        .as_ref()
                        .is_some_and(|h| h.installed.version().is_some())
            })
        })
    });
}

#[gpui::test]
fn setup_runs_once_then_never_again_and_a_hook_can_force_it(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::C, Hooks::default());
    assert!(root.read_with(vcx, |r, _| r.setup().is_some()));
    click(vcx, "setup-start");
    draw(vcx);
    assert!(onboarded(vcx));
    assert!(root.read_with(vcx, |r, _| r.setup().is_none()));
    // A second window of the same app: onboarded, so no setup.
    let toasts = vcx.update(|_, cx| crate::ui::toast::install(cx));
    let again = vcx.update(|_, cx| {
        cx.open_window(gpui::WindowOptions::default(), |window, cx| {
            let store = store.clone();
            cx.new(|cx| {
                let mut root = RootView::new(store, toasts, Some(Appearance::Dark), window, cx);
                root.init_draft38(false, window, cx);
                root
            })
        })
        .unwrap()
    });
    assert!(again.read_with(vcx, |r, _| r.setup().is_none()).unwrap());
    // The hook shows it even so.
    vcx.update(|_, cx| {
        cx.set_global(Hooks {
            first_run: Some("1".into()),
            gate_hold: false,
            import: None,
        });
    });
    let (forced, _) = {
        let toasts = vcx.update(|_, cx| crate::ui::toast::install(cx));
        let store = store.clone();
        let handle = vcx.update(|_, cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                cx.new(|cx| {
                    let mut root = RootView::new(store, toasts, Some(Appearance::Dark), window, cx);
                    root.init_draft38(false, window, cx);
                    root
                })
            })
            .unwrap()
        });
        (handle, ())
    };
    assert!(forced.read_with(vcx, |r, _| r.setup().is_some()).unwrap());
}

#[gpui::test]
fn the_wizard_walks_five_steps_and_back(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Hooks::default());
    let step = |vcx: &mut VisualTestContext, root: &Entity<RootView>| {
        let f = flow(vcx, root);
        f.read_with(vcx, |f, _| f.setup.step)
    };
    assert_eq!(step(vcx, &root), Step::Welcome);
    assert!(!painted(vcx, "setup-back"), "nowhere to go back to");
    click(vcx, "setup-continue");
    click(vcx, "setup-continue");
    assert_eq!(step(vcx, &root), Step::Agents);
    assert!(painted(vcx, "setup-agent-codex") && painted(vcx, "setup-agent-pi"));
    click(vcx, "setup-back");
    assert_eq!(step(vcx, &root), Step::Look);
    click(vcx, "setup-continue");
    click(vcx, "setup-continue");
    click(vcx, "setup-continue");
    assert_eq!(step(vcx, &root), Step::Done);
    assert!(!painted(vcx, "setup-skip"), "nothing left to skip");
    click(vcx, "setup-start");
    assert!(onboarded(vcx));
}

#[gpui::test]
fn the_checklist_counts_what_is_done_and_goes_when_it_is_put_away(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::B, Hooks::default());
    wait_drawn(vcx, |vcx| painted(vcx, "setup-card"));
    let progress = |vcx: &mut VisualTestContext, root: &Entity<RootView>| {
        let f = flow(vcx, root);
        f.read_with(vcx, |f, cx| {
            let blocked = crate::vm::first_run::blocked(&f.lines(cx));
            f.setup.progress(blocked)
        })
    };
    assert_eq!(progress(vcx, &root), "0 of 4");
    click(vcx, "setup-item-appearance");
    click(vcx, "setup-mode-dark");
    click(vcx, "setup-item-first-project");
    click(vcx, "setup-project-reno");
    assert_eq!(progress(vcx, &root), "2 of 4");
    click(vcx, "setup-dismiss");
    draw(vcx);
    assert!(!painted(vcx, "setup-card"));
    assert!(onboarded(vcx));
}

// ---- the connecting gate -----------------------------------------------------------------------

#[gpui::test]
fn the_first_frame_is_drawn_before_the_daemon_has_said_anything(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        draft38::install(Variant::A, Hooks::default(), cx);
    });
    // A store that was never started: nothing will ever answer.
    let store = cx.new(|_| Store::connecting());
    for variant in Variant::ALL {
        cx.update(|cx| draft38::show(variant, cx));
        let (root, vcx) = window(cx, &store, Appearance::Dark);
        root.update_in(vcx, |root, window, cx| root.init_draft38(false, window, cx));
        draw(vcx);
        let drawn = match variant {
            Variant::A => painted(vcx, "gate-line"),
            Variant::B => painted(vcx, "gate-progress") && painted(vcx, "gate-skeleton"),
            Variant::C => painted(vcx, "gate-list"),
        };
        assert!(
            drawn,
            "{variant:?} says what it is doing on the first frame"
        );
        assert!(
            painted(vcx, "status-bar"),
            "the bar is there under the gate"
        );
        assert_eq!(
            root.read_with(vcx, |r, _| r.gate_phase()),
            Some(draft38::gate::Phase::Up)
        );
    }
}

#[gpui::test]
fn the_gate_fades_once_the_daemon_has_answered_and_goes(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open_gated(cx, Variant::A, Hooks::default());
    assert!(painted(vcx, "gate-line"));
    vcx.executor().advance_clock(Duration::from_millis(1_400));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(draft38::gate::Phase::Fading)
    );
    vcx.executor().advance_clock(Duration::from_millis(900));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.gate_phase()), None);
    assert!(!painted(vcx, "gate-splash"));
}

#[gpui::test]
fn a_held_gate_stays_up(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open_gated(
        cx,
        Variant::C,
        Hooks {
            first_run: None,
            gate_hold: true,
            import: None,
        },
    );
    vcx.executor().advance_clock(Duration::from_secs(10));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(draft38::gate::Phase::Up)
    );
    assert!(painted(vcx, "gate-list"));
}

#[gpui::test]
fn the_gate_does_not_hide_an_offline_daemon(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        draft38::install(Variant::A, Hooks::default(), cx);
    });
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.go_offline("Daemon not running", cx);
        store
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| root.init_draft38(true, window, cx));
    vcx.executor().advance_clock(Duration::from_millis(1_400));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.gate_phase()),
        Some(draft38::gate::Phase::Fading),
        "Home's notice and Retry must be reachable"
    );
}

// ---- restarting renod --------------------------------------------------------------------------

fn skip_setup(vcx: &mut VisualTestContext, root: &Entity<RootView>) {
    click(vcx, "setup-skip");
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.setup().is_none()));
}

fn running_version(vcx: &mut VisualTestContext, store: &Entity<Store>) -> String {
    store.read_with(vcx, |s, _| s.daemon_versions().unwrap().running)
}

#[gpui::test]
fn each_variant_offers_a_restart_that_asks_first_and_says_what_happens_to_live_runs(
    cx: &mut TestAppContext,
) {
    for variant in Variant::ALL {
        let store = world(cx, variant, Hooks::default());
        let (root, vcx) = window(cx, &store, Appearance::Dark);
        root.update_in(vcx, |root, window, cx| root.init_draft38(true, window, cx));
        let_the_gate_go(vcx);
        let live = store.read_with(vcx, |s, _| crate::vm::shell::live_runs(s.state()).len());
        // The affordance is where the variant puts it.
        let affordance = match variant {
            Variant::A => "restart-pill",
            Variant::B => "restart-banner-action",
            Variant::C => "bar-daemon",
        };
        assert!(painted(vcx, affordance), "{variant:?}");
        click(vcx, affordance);
        draw(vcx);
        if variant == Variant::C {
            // The card first, and then the question.
            assert!(painted(vcx, "restart-ask"));
            click(vcx, "restart-ask");
            draw(vcx);
        }
        assert!(
            painted(vcx, "restart-live-words"),
            "{variant:?} asks before it restarts"
        );
        assert_eq!(
            root.read_with(vcx, |r, _| r.restart_phase().clone()),
            Phase::Asking
        );
        assert_eq!(
            running_version(vcx, &store),
            "0.4.1",
            "asking changes nothing"
        );
        // Not now: nothing happened.
        click(vcx, "restart-cancel");
        draw(vcx);
        assert_eq!(running_version(vcx, &store), "0.4.1");
        assert_eq!(
            root.read_with(vcx, |r, _| r.restart_phase().clone()),
            Phase::Idle
        );
        assert!(live > 0, "the demo world has live runs to be told about");
    }
}

#[gpui::test]
fn a_restart_runs_the_installed_version_and_the_affordance_goes(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
    click(vcx, "restart-pill");
    click(vcx, "restart-confirm");
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Restarting
    );
    assert!(painted(vcx, "restart-pill"), "it says it is restarting");
    vcx.executor().advance_clock(Duration::from_millis(2_100));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(running_version(vcx, &store), "0.4.2");
    assert_eq!(
        root.read_with(vcx, |r, _| r.restart_phase().clone()),
        Phase::Idle
    );
    assert!(!painted(vcx, "restart-pill"), "nothing newer is installed");
}

#[gpui::test]
fn a_restart_that_does_not_come_back_says_so_and_can_be_tried_again(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
    store.read_with(vcx, |s, _| s.demo_fail_next_restart());
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
    assert!(painted(vcx, "restart-retry"));
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

#[gpui::test]
fn a_daemon_that_cannot_restart_shows_no_affordance(cx: &mut TestAppContext) {
    for variant in Variant::ALL {
        let store = world(cx, variant, Hooks::default());
        store.update(cx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
        let (root, vcx) = window(cx, &store, Appearance::Dark);
        root.update_in(vcx, |root, window, cx| root.init_draft38(true, window, cx));
        let_the_gate_go(vcx);
        for id in ["restart-pill", "restart-banner"] {
            assert!(!painted(vcx, id), "{variant:?}: {id}");
        }
    }
}

// ---- skip this version -------------------------------------------------------------------------

#[gpui::test]
fn skip_hides_the_version_per_agent_and_is_remembered(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
    assert!(painted(vcx, "bar-updates"), "Pi has an update to take");
    click(vcx, "bar-updates");
    draw(vcx);
    assert!(painted(vcx, "skip-pi") && painted(vcx, "update-pi"));
    click(vcx, "skip-pi");
    draw(vcx);
    assert!(
        !painted(vcx, "bar-updates"),
        "the island hides a skipped version"
    );
    assert!(!painted(vcx, "updates-menu"), "and the list closes with it");
    let skipped = vcx.update(|_, cx| crate::prefs::current(cx).skipped_updates);
    assert_eq!(skipped.get("pi").map(String::as_str), Some("1.6.0"));
}

// ---- copy link and open_link -------------------------------------------------------------------

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

#[gpui::test]
fn the_row_menu_copies_a_link_to_the_thread(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
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
    assert!(painted(vcx, "sidebar-menu-copy-link"));
    click(vcx, "sidebar-menu-copy-link");
    assert_eq!(clipboard(vcx).as_deref(), Some("reno://thread/thr_plan1"));
    assert!(
        vcx.update(|_, cx| crate::ui::toast::shown(cx))
            .iter()
            .any(|(_, m)| m.contains("link"))
    );
}

#[gpui::test]
fn open_link_focuses_the_thread_and_refuses_anything_else(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
    let focused = |vcx: &mut VisualTestContext, store: &Entity<Store>| {
        store.read_with(vcx, |s, _| s.looking_at().cloned())
    };
    assert_eq!(focused(vcx, &store), None);
    // An unknown thread, and things that are not thread links, open nothing.
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
    let (_store, root, vcx) = open(cx, Variant::A, Hooks::default());
    skip_setup(vcx, &root);
    wait_drawn(vcx, |vcx| painted(vcx, "recent-run_plan1"));
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    let titles = |vcx: &mut VisualTestContext, root: &Entity<RootView>| -> Vec<String> {
        root.read_with(vcx, |r, cx| {
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
        })
    };
    // Typing finds a live thread of `reno` and not the archived one.
    vcx.simulate_input("trim the CI");
    draw(vcx);
    assert!(
        titles(vcx, &root)
            .iter()
            .all(|t| !t.contains("trim the CI cache")),
        "archived: {:?}",
        titles(vcx, &root)
    );
}

#[gpui::test]
fn without_the_draft_an_archived_thread_is_still_in_the_palette(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        crate::actions::bind_keys(cx);
    });
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config(), cx);
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.thread_of(&run_id("arch1")).is_some())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |r, window, cx| r.focus_composer(window, cx));
    draw(vcx);
    vcx.simulate_keystrokes("cmd-k");
    vcx.simulate_input("trim the CI");
    draw(vcx);
    let found = root.read_with(vcx, |r, cx| {
        let sheet = r.palette().expect("open").read(cx).sheet();
        sheet.sections.iter().flat_map(|s| s.items.iter()).any(
            |i| matches!(i, crate::vm::palette::Item::Thread(t) if t.title.contains("trim the CI")),
        )
    });
    assert!(found, "the app is as it was without --draft38");
    let _ = AgentState::Ready;
}

// ---- the first project: import from this Mac ---------------------------------------------------

/// Setup at its project step, the folder panel answering with `answer` (`None`: cancelled).
fn at_the_project_step<'a>(
    cx: &'a mut TestAppContext,
    no_projects: bool,
    answer: Option<&str>,
) -> (
    Rc<crate::folder_picker::fake::Recording>,
    Entity<RootView>,
    &'a mut VisualTestContext,
) {
    let store = world_with(
        cx,
        Variant::A,
        Hooks::default(),
        FakeConfig {
            no_projects,
            ..config()
        },
    );
    let panel = Rc::new(crate::folder_picker::fake::Recording::default());
    *panel.answer.borrow_mut() = answer.map(std::path::PathBuf::from);
    cx.update(|cx| crate::folder_picker::install(cx, panel.clone()));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |root, window, cx| root.init_draft38(false, window, cx));
    let_the_gate_go(vcx);
    let f = flow(vcx, &root);
    f.update(vcx, |f, cx| f.go(Step::Project, cx));
    draw(vcx);
    (panel, root, vcx)
}

fn found(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<String> {
    let f = flow(vcx, root);
    f.read_with(vcx, |f, cx| f.found(cx))
}

#[gpui::test]
fn import_from_this_mac_asks_the_panel_and_the_folder_becomes_the_first_project(
    cx: &mut TestAppContext,
) {
    let (panel, root, vcx) = at_the_project_step(cx, true, Some("/demo/reno"));
    assert!(painted(vcx, "setup-import") && painted(vcx, "setup-clone"));
    assert_eq!(panel.asked.get(), 0, "nothing is asked until it is pressed");
    click(vcx, "setup-import");
    wait_drawn(vcx, |vcx| painted(vcx, "setup-found"));
    assert_eq!(panel.asked.get(), 1);
    assert_eq!(
        found(vcx, &root).as_deref(),
        Some("reno \u{b7} a git repository on main \u{b7} runs get their own worktree")
    );
    assert_eq!(
        root.read_with(vcx, |r, _| r.home_folder().cloned()),
        Some(crate::vm::projects::FolderKey::Repo("/demo/reno".into()))
    );
    let f = flow(vcx, &root);
    assert_eq!(
        f.read_with(vcx, |f, _| f.setup.project.clone()).as_deref(),
        Some("reno")
    );
}

#[gpui::test]
fn a_plain_folder_says_runs_work_in_the_folder_itself(cx: &mut TestAppContext) {
    let (_panel, root, vcx) = at_the_project_step(cx, true, Some("/demo/notes"));
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
        (Some("/demo/todo.txt"), "That is not a folder"),
        (Some("/demo/locked-vault"), "Reno cannot read that folder"),
    ] {
        let (_panel, root, vcx) = at_the_project_step(cx, true, answer);
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
    let (panel, root, vcx) = at_the_project_step(cx, true, None);
    click(vcx, "setup-import");
    draw(vcx);
    assert_eq!(panel.asked.get(), 1);
    let f = flow(vcx, &root);
    assert_eq!(f.read_with(vcx, |f, _| f.import_error.clone()), None);
    assert_eq!(f.read_with(vcx, |f, _| f.setup.project.clone()), None);
}

#[gpui::test]
fn known_projects_are_listed_only_when_there_are_some(cx: &mut TestAppContext) {
    let (_panel, _root, vcx) = at_the_project_step(cx, true, None);
    assert!(
        !painted(vcx, "setup-project-reno"),
        "a true first run lists none"
    );
    let (_panel, _root, vcx) = at_the_project_step(cx, false, None);
    for name in ["reno", "web-console", "atlas-api"] {
        assert!(painted(vcx, &format!("setup-project-{name}")), "{name}");
    }
}

#[gpui::test]
fn clone_from_url_uses_the_places_clone_and_its_failure_words(cx: &mut TestAppContext) {
    let (_panel, root, vcx) = at_the_project_step(cx, true, None);
    click(vcx, "setup-clone");
    let f = flow(vcx, &root);
    f.update(vcx, |f, cx| {
        f.url
            .update(cx, |u, cx| u.set_text("https://example.com/x.git", cx))
    });
    click(vcx, "setup-clone-go");
    vcx.executor().advance_clock(Duration::from_millis(800));
    vcx.run_until_parked();
    draw(vcx);
    assert_eq!(
        f.read_with(vcx, |f, _| f.clone.clone()),
        Some(crate::ui::draft38::flow::CloneUi::Failed(
            "Could not clone: repository not found".to_owned()
        ))
    );
    f.update(vcx, |f, cx| {
        f.url.update(cx, |u, cx| {
            u.set_text("https://github.com/ada/atlas-docs.git", cx)
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
fn a_daemon_that_cannot_take_a_folder_in_shows_no_import(cx: &mut TestAppContext) {
    let (_panel, root, vcx) = at_the_project_step(cx, false, None);
    let store = flow(vcx, &root).read_with(vcx, |f, _| f.store.clone());
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    draw(vcx);
    assert!(!painted(vcx, "setup-import") && !painted(vcx, "setup-clone"));
}
