//! `cargo xtask conformance-live`: the I/O shell around [`crate::conformance`].
//!
//! It runs the live conformance suite (`crates/reno-harness/tests/conformance_live.rs`, which
//! spends real model runs) for the CLIs that are installed and signed in, writes
//! `<data dir>/conformance/YYYY-MM-DD.{json,md}`, compares with the latest earlier report, prints
//! the Markdown, and exits 1 on a regression (see [`crate::conformance`] for what that is).
//!
//! ```text
//! cargo xtask conformance-live                      # both CLIs, whichever are usable
//! cargo xtask conformance-live --clis codex         # one, which must be usable
//! cargo xtask conformance-live --print-plist        # the nightly LaunchAgent, to stdout only
//! cargo xtask conformance-live --from-json run.json # report a run that was already made
//! ```
//!
//! `--print-plist` only prints. Nothing here installs a LaunchAgent, touches `launchctl`, or
//! writes outside `<data dir>/conformance`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use reno_client::launcher::ScheduledJob;
use serde::Serialize;

use crate::conformance::{
    Bump, Comparison, Report, civil_date, compare, markdown, previous_date, report_date,
};

pub(crate) const USAGE: &str = "usage: cargo xtask conformance-live [--clis claude-code,codex|auto] [--data-dir DIR] [--date YYYY-MM-DD] [--from-json FILE]\n       cargo xtask conformance-live --print-plist [--hour H] [--minute M] [--data-dir DIR]";

/// The nightly job's label, and so its plist's file name.
const LABEL: &str = "dev.reno.conformance-live";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) clis: String,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) date: Option<String>,
    pub(crate) from_json: Option<PathBuf>,
    pub(crate) print_plist: bool,
    pub(crate) hour: u8,
    pub(crate) minute: u8,
}

pub(crate) fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        clis: "auto".into(),
        data_dir: None,
        date: None,
        from_json: None,
        print_plist: false,
        hour: 3,
        minute: 17,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} wants {what}"))
        };
        match arg.as_str() {
            "--clis" => {
                let clis = value("claude-code, codex, both (comma separated) or auto")?;
                let known = clis
                    .split(',')
                    .all(|c| matches!(c.trim(), "claude-code" | "codex"));
                if clis != "auto" && !known {
                    return Err(format!(
                        "--clis wants claude-code, codex, both or auto, got {clis:?}"
                    ));
                }
                options.clis = clis;
            }
            "--data-dir" => options.data_dir = Some(PathBuf::from(value("a directory")?)),
            "--date" => {
                let date = value("YYYY-MM-DD")?;
                if report_date(&format!("{date}.json")).is_none() {
                    return Err(format!("--date wants YYYY-MM-DD, got {date:?}"));
                }
                options.date = Some(date);
            }
            "--from-json" => options.from_json = Some(PathBuf::from(value("a file")?)),
            "--print-plist" => options.print_plist = true,
            "--hour" => {
                options.hour = value("0-23")?
                    .parse()
                    .ok()
                    .filter(|h| *h < 24)
                    .ok_or("--hour wants 0-23")?;
            }
            "--minute" => {
                options.minute = value("0-59")?
                    .parse()
                    .ok()
                    .filter(|m| *m < 60)
                    .ok_or("--minute wants 0-59")?;
            }
            other => return Err(format!("unknown conformance-live option {other}")),
        }
    }
    Ok(options)
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("xtask: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let outcome = if options.print_plist {
        print_plist(&options)
    } else {
        report(&options).map(|clean| {
            if clean {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        })
    };
    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::from(2)
        }
    }
}

fn data_dir(options: &Options) -> Result<PathBuf, String> {
    options
        .data_dir
        .clone()
        .or_else(reno_client::paths::default_data_dir)
        .ok_or_else(|| {
            "cannot work out the data directory: set HOME, RENO_DATA_DIR or --data-dir".to_owned()
        })
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn print_plist(options: &Options) -> Result<ExitCode, String> {
    let data = data_dir(options)?;
    let cargo = std::env::var("CARGO").map_err(|_| {
        "CARGO is not set: run this as `cargo xtask conformance-live --print-plist`".to_owned()
    })?;
    let mut env = Vec::new();
    for key in ["PATH", "HOME"] {
        match std::env::var(key) {
            Ok(value) => env.push((key.to_owned(), value)),
            Err(_) => return Err(format!("{key} is not set; the job needs it")),
        }
    }
    if std::env::var_os(reno_client::paths::DATA_DIR_ENV).is_some() {
        env.push((
            reno_client::paths::DATA_DIR_ENV.to_owned(),
            data.display().to_string(),
        ));
    }
    let dir = data.join("conformance");
    let job = ScheduledJob::new(
        LABEL,
        vec![cargo, "xtask".into(), "conformance-live".into()],
        workspace_root(),
        env,
        (options.hour, options.minute),
        dir.join("nightly.out"),
        dir.join("nightly.err"),
    )
    .map_err(|e| e.to_string())?;
    print!("{}", job.plist());
    eprintln!(
        "Not installed. To install it yourself:\n  mkdir -p '{}'\n  save the plist above as ~/Library/LaunchAgents/{}\n  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{}\nIt runs the live suite once a day at {:02}:{:02} local time and costs model runs each time.",
        dir.display(),
        job.file_name(),
        job.file_name(),
        options.hour,
        options.minute
    );
    Ok(ExitCode::SUCCESS)
}

/// What the dated JSON file holds: the report and what the comparison found.
#[derive(Serialize)]
struct Written<'a> {
    #[serde(flatten)]
    report: &'a Report,
    comparison: WrittenComparison<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WrittenComparison<'a> {
    previous: Option<&'a str>,
    regressions: &'a [String],
    notes: &'a [String],
    version_bumps: Vec<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Runs the live test and returns the report it wrote.
fn run_live(clis: &str, work: &Path) -> Result<Report, String> {
    let out = work.join("live-report.json");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    eprintln!("running the live suite for {clis}: this makes real model runs");
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args([
            "test",
            "-p",
            "reno-harness",
            "--test",
            "conformance_live",
            "--",
            "--ignored",
            "--nocapture",
        ])
        .env("RENO_CONFORMANCE_LIVE", clis)
        .env("RENO_CONFORMANCE_REPORT", &out)
        .status()
        .map_err(|e| format!("cannot run cargo test: {e}"))?;
    read_report(&out).map_err(|e| format!("the live test wrote no report ({status}): {e}"))
}

fn read_report(path: &Path) -> Result<Report, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs and reports; `true` when there is no regression.
fn report(options: &Options) -> Result<bool, String> {
    let dir = data_dir(options)?.join("conformance");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut now = match &options.from_json {
        Some(path) => read_report(path)?,
        None => {
            let work = tempdir_in(&dir)?;
            let report = run_live(&options.clis, &work);
            let _ = fs::remove_dir_all(&work);
            report?
        }
    };
    let generated = now_ms();
    now.schema = now.schema.max(1);
    now.generated_at_ms = generated;
    now.date = options
        .date
        .clone()
        .unwrap_or_else(|| civil_date(generated));

    let names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    let prev_date = previous_date(names.iter().map(String::as_str), &now.date);
    let prev = match &prev_date {
        Some(date) => match read_report(&dir.join(format!("{date}.json"))) {
            Ok(report) => Some(report),
            Err(error) => {
                eprintln!(
                    "xtask: the previous report is unreadable, comparing with nothing: {error}"
                );
                None
            }
        },
        None => None,
    };
    let cmp = compare(prev.as_ref(), &now);
    let text = markdown(&now, prev_date.as_deref(), &cmp);
    write_files(&dir, &now, prev_date.as_deref(), &cmp, &text)?;
    print!("{text}");
    println!("wrote {}/{}.{{json,md}}", dir.display(), now.date);
    Ok(cmp.regressions.is_empty())
}

fn write_files(
    dir: &Path,
    now: &Report,
    previous: Option<&str>,
    cmp: &Comparison,
    text: &str,
) -> Result<(), String> {
    let written = Written {
        report: now,
        comparison: WrittenComparison {
            previous,
            regressions: &cmp.regressions,
            notes: &cmp.notes,
            version_bumps: cmp.bumps.iter().map(Bump::line).collect(),
        },
    };
    let json = serde_json::to_vec_pretty(&written).map_err(|e| e.to_string())?;
    for (extension, bytes) in [("json", json.as_slice()), ("md", text.as_bytes())] {
        let path = dir.join(format!("{}.{extension}", now.date));
        fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// A fresh scratch directory under `parent`.
fn tempdir_in(parent: &Path) -> Result<PathBuf, String> {
    let path = parent.join(format!(".run-{}", std::process::id()));
    fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_defaults_run_both_clis_at_a_quiet_hour() {
        let options = parse_args(&[]).unwrap();
        assert_eq!(options.clis, "auto");
        assert_eq!((options.hour, options.minute), (3, 17));
        assert!(!options.print_plist && options.from_json.is_none() && options.date.is_none());
    }

    #[test]
    fn options_are_checked_where_they_are_read() {
        let options = parse_args(&args(&[
            "--clis",
            "codex",
            "--date",
            "2026-11-17",
            "--hour",
            "4",
            "--minute",
            "5",
            "--from-json",
            "r.json",
            "--data-dir",
            "/d",
            "--print-plist",
        ]))
        .unwrap();
        assert_eq!(options.clis, "codex");
        assert_eq!(options.date.as_deref(), Some("2026-11-17"));
        assert_eq!((options.hour, options.minute), (4, 5));
        assert_eq!(options.from_json, Some(PathBuf::from("r.json")));
        assert_eq!(options.data_dir, Some(PathBuf::from("/d")));
        assert!(options.print_plist);
        for bad in [
            &["--clis", "grok"][..],
            &["--date", "yesterday"],
            &["--hour", "24"],
            &["--minute", "60"],
            &["--hour"],
            &["--install"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
        assert!(parse_args(&args(&["--clis", "claude-code,codex"])).is_ok());
    }

    #[test]
    fn a_report_is_written_dated_compared_and_the_exit_code_follows_the_regressions() {
        let dir = std::env::temp_dir().join(format!("reno-xtask-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let run = |json: &str, date: &str| {
            let input = dir.join(format!("input-{date}.json"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(&input, json).unwrap();
            report(&Options {
                clis: "auto".into(),
                data_dir: Some(dir.clone()),
                date: Some(date.into()),
                from_json: Some(input),
                print_plist: false,
                hour: 3,
                minute: 17,
            })
            .unwrap()
        };
        let green = r#"{"schema":1,"targets":[{"target":"codex","cliVersion":"1.2.3","cliVerified":"1.2.3","cases":[{"case":"probe","status":"pass"},{"case":"steer","status":"pass"}]}]}"#;
        let red = r#"{"schema":1,"targets":[{"target":"codex","cliVersion":"1.2.3","cliVerified":"1.2.3","cases":[{"case":"probe","status":"pass"},{"case":"steer","status":"fail","reason":"no text"}]}]}"#;
        assert!(run(green, "2026-11-16"));
        assert!(!run(red, "2026-11-17"));
        let conformance = dir.join("conformance");
        for name in [
            "2026-11-16.json",
            "2026-11-16.md",
            "2026-11-17.json",
            "2026-11-17.md",
        ] {
            assert!(conformance.join(name).exists(), "{name}");
        }
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(conformance.join("2026-11-17.json")).unwrap())
                .unwrap();
        assert_eq!(written["date"], "2026-11-17");
        assert_eq!(written["comparison"]["previous"], "2026-11-16");
        assert_eq!(
            written["comparison"]["regressions"][0],
            "codex/steer: fails: no text"
        );
        let md = fs::read_to_string(conformance.join("2026-11-17.md")).unwrap();
        assert!(md.contains("Compared with: 2026-11-16."), "{md}");
        // A re-run on the same day replaces that day's files and still compares with the 16th.
        assert!(run(green, "2026-11-17"));
        let _ = fs::remove_dir_all(&dir);
    }
}
