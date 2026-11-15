//! `reno defs list`, `reno defs show` and `reno defs check`.
//!
//! `list` and `show` ask the daemon: they show what is in force. `check` does not: it reads the
//! definition files itself and validates them with the same pure code the daemon uses
//! (`reno_proto::defs`), so it works with no daemon running, before an edit is saved to a
//! repository, or in CI. The one thing it cannot know is which executors a daemon has; it
//! accepts the ones Reno builds, and `--executor` adds others.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, bail};
use reno_proto::defs::{
    DefError, DefFile, DefKind, DefsSnapshot, MAX_FILE_BYTES, MAX_FILES_PER_KIND, POLICY_NAME,
    ValidationContext, load_set,
};
use serde::Serialize;

use crate::cli::{DefsCheck, DefsCommand, DefsList, DefsShow};
use crate::env::{Env, print_json, print_line};

pub(crate) async fn run(env: &Env, command: DefsCommand) -> anyhow::Result<ExitCode> {
    match command {
        DefsCommand::List(args) => list(env, args).await,
        DefsCommand::Show(args) => show(env, args).await,
        DefsCommand::Check(args) => check(env, args),
    }
}

async fn list(env: &Env, args: DefsList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let snapshot = client
        .list_defs(args.kind)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&snapshot)?;
    } else {
        print_snapshot(&snapshot)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_snapshot(snapshot: &DefsSnapshot) -> anyhow::Result<()> {
    if snapshot.defs.is_empty() {
        print_line("no definitions")?;
    }
    for def in &snapshot.defs {
        print_line(format_args!(
            "{:<8} {:<24} v{:<4} {}",
            def.kind.as_str(),
            def.name,
            def.version,
            def.hash.get(..12).unwrap_or(&def.hash)
        ))?;
    }
    if !snapshot.errors.is_empty() {
        print_line(format_args!(
            "\nthe files on disk have {} error(s); the definitions above are the last good set:",
            snapshot.errors.len()
        ))?;
        for error in &snapshot.errors {
            print_line(format_args!("  {error}"))?;
        }
    }
    Ok(())
}

async fn show(env: &Env, args: DefsShow) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let record = client
        .get_def(args.kind, &args.name, args.version)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&record)?;
        return Ok(ExitCode::SUCCESS);
    }
    let latest = if record.latest { ", latest" } else { "" };
    print_line(format_args!(
        "# {} {} v{}{latest}  {}",
        record.kind, record.name, record.version, record.hash
    ))?;
    // The body is the file's own shape, so TOML shows it as it is written.
    let text = toml::to_string_pretty(&record.body)
        .or_else(|_| serde_json::to_string_pretty(&record.body))
        .context("cannot print the definition")?;
    print_line(text.trim_end())?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Checked<'a> {
    ok: bool,
    dir: &'a Path,
    definitions: usize,
    errors: &'a [DefError],
}

fn check(env: &Env, args: DefsCheck) -> anyhow::Result<ExitCode> {
    let dir = match args.dir.or_else(reno_client::default_defs_dir) {
        Some(dir) => dir,
        None => bail!("cannot find the definitions directory: set --dir, RENO_DEFS_DIR, or HOME"),
    };
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let context = ValidationContext::with_builtin(args.executors);
    let (files, mut errors) = read_files(&dir)?;
    let definitions = match load_set(&files, &context) {
        Ok(set) => set.len(),
        Err(found) => {
            errors.extend(found);
            0
        }
    };
    let ok = errors.is_empty();
    if env.json {
        print_json(&Checked {
            ok,
            dir: &dir,
            definitions,
            errors: &errors,
        })?;
    } else if ok {
        print_line(format_args!(
            "ok: {definitions} definition(s) in {}",
            dir.display()
        ))?;
    } else {
        for error in &errors {
            print_line(error)?;
        }
        print_line(format_args!(
            "{} error(s) in {}",
            errors.len(),
            dir.display()
        ))?;
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Reads `bots/*.toml`, `triggers/*.toml` and `policy.toml` under `dir`, as the daemon does:
/// no deeper, other files ignored, each file limited to [`MAX_FILE_BYTES`] and each directory to
/// [`MAX_FILES_PER_KIND`] files. What cannot be read is returned as an error to report.
fn read_files(dir: &Path) -> anyhow::Result<(Vec<DefFile>, Vec<DefError>)> {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    for (sub, kind) in [("bots", DefKind::Bot), ("triggers", DefKind::Trigger)] {
        let path = dir.join(sub);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot list {}", path.display())),
        };
        let mut count = 0usize;
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "toml")
                || !fs::metadata(&path).is_ok_and(|m| m.is_file())
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                problems.push(DefError::new(
                    format!(
                        "{sub}/{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    "",
                    "the file name is not valid UTF-8",
                ));
                continue;
            };
            count += 1;
            if count > MAX_FILES_PER_KIND {
                problems.push(DefError::new(
                    format!("{sub}/"),
                    "",
                    format!("holds more than {MAX_FILES_PER_KIND} files; the rest were not read"),
                ));
                break;
            }
            read_one(&path, kind, stem, &mut files, &mut problems);
        }
    }
    let policy = dir.join("policy.toml");
    if fs::metadata(&policy).is_ok_and(|m| m.is_file()) {
        read_one(
            &policy,
            DefKind::Policy,
            POLICY_NAME,
            &mut files,
            &mut problems,
        );
    }
    files.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok((files, problems))
}

pub(crate) fn read_one(
    path: &Path,
    kind: DefKind,
    name: &str,
    files: &mut Vec<DefFile>,
    problems: &mut Vec<DefError>,
) {
    let shown = kind.path_for(name);
    let limit = u64::try_from(MAX_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    if let Err(e) = fs::File::open(path).and_then(|f| f.take(limit).read_to_end(&mut bytes)) {
        problems.push(DefError::new(
            shown,
            "",
            format!("cannot read the file: {e}"),
        ));
        return;
    }
    // An oversized file is cut inside the limit's last byte; lossy decoding never shortens it, so
    // the loader reports the size and not the encoding.
    let text = if bytes.len() > MAX_FILE_BYTES {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        String::from_utf8(bytes)
    };
    match text {
        Ok(text) => files.push(DefFile {
            kind,
            name: name.to_owned(),
            text,
        }),
        Err(_) => problems.push(DefError::new(shown, "", "the file is not valid UTF-8")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, rel: &str, bytes: &[u8]) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create the directory");
        fs::write(path, bytes).expect("write the file");
    }

    #[test]
    fn only_definition_files_are_read_and_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        for rel in [
            "bots/zed.toml",
            "bots/alpha.toml",
            "triggers/nightly.toml",
            "policy.toml",
            "bots/notes.md",
            "bots/deep/x.toml",
            "other.toml",
        ] {
            put(dir.path(), rel, b"x");
        }
        let (files, problems) = read_files(dir.path()).expect("read");
        assert!(problems.is_empty());
        let names: Vec<_> = files.iter().map(|f| (f.kind, f.name.as_str())).collect();
        assert_eq!(
            names,
            [
                (DefKind::Bot, "alpha"),
                (DefKind::Bot, "zed"),
                (DefKind::Trigger, "nightly"),
                (DefKind::Policy, "policy"),
            ]
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_reported_and_an_oversized_one_is_left_to_the_loader() {
        let dir = tempfile::tempdir().expect("tempdir");
        put(dir.path(), "bots/bad.toml", &[0xff, 0xfe]);
        let big = format!("{}{}", "a".repeat(MAX_FILE_BYTES - 1), "é".repeat(10));
        put(dir.path(), "bots/big.toml", big.as_bytes());
        let (files, problems) = read_files(dir.path()).expect("read");
        assert_eq!(
            problems.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["bots/bad.toml: the file is not valid UTF-8"]
        );
        let errors = load_set(&files, &ValidationContext::with_builtin([])).expect_err("too big");
        assert!(
            errors[0]
                .to_string()
                .starts_with("bots/big.toml: the file is ")
        );
    }

    #[test]
    fn a_directory_that_is_not_there_holds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (files, problems) = read_files(dir.path()).expect("read");
        assert!(files.is_empty() && problems.is_empty());
    }
}
