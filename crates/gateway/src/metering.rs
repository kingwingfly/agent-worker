//! Reads token usage out of Anthropic Messages responses on their way through the proxy.
//!
//! The proxy is the one place that sees every model call a session makes, including
//! background requests and turns that are stopped or crash before reporting anything.

use agent_proto::Usage;
use serde_json::Value;

/// A line this long is not an event worth reading; it is skipped rather than buffered.
const MAX_LINE: usize = 1 << 20;

/// Takes the figures from a `usage` object. Each is a running total for the message, so
/// a later report replaces an earlier one.
fn merge(total: &mut Usage, reported: &Value) {
    let fields = [
        ("input_tokens", &mut total.input_tokens),
        ("output_tokens", &mut total.output_tokens),
        (
            "cache_creation_input_tokens",
            &mut total.cache_creation_input_tokens,
        ),
        (
            "cache_read_input_tokens",
            &mut total.cache_read_input_tokens,
        ),
    ];
    for (name, slot) in fields {
        if let Some(count) = reported[name].as_u64() {
            *slot = count;
        }
    }
}

/// Collects usage from a server-sent event stream, fed in whatever pieces it arrives in.
///
/// `message_start` reports the input side and `message_delta` the output so far; only
/// those two are parsed, not the many content deltas in between.
#[derive(Debug, Default)]
pub struct SseUsage {
    line: Vec<u8>,
    overlong: bool,
    usage: Usage,
}

impl SseUsage {
    pub fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte != b'\n' {
                if self.line.len() < MAX_LINE {
                    self.line.push(byte);
                } else {
                    self.overlong = true;
                }
                continue;
            }
            if !std::mem::take(&mut self.overlong) {
                self.read_line();
            }
            self.line.clear();
        }
    }

    fn read_line(&mut self) {
        let Some(data) = self.line.strip_prefix(b"data:") else {
            return;
        };
        if !data.windows(7).any(|window| window == b"\"usage\"") {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        match event["type"].as_str() {
            Some("message_start") => merge(&mut self.usage, &event["message"]["usage"]),
            Some("message_delta") => merge(&mut self.usage, &event["usage"]),
            _ => {}
        }
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }
}

/// Usage from the body of a response that was not streamed.
pub fn json_usage(body: &[u8]) -> Usage {
    let mut usage = Usage::default();
    if let Ok(message) = serde_json::from_slice::<Value>(body) {
        merge(&mut usage, &message["usage"]);
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = concat!(
        "event: message_start\r\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":25,",
        "\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":4000,\"output_tokens\":1}}}\r\n\r\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\\\"usage\\\" is a word\"}}\n\n",
        "event: ping\ndata: {\"type\": \"ping\"}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null},\"usage\":{\"output_tokens\":40}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":310}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    fn expected() -> Usage {
        Usage {
            input_tokens: 25,
            output_tokens: 310,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 4000,
        }
    }

    #[test]
    fn reads_input_from_the_start_and_output_from_the_last_delta() {
        let mut scanner = SseUsage::default();
        scanner.feed(STREAM.as_bytes());
        assert_eq!(scanner.usage(), expected());
        assert_eq!(scanner.usage().billable(), 25 + 310 + 100);
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_stream_is_cut_up() {
        for size in [1, 2, 7, 64, 1000] {
            let mut scanner = SseUsage::default();
            STREAM
                .as_bytes()
                .chunks(size)
                .for_each(|chunk| scanner.feed(chunk));
            assert_eq!(scanner.usage(), expected(), "chunks of {size}");
        }
    }

    #[test]
    fn a_stream_cut_short_still_counts_what_was_reported() {
        let mut scanner = SseUsage::default();
        let cut = STREAM.find("event: message_delta").unwrap();
        scanner.feed(&STREAM.as_bytes()[..cut]);
        assert_eq!(scanner.usage().input_tokens, 25);
        assert_eq!(scanner.usage().output_tokens, 1);
    }

    #[test]
    fn junk_and_oversized_lines_are_ignored() {
        let mut scanner = SseUsage::default();
        scanner.feed(b"data: not json \"usage\"\n: comment\n\n");
        scanner.feed(&vec![b'x'; MAX_LINE + 10]);
        scanner.feed(b"\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n");
        assert_eq!(scanner.usage().output_tokens, 7);
    }

    #[test]
    fn reads_usage_from_a_plain_json_response() {
        let body =
            br#"{"type":"message","content":[],"usage":{"input_tokens":9,"output_tokens":3}}"#;
        let usage = json_usage(body);
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 3));
        assert_eq!(json_usage(b"{\"input_tokens\": 5}"), Usage::default());
        assert_eq!(json_usage(b"<html>"), Usage::default());
    }
}
