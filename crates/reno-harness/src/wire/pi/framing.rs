//! One stdout line of Pi as a JSON frame.
//!
//! Pi's framing is strict JSONL: one object per line, split on LF only (an optional CR before it
//! is dropped). U+2028 and U+2029 are valid inside a JSON string and are not line breaks, so a
//! reader must split on bytes, never on Unicode line separators.

use serde_json::Value;

/// The most bytes of one frame the driver keeps. A frame over it (a huge tool result) costs that
/// frame, not the run.
pub(crate) const MAX_FRAME: usize = 32 * 1024 * 1024;

/// A frame, tolerating the one thing JS emits that `serde_json` refuses: a lone UTF-16
/// surrogate escape (a string sliced mid-pair), which becomes U+FFFD instead of costing the
/// frame. `None` for a line that is not a JSON value (stray output from a dependency).
pub(crate) fn parse_frame(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok().or_else(|| {
        let repaired = replace_lone_surrogates(bytes)?;
        serde_json::from_slice(&repaired).ok()
    })
}

/// `bytes` with each unpaired `\uD800`-`\uDFFF` escape replaced by `\uFFFD`; `None` when there is
/// none to replace.
fn replace_lone_surrogates(bytes: &[u8]) -> Option<Vec<u8>> {
    fn unit(bytes: &[u8], at: usize) -> Option<u16> {
        if bytes.get(at..at + 2)? != b"\\u" {
            return None;
        }
        let hex = bytes.get(at + 2..at + 6)?;
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        match unit(bytes, at) {
            Some(0xD800..=0xDBFF) if matches!(unit(bytes, at + 6), Some(0xDC00..=0xDFFF)) => {
                out.extend_from_slice(&bytes[at..at + 12]);
                at += 12;
            }
            Some(0xD800..=0xDFFF) => {
                out.extend_from_slice(b"\\ufffd");
                changed = true;
                at += 6;
            }
            Some(_) => {
                out.extend_from_slice(&bytes[at..at + 6]);
                at += 6;
            }
            // Any other escape: keep it whole, so `\\u` stays literal text.
            None => {
                let end = (at + 2).min(bytes.len());
                out.extend_from_slice(&bytes[at..end]);
                at = end;
            }
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn surrogate_repair_keeps_pairs_and_literal_backslashes() {
        let pair = br#"{"t":"\ud83d\ude00"}"#;
        assert!(replace_lone_surrogates(pair).is_none());
        let literal = br#"{"t":"\\ud800"}"#;
        assert!(replace_lone_surrogates(literal).is_none());
        let trailing = br#"{"t":"a\udc00b\ud800"}"#;
        let repaired = replace_lone_surrogates(trailing).unwrap();
        assert_eq!(repaired, br#"{"t":"a\ufffdb\ufffd"}"#);
        assert_eq!(parse_frame(trailing).unwrap()["t"], "a\u{fffd}b\u{fffd}");
    }

    #[test]
    fn stray_output_is_not_a_frame_and_line_separators_stay_in_their_string() {
        assert!(parse_frame(b"not json").is_none());
        assert_eq!(
            parse_frame("{\"t\":\"a\u{2028}b\"}".as_bytes()).unwrap()["t"],
            "a\u{2028}b"
        );
    }
}
