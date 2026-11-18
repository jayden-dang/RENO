//! Turning off a CLI's own updater, and putting it back (EX-12).
//!
//! Two CLIs update themselves: Claude Code (`env.DISABLE_AUTOUPDATER = "1"` in
//! `~/.claude/settings.json`; this also stops plugin auto-update) and Grok (`[cli] auto_update =
//! false` in `~/.grok/config.toml`). While Reno's auto-update is on it leaves both alone. When a
//! person turns it off, Reno turns the CLI's own updater off with it; turning it on again puts
//! back what was there before.
//!
//! An edit keeps everything else in the file (a TOML edit keeps comments and layout, a JSON edit
//! keeps key order) and is atomic: the new text goes to a temporary file in the same directory
//! and is renamed over the old one. A file that does not parse is never touched. The paths are
//! injected ([`SelfUpdateFiles::new`]); tests use a temporary directory and never the real files.
//!
//! What was there before is returned as an opaque string for the caller to store before it
//! relies on the edit, and handed back to [`SelfUpdateControl::restore`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A CLI's own updater, and where its switch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelfUpdateSwitch {
    /// Claude Code: `env.DISABLE_AUTOUPDATER` in its `settings.json`.
    ClaudeSettings,
    /// Grok: `[cli] auto_update` in its `config.toml`.
    GrokConfig,
}

/// Why a switch could not be moved.
#[derive(Debug, thiserror::Error)]
pub enum SelfUpdateError {
    /// The file could not be read or written.
    #[error("could not edit {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
    },
    /// The file is not what the CLI writes; it was left as it was.
    #[error("{path} was left alone: {why}")]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The record of the previous value is not one this build wrote.
    #[error("the stored previous value is not readable")]
    BadRecord,
}

/// The port the daemon uses to move the switches. The kernel only calls it.
pub trait SelfUpdateControl: Send + Sync + std::fmt::Debug {
    /// Turns the CLI's own updater off and returns what the key held before, as a record to
    /// store and later give to [`restore`](Self::restore).
    fn turn_off(&self, switch: SelfUpdateSwitch) -> Result<String, SelfUpdateError>;

    /// Puts the key back as `previous` (a record [`turn_off`](Self::turn_off) returned) says.
    fn restore(&self, switch: SelfUpdateSwitch, previous: &str) -> Result<(), SelfUpdateError>;
}

/// What a key held before Reno set it.
#[derive(Debug, Serialize, Deserialize)]
struct Previous {
    /// The value, or absent when the key was not there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    was: Option<serde_json::Value>,
    /// Reno created the table the key is in (`env`, `[cli]`), so it removes it again when empty.
    #[serde(default)]
    parent: bool,
}

/// The two files, at injected paths.
#[derive(Debug, Clone)]
pub struct SelfUpdateFiles {
    claude_settings: PathBuf,
    grok_config: PathBuf,
}

impl SelfUpdateFiles {
    /// The files at these paths.
    pub fn new(claude_settings: PathBuf, grok_config: PathBuf) -> Self {
        Self {
            claude_settings,
            grok_config,
        }
    }

    /// The files in their usual place below `home`.
    pub fn in_home(home: &Path) -> Self {
        Self::new(
            home.join(".claude").join("settings.json"),
            home.join(".grok").join("config.toml"),
        )
    }

    fn path(&self, switch: SelfUpdateSwitch) -> &Path {
        match switch {
            SelfUpdateSwitch::ClaudeSettings => &self.claude_settings,
            SelfUpdateSwitch::GrokConfig => &self.grok_config,
        }
    }
}

impl SelfUpdateControl for SelfUpdateFiles {
    fn turn_off(&self, switch: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        let path = self.path(switch);
        let text = read(path)?;
        let (edited, previous) = match switch {
            SelfUpdateSwitch::ClaudeSettings => claude_off(path, &text)?,
            SelfUpdateSwitch::GrokConfig => grok_off(path, &text)?,
        };
        write_atomically(path, &edited)?;
        serde_json::to_string(&previous).map_err(|_| SelfUpdateError::BadRecord)
    }

    fn restore(&self, switch: SelfUpdateSwitch, previous: &str) -> Result<(), SelfUpdateError> {
        let previous: Previous =
            serde_json::from_str(previous).map_err(|_| SelfUpdateError::BadRecord)?;
        let path = self.path(switch);
        if !path.exists() {
            // Nothing to put back into.
            return Ok(());
        }
        let text = read(path)?;
        let edited = match switch {
            SelfUpdateSwitch::ClaudeSettings => claude_restore(path, &text, previous)?,
            SelfUpdateSwitch::GrokConfig => grok_restore(path, &text, previous)?,
        };
        write_atomically(path, &edited)
    }
}

const CLAUDE_KEY: &str = "DISABLE_AUTOUPDATER";

fn unreadable(path: &Path, why: impl Into<String>) -> SelfUpdateError {
    SelfUpdateError::Unreadable {
        path: path.to_owned(),
        why: why.into(),
    }
}

fn claude_off(path: &Path, text: &str) -> Result<(String, Previous), SelfUpdateError> {
    let mut root = parse_json(path, text)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| unreadable(path, "it is not a JSON object"))?;
    let parent = !object.contains_key("env");
    let env = object
        .entry("env")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| unreadable(path, "`env` is not an object"))?;
    let was = env.insert(CLAUDE_KEY.to_owned(), "1".into());
    Ok((render_json(&root), Previous { was, parent }))
}

fn claude_restore(path: &Path, text: &str, previous: Previous) -> Result<String, SelfUpdateError> {
    let mut root = parse_json(path, text)?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| unreadable(path, "it is not a JSON object"))?;
    let Some(env) = object.get_mut("env").and_then(|env| env.as_object_mut()) else {
        // The person removed the table since: there is nothing of Reno's to take out.
        return Ok(text.to_owned());
    };
    match previous.was {
        Some(was) => {
            env.insert(CLAUDE_KEY.to_owned(), was);
        }
        None => {
            env.shift_remove(CLAUDE_KEY);
        }
    }
    if previous.parent && env.is_empty() {
        object.shift_remove("env");
    }
    Ok(render_json(&root))
}

fn parse_json(path: &Path, text: &str) -> Result<serde_json::Value, SelfUpdateError> {
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(text).map_err(|e| unreadable(path, format!("it is not valid JSON ({e})")))
}

fn render_json(root: &serde_json::Value) -> String {
    let mut text = serde_json::to_string_pretty(root).unwrap_or_default();
    text.push('\n');
    text
}

fn grok_off(path: &Path, text: &str) -> Result<(String, Previous), SelfUpdateError> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| unreadable(path, format!("it is not valid TOML ({e})")))?;
    let parent = !doc.contains_key("cli");
    let cli = doc
        .entry("cli")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| unreadable(path, "`cli` is not a table"))?;
    let was = cli
        .get("auto_update")
        .and_then(toml_edit::Item::as_bool)
        .map(serde_json::Value::Bool);
    if cli.get("auto_update").is_some() && was.is_none() {
        return Err(unreadable(path, "`cli.auto_update` is not a boolean"));
    }
    cli["auto_update"] = toml_edit::value(false);
    Ok((doc.to_string(), Previous { was, parent }))
}

fn grok_restore(path: &Path, text: &str, previous: Previous) -> Result<String, SelfUpdateError> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| unreadable(path, format!("it is not valid TOML ({e})")))?;
    let Some(cli) = doc.get_mut("cli").and_then(toml_edit::Item::as_table_mut) else {
        return Ok(text.to_owned());
    };
    match previous.was.as_ref().and_then(serde_json::Value::as_bool) {
        Some(was) => cli["auto_update"] = toml_edit::value(was),
        None => {
            cli.remove("auto_update");
        }
    }
    if previous.parent && cli.is_empty() {
        doc.remove("cli");
    }
    Ok(doc.to_string())
}

fn read(path: &Path) -> Result<String, SelfUpdateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        // A CLI that has not written its file yet: Reno writes it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(SelfUpdateError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Writes `text` to a temporary file next to `path` and renames it over `path`. A symlink is
/// followed, so the link stays a link; the file keeps its permissions.
fn write_atomically(path: &Path, text: &str) -> Result<(), SelfUpdateError> {
    let io = |source| SelfUpdateError::Io {
        path: path.to_owned(),
        source,
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(".{name}.reno-{}", std::process::id()));
    std::fs::write(&temp, text).map_err(io)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        // Best effort: the file the person had keeps its mode.
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    std::fs::rename(&temp, &target).map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        io(source)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn files(dir: &Path) -> SelfUpdateFiles {
        SelfUpdateFiles::in_home(dir)
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn claude_off_sets_the_key_keeps_the_rest_and_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let original = "{\n  \"zebra\": 1,\n  \"env\": {\n    \"FOO\": \"bar\"\n  },\n  \"alpha\": [\n    1,\n    2\n  ]\n}\n";
        put(&path, original);
        let switch = SelfUpdateSwitch::ClaudeSettings;
        let previous = files(dir.path()).turn_off(switch).unwrap();
        let edited: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(edited["env"]["DISABLE_AUTOUPDATER"], "1");
        assert_eq!(edited["env"]["FOO"], "bar");
        assert_eq!(edited["alpha"], serde_json::json!([1, 2]));
        // Keys keep the order the person had them in.
        let keys: Vec<_> = edited.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zebra", "env", "alpha"]);
        files(dir.path()).restore(switch, &previous).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn claude_restore_puts_back_a_value_the_person_had() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        put(&path, r#"{"env":{"DISABLE_AUTOUPDATER":"0"}}"#);
        let switch = SelfUpdateSwitch::ClaudeSettings;
        let previous = files(dir.path()).turn_off(switch).unwrap();
        let now: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(now["env"]["DISABLE_AUTOUPDATER"], "1");
        files(dir.path()).restore(switch, &previous).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["env"]["DISABLE_AUTOUPDATER"], "0");
    }

    #[test]
    fn claude_creates_the_file_and_removes_what_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let switch = SelfUpdateSwitch::ClaudeSettings;
        let previous = files(dir.path()).turn_off(switch).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"1\""));
        files(dir.path()).restore(switch, &previous).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({}));
    }

    #[test]
    fn a_file_that_does_not_parse_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        put(&path, "{ not json");
        let error = files(dir.path())
            .turn_off(SelfUpdateSwitch::ClaudeSettings)
            .unwrap_err();
        assert!(matches!(error, SelfUpdateError::Unreadable { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let path = dir.path().join(".grok/config.toml");
        put(&path, "[cli\nauto_update =");
        files(dir.path())
            .turn_off(SelfUpdateSwitch::GrokConfig)
            .unwrap_err();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[cli\nauto_update ="
        );
    }

    #[test]
    fn grok_off_keeps_comments_and_restores_the_previous_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".grok/config.toml");
        let original = "# my grok\n[ui]\ntheme = \"dark\"  # keep\n\n[cli]\nauto_update = true # on\nuse_leader = false\n";
        put(&path, original);
        let switch = SelfUpdateSwitch::GrokConfig;
        let previous = files(dir.path()).turn_off(switch).unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("auto_update = false"), "{edited}");
        assert!(edited.contains("# my grok"));
        assert!(edited.contains("theme = \"dark\"  # keep"));
        assert!(edited.contains("use_leader = false"));
        files(dir.path()).restore(switch, &previous).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("auto_update = true"), "{back}");
        assert!(back.contains("# my grok"));
    }

    #[test]
    fn grok_removes_the_key_and_table_it_added() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".grok/config.toml");
        put(&path, "[ui]\ntheme = \"dark\"\n");
        let switch = SelfUpdateSwitch::GrokConfig;
        let previous = files(dir.path()).turn_off(switch).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("auto_update = false")
        );
        files(dir.path()).restore(switch, &previous).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ui]\ntheme = \"dark\"\n"
        );
    }

    #[test]
    fn a_symlinked_file_stays_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        put(&real, "{}");
        let link = dir.path().join(".claude/settings.json");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        files(dir.path())
            .turn_off(SelfUpdateSwitch::ClaudeSettings)
            .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::read_to_string(&real).unwrap().contains("\"1\""));
    }

    #[test]
    fn a_garbled_record_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        put(&dir.path().join(".grok/config.toml"), "[cli]\n");
        assert!(matches!(
            files(dir.path()).restore(SelfUpdateSwitch::GrokConfig, "nope"),
            Err(SelfUpdateError::BadRecord)
        ));
    }
}
