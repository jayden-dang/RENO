//! Behaviour tests for definitions: one test per validation rule, the two-stage loader, the
//! canonical hash and the shipped defaults.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use super::*;
use crate::exec::{AccessMode, ActionClass};

fn ctx() -> ValidationContext {
    ValidationContext::with_builtin(["extra".parse().unwrap()])
}

fn file(kind: DefKind, name: &str, text: &str) -> DefFile {
    DefFile {
        kind,
        name: name.to_owned(),
        text: text.to_owned(),
    }
}

const BOT: &str = r#"
name = "keeper"
instructions = "Keep dependencies current."
executors = ["claude-code", "codex"]
triggers = ["nightly"]
"#;

const TRIGGER: &str = r#"
name = "nightly"

[when]
kind = "cron"
cron = "0 2 * * *"
timezone = "Asia/Ho_Chi_Minh"

[target]
bot = "keeper"
prompt = "Check for outdated dependencies."
"#;

fn bot_with(extra: &str) -> String {
    format!("name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"codex\"]\n{extra}\n")
}

fn trigger_with(when: &str) -> String {
    format!("name = \"nightly\"\n[when]\n{when}\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n")
}

fn load(files: &[DefFile]) -> Result<DefSet, Vec<DefError>> {
    load_set(files, &ctx())
}

/// The errors of loading one bot (no triggers around), as display strings.
fn bot_errors(text: &str) -> Vec<String> {
    show(load(&[file(DefKind::Bot, "keeper", text)]))
}

/// The errors of loading a bot "keeper" and one trigger "nightly", as display strings.
fn trigger_errors(text: &str) -> Vec<String> {
    show(load(&[
        file(DefKind::Bot, "keeper", &bot_with("")),
        file(DefKind::Trigger, "nightly", text),
    ]))
}

fn policy_errors(text: &str) -> Vec<String> {
    show(load(&[file(DefKind::Policy, "policy", text)]))
}

fn show(result: Result<DefSet, Vec<DefError>>) -> Vec<String> {
    match result {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    }
}

#[track_caller]
fn assert_one(errors: &[String], expected: &str) {
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].starts_with(expected),
        "expected an error starting `{expected}`, got `{}`",
        errors[0]
    );
}

// ---- the whole loader ---------------------------------------------------------------------

#[test]
fn a_valid_snapshot_loads_with_hashes() {
    let set = load(&[
        file(DefKind::Bot, "keeper", BOT),
        file(DefKind::Trigger, "nightly", TRIGGER),
        file(DefKind::Policy, "policy", defaults::POLICY),
    ])
    .unwrap();
    assert_eq!(set.len(), 3);
    assert!(!set.is_empty());
    let keeper = set.bot("keeper").unwrap();
    assert_eq!(keeper.access, AccessMode::Supervised);
    assert_eq!(keeper.hash().len(), 64);
    assert!(set.policy().is_some());
    let kinds: Vec<_> = set.entries().iter().map(|e| (e.kind, e.name)).collect();
    assert_eq!(
        kinds,
        [
            (DefKind::Bot, "keeper"),
            (DefKind::Trigger, "nightly"),
            (DefKind::Policy, "policy")
        ]
    );
}

#[test]
fn the_empty_snapshot_is_a_valid_empty_set() {
    let set = load(&[]).unwrap();
    assert!(set.is_empty());
    assert!(set.policy().is_none());
}

#[test]
fn every_error_is_returned_at_once_with_its_path() {
    let bot = r#"
name = "keeper"
instructions = "x"
executors = ["claude-code", "codex", "x"]
triggers = ["nightly", "ghost"]
"#;
    let errors = show(load(&[
        file(DefKind::Bot, "keeper", bot),
        file(
            DefKind::Trigger,
            "nightly",
            &TRIGGER.replace("keeper", "nobody"),
        ),
    ]));
    assert_eq!(
        errors,
        [
            "bots/keeper.toml: executors[2]: unknown executor \"x\"",
            "bots/keeper.toml: triggers[0]: trigger \"nightly\" starts bot \"nobody\", not this one",
            "bots/keeper.toml: triggers[1]: unknown trigger \"ghost\"",
            "triggers/nightly.toml: target.bot: unknown bot \"nobody\"",
        ]
    );
}

#[test]
fn errors_are_grouped_by_file_in_load_order_whatever_stage_found_them() {
    // The policy and a trigger fail to parse; the bot fails validation. Parse errors are found
    // first, but the report reads bots, triggers, policy.
    let errors = show(load(&[
        file(DefKind::Policy, "policy", "mode = 1"),
        file(DefKind::Trigger, "nightly", "name = "),
        file(DefKind::Bot, "keeper", &bot_with("team = \"X Y\"")),
    ]));
    let files: Vec<_> = errors
        .iter()
        .map(|e| e.split(':').next().unwrap())
        .collect();
    assert_eq!(
        files,
        ["bots/keeper.toml", "triggers/nightly.toml", "policy.toml"]
    );
}

#[test]
fn a_file_that_does_not_parse_is_not_also_reported_as_missing() {
    let errors = show(load(&[
        file(DefKind::Bot, "keeper", "name = "),
        file(DefKind::Trigger, "nightly", TRIGGER),
    ]));
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].starts_with("bots/keeper.toml: line 1, column 8"),
        "{errors:?}"
    );
}

#[test]
fn a_set_with_one_bad_file_loads_nothing() {
    let result = load(&[
        file(DefKind::Bot, "keeper", BOT),
        file(DefKind::Trigger, "nightly", &TRIGGER.replace("0 2", "99 2")),
    ]);
    assert!(result.is_err());
}

// ---- parsing ------------------------------------------------------------------------------

#[test]
fn an_unknown_field_is_an_error_with_its_line_and_column() {
    let errors = bot_errors(&bot_with("colour = \"red\""));
    assert_one(
        &errors,
        "bots/keeper.toml: line 4, column 1: unknown field `colour`",
    );
}

#[test]
fn a_missing_field_is_an_error() {
    let errors = bot_errors("name = \"keeper\"\nexecutors = [\"codex\"]\n");
    assert_one(&errors, "bots/keeper.toml: ");
    assert!(
        errors[0].contains("missing field `instructions`"),
        "{errors:?}"
    );
}

#[test]
fn a_malformed_executor_id_is_a_parse_error() {
    let errors = bot_errors(&BOT.replace("claude-code", "has space"));
    assert_one(&errors, "bots/keeper.toml: line 4");
}

#[test]
fn an_unknown_field_inside_a_trigger_kind_is_an_error() {
    let errors = trigger_errors(&trigger_with("kind = \"manual\"\nat = \"x\""));
    assert_one(&errors, "triggers/nightly.toml: line");
    assert!(errors[0].contains("unknown field `at`"), "{errors:?}");
}

#[test]
fn an_unknown_trigger_kind_is_an_error() {
    let errors = trigger_errors(&trigger_with("kind = \"hourly\""));
    assert_one(&errors, "triggers/nightly.toml: line");
    assert!(errors[0].contains("unknown variant `hourly`"), "{errors:?}");
}

#[test]
fn a_file_over_the_size_limit_is_refused() {
    let big = format!("{}\n# {}", bot_with(""), "x".repeat(MAX_FILE_BYTES));
    assert_one(&bot_errors(&big), "bots/keeper.toml: the file is ");
}

#[test]
fn a_policy_file_with_another_name_is_refused() {
    let errors = show(load(&[file(DefKind::Policy, "rules", "")]));
    assert_one(
        &errors,
        "policy.toml: the policy file must be named policy.toml",
    );
}

#[test]
fn the_same_definition_twice_is_refused() {
    let errors = show(load(&[
        file(DefKind::Bot, "keeper", &bot_with("")),
        file(DefKind::Bot, "keeper", &bot_with("")),
    ]));
    assert_one(&errors, "bots/keeper.toml: this definition appears twice");
}

// ---- bot rules ----------------------------------------------------------------------------

#[test]
fn bot_name_must_be_a_legal_name() {
    for bad in ["", "Keeper", "-x", "a b", &"a".repeat(65)] {
        let text = bot_with("").replace("\"keeper\"", &format!("{bad:?}"));
        let errors = bot_errors(&text);
        assert_one(&errors, "bots/keeper.toml: name: ");
        assert!(
            errors[0].contains("is not a valid name"),
            "{bad}: {errors:?}"
        );
    }
}

#[test]
fn bot_name_must_equal_the_file_stem() {
    let errors = show(load(&[file(DefKind::Bot, "other", &bot_with(""))]));
    assert_one(
        &errors,
        "bots/other.toml: name: \"keeper\" must equal the file name \"other\"",
    );
}

#[test]
fn bot_instructions_must_not_be_blank() {
    let text = bot_with("").replace("instructions = \"x\"", "instructions = \"  \\n \"");
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: instructions: must not be empty",
    );
}

#[test]
fn bot_instructions_are_bounded() {
    let text = bot_with("").replace(
        "instructions = \"x\"",
        &format!("instructions = \"{}\"", "x".repeat(64 * 1024 + 1)),
    );
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: instructions: is 65537 bytes",
    );
}

#[test]
fn bot_must_list_an_executor() {
    let text = bot_with("").replace("[\"codex\"]", "[]");
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: executors: must list at least one",
    );
}

#[test]
fn bot_executors_must_be_known() {
    let text = bot_with("").replace("[\"codex\"]", "[\"codex\", \"mock\", \"nope\"]");
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: executors[2]: unknown executor \"nope\"",
    );
}

#[test]
fn bot_executors_come_from_the_context() {
    let text = bot_with("").replace("[\"codex\"]", "[\"acp.grok\"]");
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: executors[0]: unknown executor",
    );
    let known = ValidationContext::new(["acp.grok".parse().unwrap()]);
    let set = load_set(&[file(DefKind::Bot, "keeper", &text)], &known);
    assert!(set.is_ok());
}

#[test]
fn bot_executors_must_not_repeat() {
    let text = bot_with("").replace("[\"codex\"]", "[\"codex\", \"codex\"]");
    assert_one(
        &bot_errors(&text),
        "bots/keeper.toml: executors[1]: duplicate executor \"codex\"",
    );
}

#[test]
fn bot_executors_are_bounded() {
    let many = (0..17).map(|_| "\"codex\"").collect::<Vec<_>>().join(", ");
    let text = bot_with("").replace("[\"codex\"]", &format!("[{many}]"));
    let errors = bot_errors(&text);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("executors: lists 17 executors; the limit is 16")),
        "{errors:?}"
    );
}

#[test]
fn bot_team_must_be_a_legal_name() {
    assert_one(
        &bot_errors(&bot_with("team = \"Core Team\"")),
        "bots/keeper.toml: team: \"Core Team\" is not a valid name",
    );
    assert!(bot_errors(&bot_with("team = \"core\"")).is_empty());
}

#[test]
fn bot_trigger_names_must_be_legal() {
    assert_one(
        &bot_errors(&bot_with("triggers = [\"Bad Name\"]")),
        "bots/keeper.toml: triggers[0]: \"Bad Name\" is not a valid name",
    );
}

#[test]
fn bot_triggers_must_exist() {
    assert_one(
        &bot_errors(&bot_with("triggers = [\"ghost\"]")),
        "bots/keeper.toml: triggers[0]: unknown trigger \"ghost\"",
    );
}

#[test]
fn bot_triggers_must_not_repeat() {
    let errors = show(load(&[
        file(
            DefKind::Bot,
            "keeper",
            &bot_with("triggers = [\"nightly\", \"nightly\"]"),
        ),
        file(DefKind::Trigger, "nightly", TRIGGER),
    ]));
    assert_one(
        &errors,
        "bots/keeper.toml: triggers[1]: duplicate trigger \"nightly\"",
    );
}

#[test]
fn bot_triggers_must_target_the_bot() {
    let errors = show(load(&[
        file(
            DefKind::Bot,
            "keeper",
            &bot_with("triggers = [\"nightly\"]"),
        ),
        file(
            DefKind::Bot,
            "scout",
            &bot_with("").replace("keeper", "scout"),
        ),
        file(
            DefKind::Trigger,
            "nightly",
            &TRIGGER.replace("\"keeper\"", "\"scout\""),
        ),
    ]));
    assert_one(
        &errors,
        "bots/keeper.toml: triggers[0]: trigger \"nightly\" starts bot \"scout\", not this one",
    );
}

#[test]
fn bot_triggers_are_bounded() {
    let names = (0..65)
        .map(|i| format!("\"t{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let errors = bot_errors(&bot_with(&format!("triggers = [{names}]")));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("triggers: lists 65 triggers; the limit is 64")),
        "{errors:?}"
    );
}

#[test]
fn bot_concurrency_is_bounded() {
    for bad in [0, 65] {
        assert_one(
            &bot_errors(&bot_with(&format!("[budget]\nmax_concurrent_runs = {bad}"))),
            "bots/keeper.toml: budget.max_concurrent_runs: ",
        );
    }
    assert!(bot_errors(&bot_with("[budget]\nmax_concurrent_runs = 64")).is_empty());
}

#[test]
fn bot_wall_time_must_be_a_sane_duration() {
    assert_one(
        &bot_errors(&bot_with("[budget]\nmax_wall_time = \"soon\"")),
        "bots/keeper.toml: budget.max_wall_time: ",
    );
    assert_one(
        &bot_errors(&bot_with("[budget]\nmax_wall_time = \"25h\"")),
        "bots/keeper.toml: budget.max_wall_time: the wall time \"25h\" is over the maximum of 24h",
    );
}

#[test]
fn bot_token_ceiling_must_be_positive() {
    assert_one(
        &bot_errors(&bot_with("[budget]\ndaily_token_ceiling = 0")),
        "bots/keeper.toml: budget.daily_token_ceiling: must be at least 1",
    );
    assert!(bot_errors(&bot_with("[budget]\ndaily_token_ceiling = 1000")).is_empty());
}

// ---- trigger rules ------------------------------------------------------------------------

#[test]
fn trigger_name_must_be_legal_and_equal_the_stem() {
    let errors = trigger_errors(&TRIGGER.replace("name = \"nightly\"", "name = \"Nightly\""));
    assert_one(
        &errors,
        "triggers/nightly.toml: name: \"Nightly\" is not a valid name",
    );
    let errors = trigger_errors(&TRIGGER.replace("name = \"nightly\"", "name = \"weekly\""));
    assert_one(
        &errors,
        "triggers/nightly.toml: name: \"weekly\" must equal the file name \"nightly\"",
    );
}

#[test]
fn trigger_target_bot_must_exist() {
    let errors = trigger_errors(&TRIGGER.replace("bot = \"keeper\"", "bot = \"ghost\""));
    assert_one(
        &errors,
        "triggers/nightly.toml: target.bot: unknown bot \"ghost\"",
    );
}

#[test]
fn trigger_target_bot_must_be_a_legal_name() {
    let errors = trigger_errors(&TRIGGER.replace("bot = \"keeper\"", "bot = \"Ghost\""));
    assert_one(
        &errors,
        "triggers/nightly.toml: target.bot: \"Ghost\" is not a valid name",
    );
}

#[test]
fn trigger_prompt_must_be_present_and_bounded() {
    let errors = trigger_errors(&TRIGGER.replace("Check for outdated dependencies.", " "));
    assert_one(
        &errors,
        "triggers/nightly.toml: target.prompt: must not be empty",
    );
    let long = "x".repeat(16 * 1024 + 1);
    let errors = trigger_errors(&TRIGGER.replace("Check for outdated dependencies.", &long));
    assert_one(
        &errors,
        "triggers/nightly.toml: target.prompt: is 16385 bytes",
    );
}

#[test]
fn trigger_at_needs_an_rfc3339_instant_with_an_offset() {
    assert!(
        trigger_errors(&trigger_with(
            "kind = \"at\"\nat = \"2026-12-01T09:00:00+07:00\""
        ))
        .is_empty()
    );
    assert!(
        trigger_errors(&trigger_with(
            "kind = \"at\"\nat = \"2026-12-01T02:00:00Z\""
        ))
        .is_empty()
    );
    for bad in ["2026-12-01T09:00:00", "tomorrow", "2026-13-01T09:00:00Z"] {
        let errors = trigger_errors(&trigger_with(&format!("kind = \"at\"\nat = \"{bad}\"")));
        assert_one(&errors, "triggers/nightly.toml: when.at: ");
    }
}

#[test]
fn trigger_after_needs_a_duration() {
    assert!(trigger_errors(&trigger_with("kind = \"after\"\nafter = \"30m\"")).is_empty());
    assert_one(
        &trigger_errors(&trigger_with("kind = \"after\"\nafter = \"later\"")),
        "triggers/nightly.toml: when.after: ",
    );
    assert_one(
        &trigger_errors(&trigger_with("kind = \"after\"\nafter = \"400d\"")),
        "triggers/nightly.toml: when.after: the delay \"400d\" is over the maximum",
    );
}

#[test]
fn trigger_every_has_a_floor() {
    assert!(trigger_errors(&trigger_with("kind = \"every\"\nevery = \"6h\"")).is_empty());
    assert!(trigger_errors(&trigger_with("kind = \"every\"\nevery = \"60s\"")).is_empty());
    assert_one(
        &trigger_errors(&trigger_with("kind = \"every\"\nevery = \"30s\"")),
        "triggers/nightly.toml: when.every: the interval \"30s\" is under the minimum of 1m",
    );
}

#[test]
fn trigger_cron_must_have_five_fields() {
    let when = |cron: &str| format!("kind = \"cron\"\ncron = \"{cron}\"\ntimezone = \"UTC\"");
    for cron in ["0 2 * *", "0 0 2 * * *", "0 0 2 * * * 2030"] {
        let errors = trigger_errors(&trigger_with(&when(cron)));
        assert_one(
            &errors,
            &format!("triggers/nightly.toml: when.cron: \"{cron}\" is not a cron expression"),
        );
        assert!(errors[0].contains("expected five fields"), "{errors:?}");
    }
}

#[test]
fn trigger_cron_fields_are_range_checked() {
    let when = |cron: &str| format!("kind = \"cron\"\ncron = \"{cron}\"\ntimezone = \"UTC\"");
    for cron in [
        "60 2 * * *",
        "0 24 * * *",
        "0 2 0 * *",
        "0 2 * 13 *",
        "0 2 * * 8",
        "0 2 * * funday",
        "5-1 2 * * *",
        "*/0 2 * * *",
    ] {
        let errors = trigger_errors(&trigger_with(&when(cron)));
        assert_one(&errors, "triggers/nightly.toml: when.cron: ");
        assert!(
            errors[0].contains("is not a cron expression"),
            "{cron}: {errors:?}"
        );
    }
}

#[test]
fn trigger_cron_must_match_some_date() {
    let when = "kind = \"cron\"\ncron = \"0 0 31 2 *\"\ntimezone = \"UTC\"";
    assert_one(
        &trigger_errors(&trigger_with(when)),
        "triggers/nightly.toml: when.cron: \"0 0 31 2 *\" never matches any date",
    );
}

#[test]
fn trigger_cron_accepts_the_usual_forms() {
    for cron in [
        "0 2 * * *",
        "*/15 9-17 * * mon-fri",
        "0 6 * jan,jul MON",
        "30 4 1,15 * 0",
        "0 0 * * 7",
        "0 */6 * * *",
        "10-50/10 * * * *",
        // croner's extensions: the last day of the month, the second Friday, a nickname.
        "0 2 L * *",
        "0 9 * * 5#2",
        "@daily",
    ] {
        let when = format!("kind = \"cron\"\ncron = \"{cron}\"\ntimezone = \"UTC\"");
        assert!(trigger_errors(&trigger_with(&when)).is_empty(), "{cron}");
    }
}

#[test]
fn trigger_cron_timezone_must_be_iana() {
    let when = |tz: &str| format!("kind = \"cron\"\ncron = \"0 2 * * *\"\ntimezone = \"{tz}\"");
    for good in [
        "UTC",
        "Asia/Ho_Chi_Minh",
        "America/New_York",
        "Europe/London",
    ] {
        assert!(
            trigger_errors(&trigger_with(&when(good))).is_empty(),
            "{good}"
        );
    }
    for bad in ["Asia/Saigonn", "GMT+7 maybe", "", "Mars/Olympus"] {
        assert_one(
            &trigger_errors(&trigger_with(&when(bad))),
            "triggers/nightly.toml: when.timezone: ",
        );
    }
}

#[test]
fn trigger_cron_needs_a_timezone() {
    let errors = trigger_errors(&trigger_with("kind = \"cron\"\ncron = \"0 2 * * *\""));
    assert_one(&errors, "triggers/nightly.toml: line");
    assert!(errors[0].contains("missing field `timezone`"), "{errors:?}");
}

#[test]
fn trigger_fs_needs_paths() {
    assert_one(
        &trigger_errors(&trigger_with("kind = \"fs\"\npaths = []")),
        "triggers/nightly.toml: when.paths: must list at least one path",
    );
}

#[test]
fn trigger_fs_paths_are_absolute_or_home_relative() {
    assert!(
        trigger_errors(&trigger_with(
            "kind = \"fs\"\npaths = [\"/srv/app/src/**\", \"~/notes/*.md\"]"
        ))
        .is_empty()
    );
    assert_one(
        &trigger_errors(&trigger_with("kind = \"fs\"\npaths = [\"src/**\"]")),
        "triggers/nightly.toml: when.paths[0]: \"src/**\" must be an absolute path",
    );
}

#[test]
fn trigger_fs_globs_must_be_well_formed() {
    assert_one(
        &trigger_errors(&trigger_with("kind = \"fs\"\npaths = [\"/srv/[abc\"]")),
        "triggers/nightly.toml: when.paths[0]: \"/srv/[abc\" is not a valid glob",
    );
}

#[test]
fn trigger_fs_paths_are_bounded() {
    let paths = (0..65)
        .map(|i| format!("\"/p{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let errors = trigger_errors(&trigger_with(&format!("kind = \"fs\"\npaths = [{paths}]")));
    assert_one(&errors, "triggers/nightly.toml: when.paths: lists 65 paths");
}

#[test]
fn trigger_fs_debounce_is_bounded() {
    let when = |d: &str| format!("kind = \"fs\"\npaths = [\"/srv\"]\ndebounce = \"{d}\"");
    assert!(trigger_errors(&trigger_with(&when("50ms"))).is_empty());
    assert!(trigger_errors(&trigger_with("kind = \"fs\"\npaths = [\"/srv\"]")).is_empty());
    assert_one(
        &trigger_errors(&trigger_with(&when("10ms"))),
        "triggers/nightly.toml: when.debounce: the debounce \"10ms\" is under the minimum of 50ms",
    );
    assert_one(
        &trigger_errors(&trigger_with(&when("2h"))),
        "triggers/nightly.toml: when.debounce: the debounce \"2h\" is over the maximum of 1h",
    );
}

#[test]
fn trigger_manual_has_no_fields_and_can_be_disabled() {
    let text = format!("enabled = false\n{}", trigger_with("kind = \"manual\""));
    let set = load(&[
        file(DefKind::Bot, "keeper", &bot_with("")),
        file(DefKind::Trigger, "nightly", &text),
    ])
    .unwrap();
    assert!(!set.triggers()["nightly"].enabled);
    assert_eq!(set.triggers()["nightly"].when.name(), "manual");
}

#[test]
fn reserved_trigger_kinds_are_rejected() {
    for when in [
        "kind = \"board\"\nscope = \"team\"\nkey = \"ci/*\"",
        "kind = \"message\"",
        "kind = \"git\"\nrepo = \"/r\"\nrefs = [\"refs/heads/main\"]",
        "kind = \"flow\"\nflow = \"nightly\"",
        "kind = \"webhook\"\npath = \"/hooks/github\"",
    ] {
        let errors = trigger_errors(&trigger_with(when));
        assert_one(&errors, "triggers/nightly.toml: when.kind: kind ");
        assert!(
            errors[0].contains("is reserved and not available yet"),
            "{errors:?}"
        );
    }
}

// ---- policy rules -------------------------------------------------------------------------

fn rule(body: &str) -> String {
    format!("[[rules]]\n{body}\n")
}

#[test]
fn an_empty_policy_is_valid_and_asks_by_default() {
    let set = load(&[file(DefKind::Policy, "policy", "")]).unwrap();
    let policy = set.policy().unwrap();
    assert_eq!(policy.default, PolicyEffect::Ask);
    assert!(policy.rules.is_empty());
}

#[test]
fn policy_globs_must_be_well_formed() {
    for field in ["tool", "command", "path"] {
        let text = rule(&format!(
            "effect = \"deny\"\nmatch = {{ {field} = \"[oops\" }}"
        ));
        assert_one(
            &policy_errors(&text),
            &format!("policy.toml: rules[0].match.{field}: \"[oops\" is not a valid glob"),
        );
    }
}

#[test]
fn policy_globs_are_bounded_and_not_empty() {
    let text = rule("effect = \"deny\"\nmatch = { tool = \"\" }");
    assert_one(
        &policy_errors(&text),
        "policy.toml: rules[0].match.tool: a glob must not be empty",
    );
    let text = rule(&format!(
        "effect = \"deny\"\nmatch = {{ tool = \"{}\" }}",
        "a".repeat(513)
    ));
    assert_one(
        &policy_errors(&text),
        "policy.toml: rules[0].match.tool: a glob is 513 bytes",
    );
}

#[test]
fn policy_command_needs_the_command_action() {
    let text = rule("effect = \"deny\"\nmatch = { action = \"edit\", command = \"ls*\" }");
    assert_one(
        &policy_errors(&text),
        "policy.toml: rules[0].match.command: only commands have",
    );
    let ok = rule("effect = \"deny\"\nmatch = { command = \"ls*\" }");
    assert!(policy_errors(&ok).is_empty());
}

#[test]
fn policy_scope_is_star_or_a_list_of_bots() {
    let star = rule("effect = \"ask\"\nmatch = { action = \"edit\", scope = \"*\" }");
    assert!(policy_errors(&star).is_empty());
    let list = rule("effect = \"ask\"\nmatch = { scope = [\"keeper\", \"scout\"] }");
    assert!(policy_errors(&list).is_empty());

    let bad = rule("effect = \"ask\"\nmatch = { scope = \"keeper\" }");
    let errors = policy_errors(&bad);
    assert_one(&errors, "policy.toml: line");
    assert!(
        errors[0].contains("must be \"*\" or a list of bot names"),
        "{errors:?}"
    );

    let empty = rule("effect = \"ask\"\nmatch = { scope = [] }");
    assert_one(
        &policy_errors(&empty),
        "policy.toml: rules[0].match.scope: must list at least one bot",
    );

    let illegal = rule("effect = \"ask\"\nmatch = { scope = [\"Keeper\"] }");
    assert_one(
        &policy_errors(&illegal),
        "policy.toml: rules[0].match.scope[0]: \"Keeper\" is not a valid name",
    );

    let twice = rule("effect = \"ask\"\nmatch = { scope = [\"a\", \"a\"] }");
    assert_one(
        &policy_errors(&twice),
        "policy.toml: rules[0].match.scope[1]: duplicate bot \"a\"",
    );
}

#[test]
fn policy_scope_bots_need_not_exist() {
    let text = rule("effect = \"ask\"\nmatch = { scope = [\"ghost\"] }");
    assert!(policy_errors(&text).is_empty());
}

#[test]
fn policy_scope_is_bounded() {
    let bots = (0..257)
        .map(|i| format!("\"b{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let text = rule(&format!("effect = \"ask\"\nmatch = {{ scope = [{bots}] }}"));
    assert_one(
        &policy_errors(&text),
        "policy.toml: rules[0].match.scope: lists 257 bots",
    );
}

#[test]
fn policy_allow_must_not_match_everything() {
    assert_one(
        &policy_errors(&rule("effect = \"allow\"")),
        "policy.toml: rules[0].match: an `allow` rule that matches everything",
    );
    assert_one(
        &policy_errors(&rule("effect = \"allow\"\nmatch = { scope = \"*\" }")),
        "policy.toml: rules[0].match: an `allow` rule that matches everything",
    );
    assert!(policy_errors(&rule("effect = \"allow\"\nmatch = { action = \"read\" }")).is_empty());
    // A catch-all that asks or denies is a legitimate way to end a policy.
    assert!(policy_errors(&rule("effect = \"deny\"")).is_empty());
}

#[test]
fn policy_notes_are_bounded() {
    let text = rule(&format!("note = \"{}\"\neffect = \"ask\"", "n".repeat(201)));
    assert_one(
        &policy_errors(&text),
        "policy.toml: rules[0].note: is longer than 200 characters",
    );
}

#[test]
fn policy_rules_are_bounded() {
    let one = rule("effect = \"deny\"\nmatch = { tool = \"x\" }");
    let errors = policy_errors(&one.repeat(1001));
    assert_one(
        &errors,
        "policy.toml: rules: holds 1001 rules; the limit is 1000",
    );
}

#[test]
fn policy_approval_ttl_is_a_bounded_duration() {
    let set = load(&[file(DefKind::Policy, "policy", "approval_ttl = \"10m\"")]).unwrap();
    assert_eq!(
        set.policy().unwrap().approval_ttl(),
        Some(std::time::Duration::from_secs(600))
    );
    assert_eq!(PolicyDef::default().approval_ttl(), None);
    for (text, message) in [
        ("0s", "is zero"),
        ("1ms", "is under the minimum of 1s"),
        ("8d", "is over the maximum of 168h"),
        ("soon", "is not a duration"),
    ] {
        let errors = policy_errors(&format!("approval_ttl = \"{text}\""));
        assert_one(&errors, "policy.toml: approval_ttl: ");
        assert!(errors[0].contains(message), "{text}: {errors:?}");
    }
}

#[test]
fn an_unset_approval_ttl_leaves_the_content_hash_alone() {
    let before = load(&[file(DefKind::Policy, "policy", "default = \"ask\"")]).unwrap();
    let hash = before.policy().unwrap().hash().to_owned();
    // The canonical form of a policy with no TTL has no `approval_ttl` key at all, so every
    // version stored before the field existed keeps its hash.
    assert!(
        !before
            .policy()
            .unwrap()
            .canonical()
            .contains("approval_ttl")
    );
    let with = load(&[file(DefKind::Policy, "policy", "approval_ttl = \"5m\"")]).unwrap();
    assert_ne!(with.policy().unwrap().hash(), hash);
}

#[test]
fn policy_unknown_keys_are_refused() {
    let errors = policy_errors("mode = \"strict\"");
    assert_one(
        &errors,
        "policy.toml: line 1, column 1: unknown field `mode`",
    );
    let errors = policy_errors(&rule("effect = \"ask\"\nmatch = { glob = \"x\" }"));
    assert_one(&errors, "policy.toml: line");
    assert!(errors[0].contains("unknown field `glob`"), "{errors:?}");
}

#[test]
fn policy_rule_order_is_kept() {
    let set = load(&[file(DefKind::Policy, "policy", defaults::POLICY)]).unwrap();
    let notes: Vec<_> = set
        .policy()
        .unwrap()
        .rules
        .iter()
        .map(|r| r.note.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(notes[0], "recursive force delete");
    assert_eq!(
        notes.last().copied(),
        Some("cargo in the example bot's worktree")
    );
}

// ---- the shipped defaults -----------------------------------------------------------------

#[test]
fn the_shipped_defaults_validate_with_only_the_builtin_executors() {
    let set = load_set(
        &[
            file(DefKind::Bot, "example", defaults::EXAMPLE_BOT),
            file(DefKind::Policy, "policy", defaults::POLICY),
        ],
        &ValidationContext::with_builtin([]),
    )
    .unwrap();
    assert_eq!(set.bots().keys().collect::<Vec<_>>(), ["example"]);
    let policy = set.policy().unwrap();
    assert_eq!(policy.default, PolicyEffect::Ask);
    // Dangerous commands are refused before anything can allow them.
    let first_allow = policy
        .rules
        .iter()
        .position(|r| r.effect == PolicyEffect::Allow);
    let last_deny = policy
        .rules
        .iter()
        .rposition(|r| r.effect == PolicyEffect::Deny);
    assert!(last_deny < first_allow);
}

#[test]
fn the_default_policy_covers_the_prototype_table() {
    let set = load(&[file(DefKind::Policy, "policy", defaults::POLICY)]).unwrap();
    let rules = &set.policy().unwrap().rules;
    let command = |r: &PolicyRule| r.matches.command.clone().unwrap_or_default();
    let by = |effect| {
        rules
            .iter()
            .filter(move |r| r.effect == effect)
            .collect::<Vec<_>>()
    };
    assert!(
        by(PolicyEffect::Ask)
            .iter()
            .any(|r| command(r) == "git push *")
    );
    assert!(
        by(PolicyEffect::Ask)
            .iter()
            .any(|r| r.matches.action == Some(ActionClass::Network))
    );
    assert!(
        by(PolicyEffect::Allow)
            .iter()
            .any(|r| command(r) == "cargo *")
    );
    let denied: Vec<_> = by(PolicyEffect::Deny).iter().map(|r| command(r)).collect();
    for needle in ["rm -rf *", "git reset --hard*"] {
        assert!(denied.iter().any(|c| c == needle), "{needle} in {denied:?}");
    }
    assert!(denied.iter().any(|c| c.contains("--force")));
    assert!(
        by(PolicyEffect::Deny)
            .iter()
            .any(|r| r.matches.path.as_deref() == Some("/**"))
    );
}

// ---- hashing ------------------------------------------------------------------------------

fn hash_of_bot(text: &str) -> String {
    let set = load(&[file(DefKind::Bot, "keeper", text)]).unwrap();
    set.bot("keeper").unwrap().hash().to_owned()
}

#[test]
fn whitespace_comments_and_key_order_do_not_change_the_hash() {
    let a = hash_of_bot(&bot_with(""));
    let reformatted = "# a comment\n\n\ninstructions   =   \"x\"   # trailing\nexecutors=[ \"codex\" ]\n\nname = \"keeper\"\n";
    assert_eq!(hash_of_bot(reformatted), a);
}

#[test]
fn spelling_out_a_default_does_not_change_the_hash() {
    let a = hash_of_bot(&bot_with(""));
    let explicit = bot_with(
        "access = \"supervised\"\nsandbox = \"workspace_write\"\ntriggers = []\n[budget]\nmax_concurrent_runs = 1\nmax_wall_time = \"45m\"",
    );
    assert_eq!(hash_of_bot(&explicit), a);
}

#[test]
fn a_change_of_meaning_changes_the_hash() {
    let a = hash_of_bot(&bot_with(""));
    assert_ne!(hash_of_bot(&bot_with("access = \"full_access\"")), a);
    assert_ne!(hash_of_bot(&bot_with("").replace("\"x\"", "\"y\"")), a);
    assert_ne!(
        hash_of_bot(&bot_with("").replace("[\"codex\"]", "[\"codex\", \"mock\"]")),
        a
    );
}

#[test]
fn executor_order_is_part_of_the_hash() {
    let one = hash_of_bot(&bot_with("").replace("[\"codex\"]", "[\"codex\", \"mock\"]"));
    let two = hash_of_bot(&bot_with("").replace("[\"codex\"]", "[\"mock\", \"codex\"]"));
    assert_ne!(one, two);
}

#[test]
fn canonical_json_sorts_keys_at_every_depth_and_has_no_whitespace() {
    let value =
        json!({ "b": [ { "z": 1, "a": "x y" }, 2 ], "a": null, "c": { "y": true, "x": 1.5 } });
    assert_eq!(
        canonical_json(&value),
        r#"{"a":null,"b":[{"a":"x y","z":1},2],"c":{"x":1.5,"y":true}}"#
    );
}

#[test]
fn the_hash_is_sha256_of_the_canonical_text() {
    // sha256("{}"), a published value.
    assert_eq!(
        content_hash(&json!({})),
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

#[test]
fn the_body_is_the_canonical_form_that_is_hashed() {
    let set = load(&[file(DefKind::Bot, "keeper", &bot_with(""))]).unwrap();
    let bot = set.bot("keeper").unwrap();
    assert_eq!(bot.canonical(), canonical_json(bot.body()));
    assert_eq!(bot.hash(), content_hash(bot.body()));
    assert!(
        bot.canonical().starts_with("{\"access\":\"supervised\""),
        "{}",
        bot.canonical()
    );
}

// ---- stored bodies and `def.validate` -----------------------------------------------------

#[test]
fn a_set_survives_a_round_trip_through_its_bodies() {
    let set = load(&[
        file(DefKind::Bot, "keeper", BOT),
        file(DefKind::Trigger, "nightly", TRIGGER),
        file(DefKind::Policy, "policy", defaults::POLICY),
    ])
    .unwrap();
    let bodies: Vec<_> = set
        .entries()
        .iter()
        .map(|e| {
            (
                e.kind,
                e.name.to_owned(),
                serde_json::from_str(e.canonical).unwrap(),
            )
        })
        .collect();
    assert_eq!(load_bodies(&bodies, &ctx()).unwrap(), set);
}

#[test]
fn a_stored_body_that_no_longer_validates_is_reported() {
    let set = load(&[file(DefKind::Bot, "keeper", &bot_with(""))]).unwrap();
    let bodies: Vec<_> = set
        .entries()
        .iter()
        .map(|e| {
            (
                e.kind,
                e.name.to_owned(),
                serde_json::from_str(e.canonical).unwrap(),
            )
        })
        .collect();
    let narrower = ValidationContext::new(["mock".parse().unwrap()]);
    let errors = load_bodies(&bodies, &narrower).unwrap_err();
    assert_eq!(
        errors[0].to_string(),
        "bots/keeper.toml: executors[0]: unknown executor \"codex\""
    );
}

#[test]
fn validate_body_checks_a_body_against_the_current_set() {
    let current = load(&[
        file(
            DefKind::Bot,
            "keeper",
            &bot_with("triggers = [\"nightly\"]"),
        ),
        file(DefKind::Trigger, "nightly", TRIGGER),
    ])
    .unwrap();

    let mut body = serde_json::to_value(current.triggers()["nightly"].def()).unwrap();
    let (errors, hash) = validate_body(&current, DefKind::Trigger, "nightly", &body, &ctx());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(hash.as_deref(), Some(current.triggers()["nightly"].hash()));

    body["target"]["bot"] = json!("ghost");
    let (errors, hash) = validate_body(&current, DefKind::Trigger, "nightly", &body, &ctx());
    assert_eq!(hash, None);
    assert_eq!(
        errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["triggers/nightly.toml: target.bot: unknown bot \"ghost\""]
    );
}

#[test]
fn validate_body_reports_a_shape_error_and_keeps_to_its_own_file() {
    let current = load(&[file(DefKind::Bot, "keeper", &bot_with(""))]).unwrap();
    let (errors, _) = validate_body(
        &current,
        DefKind::Bot,
        "scout",
        &json!({ "nom": "x" }),
        &ctx(),
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].file, "bots/scout.toml");
    assert!(
        errors[0].message.contains("unknown field `nom`"),
        "{errors:?}"
    );
}

#[test]
fn validate_body_does_not_report_other_files_problems() {
    // The current bot lists a trigger. Validating a *new* bot must not complain about that.
    let current = load(&[file(DefKind::Bot, "keeper", &bot_with(""))]).unwrap();
    let body = json!({ "name": "scout", "instructions": "x", "executors": ["mock"] });
    let (errors, hash) = validate_body(&current, DefKind::Bot, "scout", &body, &ctx());
    assert!(errors.is_empty(), "{errors:?}");
    assert!(hash.is_some());
}

#[test]
fn validate_body_sees_references_from_other_definitions() {
    // `nightly` targets `keeper`. Replacing keeper's body keeps that reference intact.
    let current = load(&[
        file(
            DefKind::Bot,
            "keeper",
            &bot_with("triggers = [\"nightly\"]"),
        ),
        file(DefKind::Trigger, "nightly", TRIGGER),
    ])
    .unwrap();
    let body = json!({ "name": "keeper", "instructions": "x", "executors": ["mock"], "triggers": ["nightly"] });
    let (errors, _) = validate_body(&current, DefKind::Bot, "keeper", &body, &ctx());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn validate_body_of_the_policy_must_be_named_policy() {
    let (errors, _) = validate_body(
        &DefSet::default(),
        DefKind::Policy,
        "rules",
        &json!({}),
        &ctx(),
    );
    assert_eq!(
        errors[0].to_string(),
        "policy.toml: the policy file must be named policy.toml"
    );
    let (errors, hash) = validate_body(
        &DefSet::default(),
        DefKind::Policy,
        "policy",
        &json!({}),
        &ctx(),
    );
    assert!(errors.is_empty());
    assert!(hash.is_some());
}

// ---- smaller pieces -----------------------------------------------------------------------

#[test]
fn durations_parse_every_unit_and_combination() {
    use std::time::Duration;
    for (text, want) in [
        ("500ms", Duration::from_millis(500)),
        ("45s", Duration::from_secs(45)),
        ("45m", Duration::from_secs(45 * 60)),
        ("6h", Duration::from_secs(6 * 3600)),
        ("2d", Duration::from_secs(2 * 86_400)),
        ("1h30m", Duration::from_secs(5400)),
        ("1m30s", Duration::from_secs(90)),
    ] {
        assert_eq!(parse_duration(text).unwrap(), want, "{text}");
    }
}

#[test]
fn durations_reject_nonsense() {
    for bad in [
        "",
        "0s",
        "0m0s",
        "m",
        "5",
        "5x",
        "5 m",
        "-5m",
        "1.5h",
        "99999999999999999999d",
        "18446744073709551615d",
    ] {
        assert!(parse_duration(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn kinds_know_their_paths() {
    assert_eq!(DefKind::Bot.path_for("keeper"), "bots/keeper.toml");
    assert_eq!(
        DefKind::Trigger.path_for("nightly"),
        "triggers/nightly.toml"
    );
    assert_eq!(DefKind::Policy.path_for("policy"), "policy.toml");
    for (path, want) in [
        ("bots/keeper.toml", Some((DefKind::Bot, "keeper"))),
        ("triggers/a-b_c.toml", Some((DefKind::Trigger, "a-b_c"))),
        ("policy.toml", Some((DefKind::Policy, "policy"))),
        ("bots/keeper.json", None),
        ("bots/.toml", None),
        ("bots/sub/x.toml", None),
        ("flows/x.toml", None),
        ("README.md", None),
        ("keeper.toml", None),
    ] {
        let got = DefKind::parse_path(path);
        assert_eq!(got, want.map(|(k, n)| (k, n.to_owned())), "{path}");
    }
}

#[test]
fn kinds_parse_and_print() {
    for kind in DefKind::ALL {
        assert_eq!(kind.to_string().parse::<DefKind>().unwrap(), kind);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
    }
    assert!("flow".parse::<DefKind>().is_err());
}

#[test]
fn error_display_has_the_documented_shape() {
    let e = DefError::new("bots/keeper.toml", "executors[2]", "unknown executor \"x\"");
    assert_eq!(
        e.to_string(),
        "bots/keeper.toml: executors[2]: unknown executor \"x\""
    );
    let whole = DefError::new("policy.toml", "", "boom");
    assert_eq!(whole.to_string(), "policy.toml: boom");
    assert_eq!(
        serde_json::to_value(&whole).unwrap(),
        json!({"file": "policy.toml", "message": "boom"})
    );
}

#[test]
fn builtin_executors_are_valid_ids() {
    for name in BUILTIN_EXECUTORS {
        assert!(name.parse::<ExecutorId>().is_ok(), "{name}");
    }
}
