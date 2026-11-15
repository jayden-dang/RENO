//! Claude's native history on disk: the spawns a resumed chat made, and whether there is a
//! conversation to resume at all. The frame-level logic lives in
//! `crate::wire::claude::normalize`; only the file reading is here.

use serde_json::Value;

use crate::wire::claude::normalize::{Normalizer, Transcript};

/// Restore only spawn identity from Claude's native history. The CLI does
/// not replay task_started when --resume starts a new process. Session ids
/// are global; search project directories rather than duplicating the CLI's
/// cwd encoding (including long-path hashing). Long sessions' histories run
/// to hundreds of MB: the scan runs off the runtime and only parses lines
/// that can name a spawn or its agent. The same scan says whether there is a
/// conversation to resume at all.
pub(crate) async fn for_resume(
    config_root: &std::path::Path,
    session_id: &str,
) -> (Normalizer, Transcript) {
    let config_root = config_root.to_owned();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || restore_spawns(&config_root, &session_id))
        .await
        .unwrap_or_else(|_| (Normalizer::new(), Transcript::Unknown))
}

fn restore_spawns(config_root: &std::path::Path, session_id: &str) -> (Normalizer, Transcript) {
    use std::io::BufRead as _;
    let mut norm = Normalizer::new();
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return (norm, Transcript::Unknown);
    }
    let Ok(projects) = std::fs::read_dir(config_root.join("projects")) else {
        return (norm, Transcript::Unknown);
    };
    let Some(file) = projects.flatten().find_map(|project| {
        std::fs::File::open(project.path().join(format!("{session_id}.jsonl"))).ok()
    }) else {
        return (norm, Transcript::Unknown);
    };
    let mut transcript = Transcript::Empty;
    // Only a transcript read whole, every line understood, is Empty: a
    // read error or a line that might have been conversation leaves the
    // call to the CLI.
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
        // Most of a transcript is bookkeeping (queue operations, request
        // records, attachments); one user or assistant record is enough.
        if transcript == Transcript::Empty
            && (text.contains("\"type\":\"user\"") || text.contains("\"type\":\"assistant\""))
        {
            match serde_json::from_str::<Value>(text) {
                Ok(record) => {
                    if matches!(
                        record.get("type").and_then(Value::as_str),
                        Some("user" | "assistant")
                    ) {
                        transcript = Transcript::Conversation;
                    }
                }
                Err(_) => doubtful = true,
            }
        }
        if !(text.contains("\"Agent\"") || text.contains("\"Task\"") || text.contains("agentId")) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        norm.restore_spawn_record(&record);
    }
    if doubtful && transcript == Transcript::Empty {
        transcript = Transcript::Unknown;
    }
    (norm, transcript)
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
    async fn resume_reads_spawns_and_reports_a_conversation() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "session-1",
            concat!(
                "not json\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_spawn","name":"Agent","input":{"description":"scout"}}]}}"#,
                "\n",
                r#"{"type":"user","toolUseResult":{"agentId":"a1","status":"async_launched"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_spawn"}]}}"#,
                "\n{torn tail"
            )
            .as_bytes(),
        );
        let (mut norm, transcript) = for_resume(root.path(), "session-1").await;
        assert_eq!(transcript, Transcript::Conversation);
        // The spawn survived the file scan: a later task start for it resolves.
        let frame = crate::wire::claude::wire::parse_frame(
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_spawn","status":"completed"}"#,
        )
        .unwrap();
        assert!(!norm.normalize(frame, false).is_empty());
        // Another config root may hold it: only the CLI can tell.
        let (_, transcript) = for_resume(root.path(), "missing").await;
        assert_eq!(transcript, Transcript::Unknown);
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
        assert_eq!(for_resume(root.path(), "bare").await.1, Transcript::Empty);
        // A line that might have been conversation, cut short or not text:
        // the CLI decides.
        for (name, body) in [
            (
                "torn",
                &b"{\"type\":\"last-prompt\"}\n{\"type\":\"user\",\"mess"[..],
            ),
            ("garbled", &b"{\"type\":\"last-prompt\"}\n\xff\xfe\n"[..]),
        ] {
            write(root.path(), name, body);
            assert_eq!(
                for_resume(root.path(), name).await.1,
                Transcript::Unknown,
                "{name}"
            );
        }
        // No history at all: nothing can be said either way.
        let nowhere = tempfile::tempdir().unwrap();
        assert_eq!(
            for_resume(nowhere.path(), "bare").await.1,
            Transcript::Unknown
        );
    }
}
