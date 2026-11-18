//! Server-sent event framing over a byte stream (SPEC §5).

/// One dispatched SSE event. `event` is `None` for the default `message` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE decoder. Network chunks may split anywhere, including inside
/// a UTF-8 sequence or a line ending.
// shortcut: a lone `\r` is not a line ending and a line has no size cap; LM Studio sends `\n` and the server is user-chosen, upgrade if another server needs either.
#[derive(Debug, Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    /// Feeds bytes and returns every event they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..end]);
            rest = &rest[end + 1..];
            // Take the buffer so `finish_line` can borrow `self`, then give its capacity back.
            let mut line = std::mem::take(&mut self.line);
            events.extend(self.finish_line(line.strip_suffix(b"\r").unwrap_or(&line)));
            line.clear();
            self.line = line;
        }
        self.line.extend_from_slice(rest);
        events
    }

    fn finish_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        if line.is_empty() {
            let event = self.event.take();
            return self.data.take().map(|data| SseEvent { event, data });
        }
        // Decoding whole lines is what keeps a character split across chunks intact.
        let line = String::from_utf8_lossy(line);
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Quadratic per file, so every-offset splitting is limited to files this small.
    const EVERY_OFFSET_MAX_BYTES: usize = 16 * 1024;

    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lmstudio")
    }

    fn fixtures() -> Vec<PathBuf> {
        let mut files = Vec::new();
        for model in fs::read_dir(fixtures_root()).unwrap() {
            let dir = model.unwrap().path();
            if dir.is_dir() {
                for file in fs::read_dir(dir).unwrap() {
                    let path = file.unwrap().path();
                    if path.extension().is_some_and(|e| e == "sse") {
                        files.push(path);
                    }
                }
            }
        }
        assert!(files.len() > 50, "fixtures missing: {}", files.len());
        files
    }

    fn fixture(relative: &str) -> Vec<u8> {
        fs::read(fixtures_root().join(relative)).unwrap()
    }

    fn decode_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::default();
        chunks.into_iter().flat_map(|c| decoder.push(c)).collect()
    }

    #[test]
    fn splitting_small_fixtures_at_every_byte_offset_changes_nothing() {
        let mut checked = 0;
        for path in fixtures() {
            let bytes = fs::read(&path).unwrap();
            if bytes.len() > EVERY_OFFSET_MAX_BYTES {
                continue;
            }
            let whole = decode_chunks([bytes.as_slice()]);
            assert!(!whole.is_empty(), "{path:?}");
            for offset in 0..=bytes.len() {
                let (a, b) = bytes.split_at(offset);
                assert_eq!(decode_chunks([a, b]), whole, "{path:?} at {offset}");
            }
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} fixtures small enough");
    }

    #[test]
    fn single_byte_chunks_match_whole_input_for_every_fixture() {
        for path in fixtures() {
            let bytes = fs::read(&path).unwrap();
            let whole = decode_chunks([bytes.as_slice()]);
            assert!(!whole.is_empty(), "{path:?}");
            assert_eq!(decode_chunks(bytes.chunks(1)), whole, "{path:?}");
        }
    }

    #[test]
    fn decodes_the_http_200_context_overflow_error_event() {
        let events = decode_chunks([fixture("qwen3.5-4b/oversize_1.5.sse").as_slice()]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("error"));
        assert!(events[0].data.contains("exceed_context_size_error"));
    }

    #[test]
    fn decodes_a_chat_stream_ending_in_done() {
        let events = decode_chunks([fixture("qwen3.5-4b/tool_parallel_0.sse").as_slice()]);
        assert!(events.len() > 2);
        assert!(events.iter().all(|e| e.event.is_none()));
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));
    }

    #[test]
    fn keeps_a_multibyte_character_split_between_chunks() {
        let bytes = "data: é\n\n".as_bytes();
        let events = decode_chunks([&bytes[..7], &bytes[7..]]);
        assert_eq!(events[0].data, "é");
    }

    #[test]
    fn follows_the_sse_field_rules() {
        let input = b": comment\r\nid: 1\r\nevent: error\r\ndata:a\r\ndata:  b\r\n\r\n\ndata\n\n";
        let events = decode_chunks([input.as_slice()]);
        let expected = [
            SseEvent {
                event: Some("error".into()),
                data: "a\n b".into(),
            },
            SseEvent {
                event: None,
                data: String::new(),
            },
        ];
        assert_eq!(events, expected);
    }

    #[test]
    fn holds_back_an_event_until_its_blank_line() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: x\n").is_empty());
        assert_eq!(decoder.push(b"\n").len(), 1);
    }

    #[test]
    fn replaces_invalid_utf8_instead_of_failing() {
        let events = decode_chunks([b"data: \xff\n\n".as_slice()]);
        assert_eq!(events[0].data, "\u{fffd}");
    }
}
