//! Secret redaction across streamed text pieces.
//!
//! An agent's text arrives as many small `Text` / `Reasoning` events, and a secret value can be
//! split across them. [`ValueRedactor::push`] keeps the caller's state in a [`Held`]: the longest
//! tail of the text so far that could be the start of a secret value. It is not emitted until the
//! next piece of the same kind decides it (a match is redacted, a non-match goes out intact), or
//! the caller flushes it ([`ValueRedactor::flush`]) because something else happened: another
//! kind of event, the end of the run, or quiet.

use reno_proto::ExecEvent;

use super::ValueRedactor;

/// Which streamed text a held tail belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
}

/// Raw text held back because it could be the start of a secret value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    kind: Kind,
    text: String,
}

fn event(kind: Kind, text: String) -> ExecEvent {
    match kind {
        Kind::Text => ExecEvent::Text { text },
        Kind::Reasoning => ExecEvent::Reasoning { text },
    }
}

impl ValueRedactor {
    /// Redacts `event`, which follows whatever `held` was kept. Returns the events to emit, in
    /// order, and the tail to keep for the next call.
    ///
    /// Anything but a text piece of the held kind flushes `held` first, so order is kept.
    pub fn push(&self, held: Option<Held>, ev: ExecEvent) -> (Vec<ExecEvent>, Option<Held>) {
        let (kind, text) = match ev {
            ExecEvent::Text { text } if !self.is_empty() => (Kind::Text, text),
            ExecEvent::Reasoning { text } if !self.is_empty() => (Kind::Reasoning, text),
            other => {
                let mut out: Vec<ExecEvent> = held.map(|h| self.flush(h)).into_iter().collect();
                out.push(self.redact_event(other));
                return (out, None);
            }
        };
        let mut out = Vec::new();
        let mut buf = String::new();
        if let Some(held) = held {
            if held.kind == kind {
                buf = held.text;
            } else {
                out.push(self.flush(held));
            }
        }
        buf.push_str(&text);
        let keep = self.partial_suffix_len(&buf);
        let split = buf.len() - keep;
        let safe = self.redact_str(&buf[..split]).into_owned();
        if !safe.is_empty() || keep == 0 {
            out.push(event(kind, safe));
        }
        let held = (keep > 0).then(|| Held {
            kind,
            text: buf[split..].to_owned(),
        });
        (out, held)
    }

    /// The event a held tail becomes when nothing more is coming for it.
    pub fn flush(&self, held: Held) -> ExecEvent {
        event(held.kind, self.redact_str(&held.text).into_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-probe-9f3a7c21d4e85b60";

    fn redactor() -> ValueRedactor {
        ValueRedactor::new([("PROBE_TOKEN", TOKEN)])
    }

    fn text(t: &str) -> ExecEvent {
        ExecEvent::Text { text: t.into() }
    }

    fn joined(events: &[ExecEvent]) -> String {
        events
            .iter()
            .map(|e| match e {
                ExecEvent::Text { text } => text.as_str(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// Feeds the pieces, flushes at the end, returns the joined text.
    fn run(redactor: &ValueRedactor, pieces: &[&str]) -> String {
        let mut held = None;
        let mut out = Vec::new();
        for piece in pieces {
            let (events, next) = redactor.push(held.take(), text(piece));
            out.extend(events);
            held = next;
        }
        if let Some(h) = held {
            out.push(redactor.flush(h));
        }
        joined(&out)
    }

    #[test]
    fn a_value_split_at_every_offset_is_redacted() {
        let r = redactor();
        let whole = format!("token={TOKEN} done");
        for cut in 0..=whole.len() {
            let (a, b) = whole.split_at(cut);
            assert_eq!(
                run(&r, &[a, b]),
                "token=[secret:PROBE_TOKEN] done",
                "cut {cut}"
            );
        }
    }

    #[test]
    fn one_char_at_a_time_is_redacted() {
        let whole = format!("a {TOKEN} b");
        let chars: Vec<String> = whole.chars().map(String::from).collect();
        let pieces: Vec<&str> = chars.iter().map(String::as_str).collect();
        assert_eq!(run(&redactor(), &pieces), "a [secret:PROBE_TOKEN] b");
    }

    #[test]
    fn the_live_case_is_redacted() {
        let out = run(
            &redactor(),
            &["...token=", "sk-probe-9f", "3a7c21d4e85b60\n"],
        );
        assert_eq!(out, "...token=[secret:PROBE_TOKEN]\n");
    }

    #[test]
    fn a_lookalike_that_never_completes_comes_out_intact() {
        assert_eq!(
            run(&redactor(), &["x sk-probe-9f", "zzz and sk-pr"]),
            "x sk-probe-9fzzz and sk-pr"
        );
    }

    #[test]
    fn a_json_escaped_value_split_is_redacted() {
        let r = ValueRedactor::new([("K", "line-one-secret\nline-two-secret")]);
        let escaped = r"say line-one-secret\nline-two-secret ok";
        for cut in 0..=escaped.len() {
            let (a, b) = escaped.split_at(cut);
            assert_eq!(run(&r, &[a, b]), "say [secret:K] ok", "cut {cut}");
        }
    }

    #[test]
    fn a_safe_text_is_emitted_at_once() {
        let (out, held) = redactor().push(None, text("hello world"));
        assert_eq!(out, vec![text("hello world")]);
        assert!(held.is_none());
    }

    #[test]
    fn a_possible_start_is_held_and_another_kind_flushes_it_first() {
        let r = redactor();
        let (out, held) = r.push(None, text("go sk-probe"));
        assert_eq!(out, vec![text("go ")]);
        assert!(held.is_some());
        let reasoning = ExecEvent::Reasoning { text: "hm".into() };
        let (out, held) = r.push(held, reasoning.clone());
        assert_eq!(out, vec![text("sk-probe"), reasoning]);
        assert!(held.is_none());
    }

    #[test]
    fn a_match_inside_a_longer_needle_prefix_does_not_hide_its_redaction() {
        let r = ValueRedactor::new([("A", "abcdefgh"), ("B", "fghXYZWV")]);
        assert_eq!(run(&r, &["x abcdefgh y"]), "x [secret:A] y");
        assert_eq!(run(&r, &["x abcde", "fgh y"]), "x [secret:A] y");
    }

    #[test]
    fn random_chunkings_never_leak_and_lose_nothing() {
        let r = redactor();
        let whole = format!("pre {TOKEN} mid sk-probe-9 end {TOKEN}{TOKEN} sk-");
        let expect = "pre [secret:PROBE_TOKEN] mid sk-probe-9 end \
                      [secret:PROBE_TOKEN][secret:PROBE_TOKEN] sk-";
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..500 {
            let mut pieces = Vec::new();
            let mut at = 0;
            while at < whole.len() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let n = 1 + (seed % 9) as usize;
                let end = (at + n).min(whole.len());
                pieces.push(&whole[at..end]);
                at = end;
            }
            let mut held = None;
            let mut emitted = Vec::new();
            for piece in &pieces {
                let (events, next) = r.push(held.take(), text(piece));
                emitted.extend(events);
                held = next;
                assert!(!joined(&emitted).contains(TOKEN), "{pieces:?}");
            }
            if let Some(h) = held {
                emitted.push(r.flush(h));
            }
            assert_eq!(joined(&emitted), expect, "{pieces:?}");
        }
    }
}
