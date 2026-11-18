//! Exact-value redaction of a run's own secrets (T1.13).
//!
//! The shape-based redaction in the parent module guesses what a credential looks like. This one
//! knows: a run is started with a bot's secret values, and any of them that turns up in what the
//! run produces (agent text, tool input and output, diagnostics, an approval's subject) is
//! replaced by `[secret:NAME]` before the event is journaled or stored.
//!
//! Rules:
//!
//! - **Exact match, case-sensitive**, on the value as the Keychain holds it.
//! - **Minimum length [`MIN_LEN`]**: a value shorter than that is not redacted at all, because it
//!   would mangle ordinary text (a secret `1234` would eat every `1234`). Keep secrets long
//!   enough to be secrets.
//! - A value that spans lines (a private key) is also matched line by line, so a tool that
//!   prints it a line at a time is still caught. Lines shorter than [`MIN_LEN`] are left.
//! - A value with characters JSON escapes (`"`, `\`, a newline) is also matched in its escaped
//!   form, for text that carries JSON as a string.
//! - A value is also matched percent-encoded (`%2F`, in upper and lower case hex), for a URL or
//!   a form body that carries it.
//! - Longer needles first, so a value that contains another is replaced whole.
//!
//! Text that is cut after redaction needs care: a value cut in half leaves a fragment no exact
//! match can see. A tool's output is therefore handed over in a generous raw cut and cut to the
//! journal's size only after this runs (`ToolOutput::recut`); text that is cut before it is
//! seen (a patch) is passed through [`ValueRedactor::without_cut_tail`].
//!
//! A value split across consecutive text events is caught by [`ValueRedactor::push`] (the
//! supervisor holds back a possible partial value until the next piece decides it).
//!
//! What it does not catch: a value the agent transforms otherwise (base64, hex, reversed), or
//! two values that overlap each other in the text. The fence is the allowlist and the Keychain,
//! not this.

use std::borrow::Cow;

use reno_proto::ExecEvent;
use serde_json::Value;

/// The shortest secret value that is redacted. Shorter values pass through unchanged.
pub const MIN_LEN: usize = 8;

/// Replaces a run's secret values in text and events.
#[derive(Clone, Default)]
pub struct ValueRedactor {
    /// `(needle, replacement)`, longest needle first.
    needles: Vec<(String, String)>,
}

impl std::fmt::Debug for ValueRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValueRedactor")
            .field("needles", &self.needles.len())
            .finish()
    }
}

impl ValueRedactor {
    /// A redactor for `(name, value)` pairs.
    pub fn new<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut needles: Vec<(String, String)> = Vec::new();
        let mut add = |needle: &str, name: &str| {
            if needle.len() >= MIN_LEN && !needles.iter().any(|(n, _)| n == needle) {
                needles.push((needle.to_owned(), format!("[secret:{name}]")));
            }
        };
        for (name, value) in secrets {
            add(value, name);
            if value.contains(['"', '\\', '\n', '\r', '\t']) {
                let quoted = Value::String(value.to_owned()).to_string();
                add(&quoted[1..quoted.len() - 1], name);
            }
            for encoded in [percent_encode(value, true), percent_encode(value, false)] {
                if encoded != value {
                    add(&encoded, name);
                }
            }
            if value.contains('\n') {
                for line in value.lines() {
                    add(line.trim(), name);
                }
            }
        }
        needles.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self { needles }
    }

    /// Nothing to redact: no secret was long enough.
    pub fn is_empty(&self) -> bool {
        self.needles.is_empty()
    }

    /// How many bytes at the end of `text` could be the start of a secret value that the next
    /// piece of text completes: the longest suffix that is a proper prefix of some needle and
    /// does not cut through a match already complete in `text`. 0 when none.
    pub fn partial_suffix_len(&self, text: &str) -> usize {
        let longest = self.needles.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        if longest < 2 {
            return 0;
        }
        let from = text.len().saturating_sub(longest - 1);
        (from..text.len())
            .filter(|&i| text.is_char_boundary(i))
            .find(|&i| {
                let tail = &text[i..];
                self.needles
                    .iter()
                    .any(|(n, _)| n.len() > tail.len() && n.starts_with(tail))
                    && !self.match_crosses(text, i)
            })
            .map_or(0, |i| text.len() - i)
    }

    /// Whether a complete needle in `text` starts before byte `at` and ends after it.
    fn match_crosses(&self, text: &str, at: usize) -> bool {
        self.needles.iter().any(|(needle, _)| {
            let lo = at.saturating_sub(needle.len() - 1);
            (lo..at).any(|pos| {
                text.is_char_boundary(pos)
                    && text[pos..].starts_with(needle.as_str())
                    && pos + needle.len() > at
            })
        })
    }

    /// `text` with every secret value replaced.
    pub fn redact_str<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let mut out = Cow::Borrowed(text);
        for (needle, replacement) in &self.needles {
            if out.contains(needle.as_str()) {
                out = Cow::Owned(out.replace(needle.as_str(), replacement));
            }
        }
        out
    }

    /// `text`, which was cut short somewhere else, without a trailing fragment (four bytes or
    /// more) of a secret value: the rest of it is gone, so no exact match would see it.
    pub fn without_cut_tail<'t>(&self, text: &'t str) -> &'t str {
        let mut end = text.len();
        for (needle, _) in &self.needles {
            for k in (4..needle.len()).rev() {
                if k > text.len() {
                    continue;
                }
                let at = text.len() - k;
                if at < end
                    && text.is_char_boundary(at)
                    && needle.is_char_boundary(k)
                    && text.ends_with(&needle[..k])
                {
                    end = at;
                    break;
                }
            }
        }
        &text[..end]
    }

    /// `event` with every secret value replaced in every string it carries, however deep.
    ///
    /// Works on the event's JSON form, so a variant added later is covered without a change
    /// here. An event that cannot make the round trip is replaced by a notice rather than passed
    /// on unredacted.
    pub fn redact_event(&self, event: ExecEvent) -> ExecEvent {
        if self.is_empty() {
            return event;
        }
        let Ok(mut value) = serde_json::to_value(&event) else {
            return Self::withheld();
        };
        if !self.redact_value(&mut value) {
            return event;
        }
        serde_json::from_value(value).unwrap_or_else(|_| Self::withheld())
    }

    fn withheld() -> ExecEvent {
        ExecEvent::Text {
            text: "[an event was withheld: it could not be redacted]".to_owned(),
        }
    }

    /// Redacts every string in `value`. Whether anything changed.
    fn redact_value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) => match self.redact_str(text) {
                Cow::Borrowed(_) => false,
                Cow::Owned(redacted) => {
                    *text = redacted;
                    true
                }
            },
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.redact_value(item) | changed),
            Value::Object(map) => map
                .values_mut()
                .fold(false, |changed, item| self.redact_value(item) | changed),
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }
}

/// `value` percent-encoded: everything but the unreserved characters becomes `%XX`.
fn percent_encode(value: &str, upper: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else if upper {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use reno_proto::{ActionClass, StopReason, ToolCall};
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "ghp_exampleExampleExample1234";

    fn redactor() -> ValueRedactor {
        ValueRedactor::new([("GITHUB_TOKEN", TOKEN), ("PIN", "1234")])
    }

    #[test]
    fn a_secret_value_is_replaced_by_its_name() {
        let text = format!("curl -H 'Authorization: token {TOKEN}' https://api");
        assert_eq!(
            redactor().redact_str(&text),
            "curl -H 'Authorization: token [secret:GITHUB_TOKEN]' https://api"
        );
    }

    #[test]
    fn a_value_below_the_minimum_length_is_left_alone() {
        let text = "the pin is 1234 and 1234 again";
        assert_eq!(redactor().redact_str(text), text);
        assert!(ValueRedactor::new([("PIN", "1234567")]).is_empty());
        assert!(!ValueRedactor::new([("PIN", "12345678")]).is_empty());
    }

    #[test]
    fn matching_is_exact() {
        let near = TOKEN.to_ascii_uppercase();
        assert_eq!(redactor().redact_str(&near), near.as_str());
        assert_eq!(redactor().redact_str("ghp_example"), "ghp_example");
    }

    #[test]
    fn a_multiline_value_is_caught_line_by_line_and_escaped() {
        let key = "-----BEGIN KEY-----\nAAAAAAAAAAAAAAAA\n-----END KEY-----";
        let redactor = ValueRedactor::new([("DEPLOY_KEY", key)]);
        assert_eq!(redactor.redact_str(key), "[secret:DEPLOY_KEY]");
        assert_eq!(
            redactor.redact_str("line: AAAAAAAAAAAAAAAA"),
            "line: [secret:DEPLOY_KEY]"
        );
        let escaped = r"-----BEGIN KEY-----\nAAAAAAAAAAAAAAAA\n-----END KEY-----";
        assert_eq!(redactor.redact_str(escaped), "[secret:DEPLOY_KEY]");
    }

    #[test]
    fn the_longer_value_wins_when_one_contains_another() {
        let redactor = ValueRedactor::new([("SHORT", "abcdefgh"), ("LONG", "abcdefgh-ijklmnop")]);
        assert_eq!(
            redactor.redact_str("x abcdefgh-ijklmnop y"),
            "x [secret:LONG] y"
        );
    }

    #[test]
    fn an_event_is_redacted_wherever_the_value_sits() {
        let call = ToolCall::new(
            "Bash",
            json!({"command": format!("echo {TOKEN}"), "nested": [{"k": TOKEN}]}),
            ActionClass::Command,
        );
        let tool = redactor().redact_event(ExecEvent::Tool {
            id: "t1".into(),
            call,
        });
        let shown = serde_json::to_string(&tool).unwrap();
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("[secret:GITHUB_TOKEN]"), "{shown}");

        let done = redactor().redact_event(ExecEvent::Done {
            stop: StopReason::Error,
            result: Some(format!("used {TOKEN}")),
            diagnostic: Some(format!("failed with {TOKEN}")),
            reason: Some(reno_proto::StopDetail::StartRefused {
                failure: reno_proto::StartRefusal::Invalid,
                detail: format!("refused with {TOKEN}"),
            }),
        });
        let ExecEvent::Done {
            result, diagnostic, ..
        } = done
        else {
            panic!("a Done stays a Done");
        };
        assert_eq!(result.unwrap(), "used [secret:GITHUB_TOKEN]");
        assert_eq!(diagnostic.unwrap(), "failed with [secret:GITHUB_TOKEN]");
    }

    #[test]
    fn an_agent_report_is_redacted_in_its_summary_and_details() {
        let report = redactor().redact_event(ExecEvent::AgentReport {
            status: reno_proto::agent::ReportStatus::Blocked,
            summary: format!("cannot log in with {TOKEN}"),
            details: Some(format!("the token {TOKEN} was rejected")),
        });
        let ExecEvent::AgentReport {
            summary, details, ..
        } = report
        else {
            panic!("a report stays a report");
        };
        assert_eq!(summary, "cannot log in with [secret:GITHUB_TOKEN]");
        assert_eq!(
            details.unwrap(),
            "the token [secret:GITHUB_TOKEN] was rejected"
        );
    }

    #[test]
    fn an_event_without_a_secret_comes_back_unchanged() {
        let event = ExecEvent::Text {
            text: "nothing to hide".into(),
        };
        assert_eq!(redactor().redact_event(event.clone()), event);
    }

    #[test]
    fn a_percent_encoded_value_is_caught_in_either_case() {
        let value = "p@ss/word+with space&more";
        let redactor = ValueRedactor::new([("PW", value)]);
        assert_eq!(
            redactor.redact_str("https://u:p%40ss%2Fword%2Bwith%20space%26more@host"),
            "https://u:[secret:PW]@host"
        );
        assert_eq!(
            redactor.redact_str("q=p%40ss%2fword%2bwith%20space%26more"),
            "q=[secret:PW]"
        );
        // A value with nothing to encode has no second needle.
        assert_eq!(ValueRedactor::new([("T", TOKEN)]).needles.len(), 1);
    }

    #[test]
    fn a_json_escaped_value_in_a_structured_result_is_caught() {
        let value = "pa\"ss\\wo\nrd-1234";
        let redactor = ValueRedactor::new([("PW", value)]);
        let inner = json!({"password": value}).to_string();
        assert!(inner.contains(r#"pa\"ss\\wo\nrd"#), "{inner}");
        let event = redactor.redact_event(ExecEvent::ToolResult {
            id: "t".into(),
            ok: true,
            summary: None,
            output: Some(reno_proto::ToolOutput::from_text(&inner)),
        });
        let ExecEvent::ToolResult { output, .. } = event else {
            panic!("a result stays a result");
        };
        assert_eq!(output.unwrap().text, r#"{"password":"[secret:PW]"}"#);
    }

    #[test]
    fn regex_characters_and_overlaps_are_plain_text() {
        let redactor = ValueRedactor::new([("RE", "a.*b(c)[d]+$^"), ("A", "abcdefghij")]);
        assert_eq!(redactor.redact_str("x a.*b(c)[d]+$^ y"), "x [secret:RE] y");
        assert_eq!(redactor.redact_str("aXXb(c)[d]+$^"), "aXXb(c)[d]+$^");
        assert_eq!(redactor.redact_str(""), "");
    }

    #[test]
    fn a_trailing_fragment_of_a_value_is_dropped_from_cut_text() {
        let redactor = redactor();
        let cut = &TOKEN[..12];
        let text = format!("diff --git a b\n+token = {cut}");
        assert_eq!(
            redactor.without_cut_tail(&text),
            "diff --git a b\n+token = "
        );
        // A short tail is not a fragment worth dropping.
        assert_eq!(redactor.without_cut_tail("ends with ghp"), "ends with ghp");
        assert_eq!(redactor.without_cut_tail("fine"), "fine");
    }

    #[test]
    fn debug_shows_no_values() {
        assert!(!format!("{:?}", redactor()).contains("ghp_"));
    }
}
