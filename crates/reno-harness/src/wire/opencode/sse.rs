//! The `text/event-stream` framing of opencode's bus.

/// Incremental decoder (the WHATWG framing). Lines end in LF, CR or CRLF (a CR closing one chunk
/// may pair with the LF opening the next), `:` lines are comments, an event's `data` lines join
/// with LF and a blank line dispatches it. Each line decodes lossily on its own, so an invalid
/// byte costs a character rather than the event, and every byte is scanned once however large a
/// frame grows.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    /// The current block has seen a line (a comment or any field).
    in_frame: bool,
    /// The last chunk ended in CR: an LF opening the next one belongs to it.
    after_cr: bool,
    /// The first line has been read (a leading byte-order mark is dropped).
    started: bool,
    /// Complete blocks seen, comment-only ones (heartbeats) included: proof the server talks.
    pub(crate) frames: u64,
}

impl SseDecoder {
    /// Feed one chunk; returns the data of every event it completed.
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        if bytes.is_empty() {
            return events;
        }
        if std::mem::take(&mut self.after_cr) && bytes[0] == b'\n' {
            bytes = &bytes[1..];
        }
        while let Some(end) = bytes.iter().position(|b| matches!(b, b'\n' | b'\r')) {
            self.line.extend_from_slice(&bytes[..end]);
            let cr = bytes[end] == b'\r';
            bytes = &bytes[end + 1..];
            if cr {
                match bytes.first() {
                    Some(b'\n') => bytes = &bytes[1..],
                    Some(_) => {}
                    None => self.after_cr = true,
                }
            }
            self.end_line(&mut events);
        }
        self.line.extend_from_slice(bytes);
        events
    }

    fn end_line(&mut self, events: &mut Vec<String>) {
        let raw = std::mem::take(&mut self.line);
        let decoded = String::from_utf8_lossy(&raw);
        let mut line: &str = &decoded;
        if !std::mem::replace(&mut self.started, true) {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            if std::mem::take(&mut self.in_frame) {
                self.frames += 1;
            }
            if std::mem::take(&mut self.has_data) {
                events.push(std::mem::take(&mut self.data));
            }
            return;
        }
        self.in_frame = true;
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.has_data = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> (Vec<String>, u64) {
        let mut decoder = SseDecoder::default();
        let events = chunks.iter().flat_map(|c| decoder.feed(c)).collect();
        (events, decoder.frames)
    }

    #[test]
    fn a_blank_line_dispatches_and_a_comment_is_a_frame_with_no_event() {
        let (events, frames) = feed_all(&[b"data: {\"a\":1}\n\n: heartbeat\n\ndata: 2\n\n"]);
        assert_eq!(events, ["{\"a\":1}", "2"]);
        assert_eq!(frames, 3);
    }

    #[test]
    fn data_lines_join_and_every_line_ending_is_understood() {
        let (events, _) = feed_all(&[b"data: a\r\ndata: b\r\n\r\ndata: c\r\rdata: d\n\n"]);
        assert_eq!(events, ["a\nb", "c", "d"]);
    }

    #[test]
    fn a_frame_split_anywhere_decodes_the_same() {
        let whole = "\u{feff}data: {\"k\":\"v\u{e9}\"}\r\n\r\n";
        let bytes = whole.as_bytes();
        for cut in 1..bytes.len() {
            let (events, frames) = feed_all(&[&bytes[..cut], &bytes[cut..]]);
            assert_eq!(events, ["{\"k\":\"v\u{e9}\"}"], "cut at {cut}");
            assert_eq!(frames, 1);
        }
    }

    #[test]
    fn an_invalid_byte_costs_a_character_not_the_event() {
        let (events, _) = feed_all(&[b"data: a\xffb\n\n"]);
        assert_eq!(events, ["a\u{fffd}b"]);
    }
}
