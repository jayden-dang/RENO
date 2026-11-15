//! Claude's native history on disk: whether there is a conversation to resume at all.

use serde_json::Value;

/// What `--resume <id>` would find in Claude's native history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transcript {
    /// A transcript with conversation in it.
    Conversation,
    /// A transcript read through to its end without a single user or assistant line. The CLI
    /// refuses to resume it ("No conversation found") and exits.
    Empty,
    /// The history could not be looked at, or holds no file for the session: the CLI may keep it
    /// under another config root (a wrapper setting its own `CLAUDE_CONFIG_DIR`). Leave it to
    /// the CLI, whose "No conversation found" relaunches the run fresh when it truly is gone.
    Unknown,
}

/// Look in Claude's history for the session. Session ids are global; project directories are
/// searched rather than duplicating the CLI's cwd encoding (including long-path hashing). Long
/// sessions' histories run to hundreds of MB: the scan runs off the runtime and only parses
/// lines that can show a conversation.
pub(super) async fn transcript(config_root: &std::path::Path, session_id: &str) -> Transcript {
    let config_root = config_root.to_owned();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || scan(&config_root, &session_id))
        .await
        .unwrap_or(Transcript::Unknown)
}

fn scan(config_root: &std::path::Path, session_id: &str) -> Transcript {
    use std::io::BufRead as _;
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Transcript::Unknown;
    }
    let Ok(projects) = std::fs::read_dir(config_root.join("projects")) else {
        return Transcript::Unknown;
    };
    let Some(file) = projects.flatten().find_map(|project| {
        std::fs::File::open(project.path().join(format!("{session_id}.jsonl"))).ok()
    }) else {
        return Transcript::Unknown;
    };
    // Only a transcript read whole, every line understood, is Empty: a read error or a line that
    // might have been conversation leaves the call to the CLI.
    let mut doubtful = false;
    let mut reader = std::io::BufReader::with_capacity(1 << 16, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Err(_) => {
                doubtful = true;
                break;
            }
            Ok(_) => {}
        }
        // Torn lines and older/newer unknown shapes are harmless.
        let Ok(text) = std::str::from_utf8(&line) else {
            doubtful = true;
            continue;
        };
        // Most of a transcript is bookkeeping (queue operations, request records,
        // attachments); one user or assistant record is enough.
        if text.contains("\"type\":\"user\"") || text.contains("\"type\":\"assistant\"") {
            match serde_json::from_str::<Value>(text) {
                Ok(record)
                    if matches!(
                        record.get("type").and_then(Value::as_str),
                        Some("user" | "assistant")
                    ) =>
                {
                    return Transcript::Conversation;
                }
                Ok(_) => {}
                Err(_) => doubtful = true,
            }
        }
    }
    if doubtful {
        Transcript::Unknown
    } else {
        Transcript::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, name: &str, body: &[u8]) {
        let project = root.join("projects/encoded-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{name}.jsonl")), body).unwrap();
    }

    #[tokio::test]
    async fn a_transcript_with_a_conversation_resumes() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "session-1",
            concat!(
                "not json\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
                "\n{torn tail"
            )
            .as_bytes(),
        );
        assert_eq!(
            transcript(root.path(), "session-1").await,
            Transcript::Conversation
        );
        // Another config root may hold it: only the CLI can tell.
        assert_eq!(
            transcript(root.path(), "missing").await,
            Transcript::Unknown
        );
    }

    #[tokio::test]
    async fn a_transcript_without_conversation_cannot_be_resumed() {
        let root = tempfile::tempdir().unwrap();
        // What a run that died before its first reply leaves behind.
        write(
            root.path(),
            "bare",
            concat!(
                r#"{"type":"queue-operation","operation":"enqueue","content":"{\"type\":\"user\"}"}"#,
                "\n",
                r#"{"type":"last-prompt","lastPrompt":"hi"}"#,
                "\n",
            )
            .as_bytes(),
        );
        assert_eq!(transcript(root.path(), "bare").await, Transcript::Empty);
        // A line that might have been conversation, cut short or not text: the CLI decides.
        for (name, body) in [
            (
                "torn",
                &b"{\"type\":\"last-prompt\"}\n{\"type\":\"user\",\"mess"[..],
            ),
            ("garbled", &b"{\"type\":\"last-prompt\"}\n\xff\xfe\n"[..]),
        ] {
            write(root.path(), name, body);
            assert_eq!(
                transcript(root.path(), name).await,
                Transcript::Unknown,
                "{name}"
            );
        }
        // No history at all: nothing can be said either way.
        let nowhere = tempfile::tempdir().unwrap();
        assert_eq!(
            transcript(nowhere.path(), "bare").await,
            Transcript::Unknown
        );
        // A session id that is not a plain id is never joined into a path.
        assert_eq!(
            transcript(root.path(), "../bare").await,
            Transcript::Unknown
        );
    }
}
