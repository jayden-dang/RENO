//! Reading the daemon's log files: the last lines, and one JSON line as text.
//!
//! The files are `<data-dir>/logs/renod.log` (JSON lines, rotated by size) and
//! `<data-dir>/logs/renod.stderr` (whatever the daemon wrote to stderr outside its logger). Only
//! the end of a file is read, so a full 10 MiB log costs a 64 KiB read.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The JSON log's file name.
pub const LOG_FILE: &str = "renod.log";
/// The stderr file's name.
pub const STDERR_FILE: &str = "renod.stderr";

/// How much of the end of a file [`tail_lines`] looks at.
const TAIL_WINDOW: u64 = 64 * 1024;

/// Where the daemon's logs are, given its data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPaths {
    /// The log directory.
    pub dir: PathBuf,
    /// The JSON log.
    pub log: PathBuf,
    /// The stderr file.
    pub stderr: PathBuf,
}

impl LogPaths {
    /// The log files of the daemon whose data directory is `data_dir`.
    pub fn of(data_dir: &Path) -> Self {
        let dir = data_dir.join("logs");
        Self {
            log: dir.join(LOG_FILE),
            stderr: dir.join(STDERR_FILE),
            dir,
        }
    }
}

/// The last `count` non-empty lines of the file at `path`. A missing file has none.
pub fn tail_lines(path: &Path, count: usize) -> io::Result<Vec<String>> {
    tail_lines_before(path, count, u64::MAX)
}

/// The last `count` complete lines that end at or before byte `end` of `path` (the whole file
/// when `end` is past its length). A follower that starts at `end` then continues exactly where
/// these lines stop: nothing written in between is lost or shown twice.
pub fn tail_lines_before(path: &Path, count: usize, end: u64) -> io::Result<Vec<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len().min(end);
    let start = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if start > 0 && !lines.is_empty() {
        // The window began mid-line.
        lines.remove(0);
    }
    let from = lines.len().saturating_sub(count);
    Ok(lines[from..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect())
}

/// One log line as text: `LEVEL message key=value ...` for the daemon's JSON lines, the line
/// itself for anything else.
pub fn render_line(line: &str) -> String {
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        return line.to_owned();
    };
    let level = object
        .get("level")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let time = object
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut message = String::new();
    let mut rest = Vec::new();
    if let Some(serde_json::Value::Object(fields)) = object.get("fields") {
        for (key, value) in fields {
            if key == "message" {
                message = value.as_str().unwrap_or_default().to_owned();
            } else {
                match value {
                    serde_json::Value::String(text) => rest.push(format!("{key}={text}")),
                    other => rest.push(format!("{key}={other}")),
                }
            }
        }
    }
    let mut out = format!("{time} {level:>5} {message}");
    for field in rest {
        out.push(' ');
        out.push_str(&field);
    }
    out.trim().to_owned()
}

/// What to show a person about why the daemon is not up: the stderr file's last lines if it has
/// any, else the JSON log's, one per line.
pub fn recent_trouble(paths: &LogPaths, count: usize) -> String {
    for (path, render) in [(&paths.stderr, false), (&paths.log, true)] {
        if let Ok(lines) = tail_lines(path, count)
            && !lines.is_empty()
        {
            let shown: Vec<String> = lines
                .iter()
                .map(|line| {
                    if render {
                        render_line(line)
                    } else {
                        line.clone()
                    }
                })
                .collect();
            return shown.join("\n");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_lines_of_a_file_and_nothing_for_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.log");
        assert_eq!(tail_lines(&path, 3).expect("missing"), Vec::<String>::new());
        std::fs::write(&path, "a\nb\n\nc\nd\n").expect("write");
        assert_eq!(tail_lines(&path, 3).expect("tail"), ["b", "c", "d"]);
        assert_eq!(tail_lines(&path, 10).expect("tail").len(), 4);
    }

    #[test]
    fn only_the_end_of_a_big_file_is_read_and_a_cut_line_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.log");
        let line = format!("{}\n", "x".repeat(99));
        std::fs::write(&path, line.repeat(5000)).expect("write");
        let tail = tail_lines(&path, 2).expect("tail");
        assert_eq!(tail, [&line[..99], &line[..99]]);
        let all = tail_lines(&path, 100_000).expect("tail");
        assert!(all.len() < 5000 && all.iter().all(|l| l.len() == 99));
    }

    #[test]
    fn json_lines_read_as_text_and_other_lines_pass_through() {
        let json = r#"{"timestamp":"2026-11-16T10:00:00Z","level":"ERROR","fields":{"message":"renod failed","error":"locked"},"target":"renod"}"#;
        assert_eq!(
            render_line(json),
            "2026-11-16T10:00:00Z ERROR renod failed error=locked"
        );
        assert_eq!(
            render_line("renod: another one is running"),
            "renod: another one is running"
        );
    }

    #[test]
    fn trouble_prefers_the_stderr_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = LogPaths::of(dir.path());
        std::fs::create_dir_all(&paths.dir).expect("dir");
        assert_eq!(recent_trouble(&paths, 5), "");
        std::fs::write(
            &paths.log,
            "{\"level\":\"INFO\",\"fields\":{\"message\":\"hi\"}}\n",
        )
        .expect("log");
        assert_eq!(recent_trouble(&paths, 5), "INFO hi");
        std::fs::write(&paths.stderr, "renod: cannot prepare /x\n").expect("stderr");
        assert_eq!(recent_trouble(&paths, 5), "renod: cannot prepare /x");
    }
}
