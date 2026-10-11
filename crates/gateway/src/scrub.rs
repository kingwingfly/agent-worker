//! Keeps what identifies the LLM gateway, and this Worker, away from agent containers
//! and out of the chat.
//!
//! A gateway's error says who it is: the body names its site and quotes the ids it knows
//! the request by, and its response headers do the same. Claude Code prints such a body
//! into its answer word for word, and the bot shows that answer. So the proxy passes on
//! nothing a failed model call says. Every error is rebuilt here from its status and its
//! kind, in words of our own, and the original goes to the Worker's log, where only the
//! operator reads it.
//!
//! Nothing here touches the network, so it is tested on the host.

use std::borrow::Cow;

use serde_json::{Value, json};

/// The response headers of a model call that are passed on: what the body is and when to
/// try again. Every other header describes the gateway (`server`, `via`, its request
/// ids), and Claude Code quotes some of them when a call fails.
pub const KEPT_HEADERS: [&str; 5] = [
    "content-type",
    "cache-control",
    "retry-after",
    "retry-after-ms",
    "x-should-retry",
];

/// The error types of the Anthropic API. Claude Code decides by them whether to try
/// again, so one the gateway names is kept if it is one of these.
const KINDS: [&str; 10] = [
    "invalid_request_error",
    "authentication_error",
    "billing_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "timeout_error",
    "overloaded_error",
    "api_error",
];

/// Errors Claude Code knows by their wording and acts on, by repairing the conversation
/// or by telling the model what was wrong with a file. One that is found in what the
/// gateway said is passed on in these words, which are the API's and not the gateway's.
const RECOGNISED: [&str; 9] = [
    "`tool_use` ids were found without `tool_result` blocks immediately after",
    "unexpected `tool_use_id` found in `tool_result`",
    "`tool_use` ids must be unique",
    "text content blocks must be non-empty",
    "text content blocks must contain non-whitespace text",
    "image exceeds",
    "image dimensions exceed",
    "The PDF specified was not valid",
    "The PDF specified is password protected",
];

/// What a hidden host is replaced with in the chat.
const HIDDEN: &str = "[hidden]";
/// More of an event stream's line or event than this is not kept in memory to be read.
const MAX_HELD: usize = 1 << 16;

fn kind_of_status(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 => "overloaded_error",
        500.. => "api_error",
        _ => "invalid_request_error",
    }
}

/// What the user is told about an error of this kind.
fn words(kind: &str) -> &'static str {
    match kind {
        "invalid_request_error" => "The model service rejected the request.",
        "authentication_error" | "billing_error" | "permission_error" => {
            "The model service refused the request."
        }
        "not_found_error" => "The model service could not find what was asked for.",
        "request_too_large" => "The request was too large for the model service.",
        "rate_limit_error" => "The model service is limiting requests right now.",
        "timeout_error" => "The model service took too long to answer.",
        "overloaded_error" => "The model service is overloaded right now.",
        _ => "The model service had an internal error.",
    }
}

/// Reads `text` against `shape`, in which `#` is a number of at most nine digits, a
/// space is any amount of white space and every other character is itself. Returns the
/// numbers, or `None` if the text does not begin that way.
fn numbers(text: &str, shape: &str) -> Option<Vec<u64>> {
    let mut rest = text;
    let mut found = Vec::new();
    for part in shape.chars() {
        match part {
            ' ' => rest = rest.trim_start(),
            '#' => {
                let digits =
                    rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                if !(1..=9).contains(&digits) {
                    return None;
                }
                found.push(rest[..digits].parse().ok()?);
                rest = &rest[digits..];
            }
            literal => rest = rest.strip_prefix(literal)?,
        }
    }
    Some(found)
}

/// The API's own wording of the error in `said`, if it is one Claude Code acts on.
///
/// The two about the size of the conversation carry figures that Claude Code reads to
/// make room. They are written out again from the figures alone, and only when those
/// stand exactly where the API puts them, so that nothing else can ride along.
fn recognised(said: &str) -> Option<String> {
    let after = |phrase: &str| said.split_once(phrase).map(|(_, rest)| rest);
    if let Some(rest) = after("prompt is too long") {
        return Some(match numbers(rest, ": # tokens > #").as_deref() {
            Some([prompt, limit]) => {
                format!("prompt is too long: {prompt} tokens > {limit} maximum")
            }
            _ => "prompt is too long".to_owned(),
        });
    }
    if let Some(rest) = after("exceed context limit") {
        let mut message = "input length and `max_tokens` exceed context limit".to_owned();
        if let Some([input, output, limit]) = numbers(rest, ": # + # > #").as_deref() {
            message.push_str(&format!(
                ": {input} + {output} > {limit}, decrease input length or `max_tokens` and try again"
            ));
        }
        return Some(message);
    }
    RECOGNISED
        .into_iter()
        .find(|phrase| said.contains(phrase))
        .map(str::to_owned)
}

/// The body sent in place of a failed model call's own, in the shape of an Anthropic API
/// error. `status` is the response's, or `None` for an error reported inside an event
/// stream. `upstream` is only looked at for the kind of error and for wording Claude
/// Code acts on; none of its text is passed on.
pub fn error_body(status: Option<u16>, upstream: &[u8]) -> Value {
    let body: Value = serde_json::from_slice(upstream).unwrap_or(Value::Null);
    let error = &body["error"];
    let kind = KINDS
        .into_iter()
        .find(|kind| error["type"] == *kind)
        .unwrap_or_else(|| status.map_or("api_error", kind_of_status));
    // Gateways differ in where they put the message, and some send no JSON at all.
    let said = [&error["message"], error, &body["message"], &body["detail"]]
        .into_iter()
        .find_map(Value::as_str)
        .map_or_else(|| String::from_utf8_lossy(upstream), Cow::Borrowed);
    let message = recognised(&said).unwrap_or_else(|| words(kind).to_owned());
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

/// Whether the body of a response that claims success is one to pass on: nothing at all,
/// or a JSON object that is not an error. A gateway that reports a failure with status
/// 200, or answers with a page of its own, has failed like any other.
pub fn is_answer(body: &[u8]) -> bool {
    if body.iter().all(u8::is_ascii_whitespace) {
        return true;
    }
    match serde_json::from_slice(body) {
        Ok(Value::Object(fields)) => {
            fields.get("type").is_none_or(|kind| kind != "error")
                && fields.get("error").is_none_or(Value::is_null)
        }
        _ => false,
    }
}

/// How far the event being read has been made out.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// Its name has not come yet, so its lines are held back.
    #[default]
    Unnamed,
    /// It is not an error, and is passed on line by line.
    Passed,
    /// It is an error. Its lines are kept to be read and replaced when it ends.
    Error,
}

/// Replaces the `error` events of a server-sent event stream on its way through, fed in
/// whatever pieces it arrives in. Every other event is passed on byte for byte.
///
/// An event is an error by its `event:` field, which is also how the client tells. The
/// field normally comes first, so nothing but an error is held back for longer than a
/// line takes to arrive.
#[derive(Debug, Default)]
pub struct SseScrub {
    /// The line being read, as far as it has arrived.
    line: Vec<u8>,
    /// The rest of the line being read is not kept: it grew too long.
    skipping: bool,
    /// The lines of the event being read that have not been passed on.
    held: Vec<u8>,
    event: Event,
    /// The errors that were replaced, as the gateway sent them, for the log.
    pub replaced: Vec<String>,
}

impl SseScrub {
    /// Takes the next piece of the stream and returns what may be sent on, which is
    /// nothing while a line or an unnamed event is still incomplete.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            let ended = piece.ends_with(b"\n");
            if self.skipping {
                if self.event != Event::Error {
                    out.extend_from_slice(piece);
                }
                self.skipping = !ended;
                continue;
            }
            self.line.extend_from_slice(piece);
            if ended {
                let line = std::mem::take(&mut self.line);
                self.take_line(line, &mut out);
            } else if self.line.len() > MAX_HELD {
                // Too long to be a field this looks at. Of an error, its start is enough
                // to replace it by; anything else goes on its way.
                self.skipping = true;
                let line = std::mem::take(&mut self.line);
                if self.event == Event::Error {
                    self.hold(line);
                } else {
                    self.event = Event::Passed;
                    out.append(&mut self.held);
                    out.extend(line);
                }
            }
        }
        out
    }

    /// What is left to send when the stream has ended, if it ended inside an event.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.take_line(line, &mut out);
        }
        self.end_event(&mut out);
        out
    }

    fn take_line(&mut self, line: Vec<u8>, out: &mut Vec<u8>) {
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.is_empty() {
            // A blank line ends the event.
            if self.event != Event::Error {
                self.held.extend(line);
            }
            return self.end_event(out);
        }
        let named_error = text
            .strip_prefix(b"event:")
            .map(|name| name.trim_ascii() == b"error");
        match (self.event, named_error) {
            (Event::Passed, _) => out.extend(line),
            (Event::Unnamed, Some(false)) => {
                self.event = Event::Passed;
                out.append(&mut self.held);
                out.extend(line);
            }
            (Event::Unnamed, Some(true)) => {
                self.event = Event::Error;
                self.hold(line);
            }
            (Event::Error, _) => self.hold(line),
            (Event::Unnamed, None) => {
                self.hold(line);
                if self.held.len() > MAX_HELD {
                    self.event = Event::Passed;
                    out.append(&mut self.held);
                }
            }
        }
    }

    fn hold(&mut self, line: Vec<u8>) {
        if self.held.len() <= MAX_HELD {
            self.held.extend(line);
        }
    }

    /// Sends on what is held of the event that is over, or its replacement.
    fn end_event(&mut self, out: &mut Vec<u8>) {
        let held = std::mem::take(&mut self.held);
        if std::mem::take(&mut self.event) != Event::Error {
            out.extend(held);
            return;
        }
        let data: Vec<&[u8]> = held
            .split(|byte| *byte == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .filter_map(|line| line.strip_prefix(b"data:"))
            .collect();
        let error = error_body(None, &data.join(&b'\n'));
        out.extend(format!("event: error\ndata: {error}\n\n").into_bytes());
        self.replaced
            .push(String::from_utf8_lossy(&held).into_owned());
    }
}

/// The host a URL names, with its port if it has one.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Takes every mention of `host` out of `text`, however it is capitalised.
///
/// Claude Code ends some of its error messages by naming the host it calls, which is
/// this Worker. The bot runs everything it says through this, so that the Worker's
/// address is not shown whatever message it turns up in. The gateway's needs no such
/// care: nothing that names it gets as far as a container.
pub fn redact<'a>(text: &'a str, host: &str) -> Cow<'a, str> {
    // ASCII lowering keeps every byte where it was, so positions hold in `text`.
    let (lowered, host) = (text.to_ascii_lowercase(), host.to_ascii_lowercase());
    if host.is_empty() || !lowered.contains(&host) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for (at, _) in lowered.match_indices(&host) {
        out.push_str(&text[done..at]);
        out.push_str(HIDDEN);
        done = at + host.len();
    }
    out.push_str(&text[done..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLURB: &str = "Report the issue: contact https://support.acme-gateway.example \
                         (request_id: 0199c2f4-7b1e-7d3a-9c55-2f0e8a6b1c44, session: sess_8f3k2j)";

    /// Asserts that nothing of the gateway's blurb is left in `text`.
    fn assert_clean(text: &str) {
        for leak in [
            "acme",
            "Report",
            "contact",
            "request_id",
            "0199c2f4",
            "sess_",
        ] {
            assert!(!text.contains(leak), "{leak:?} is left in {text}");
        }
    }

    fn body(kind: &str, message: &str) -> Vec<u8> {
        json!({ "type": "error", "error": { "type": kind, "message": message }, "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy" })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn an_error_is_rebuilt_from_its_status_and_kind_alone() {
        let rebuilt = error_body(Some(500), &body("server_error", BLURB));
        assert_eq!(
            rebuilt,
            json!({ "type": "error", "error": {
                "type": "api_error",
                "message": "The model service had an internal error.",
            } })
        );
        assert_clean(&rebuilt.to_string());

        // A kind the API has is kept, whatever the status; others follow the status.
        let kind = |status, upstream: &[u8]| error_body(status, upstream)["error"]["type"].clone();
        assert_eq!(
            kind(Some(400), &body("overloaded_error", BLURB)),
            "overloaded_error"
        );
        assert_eq!(
            kind(Some(429), &body("acme_quota", BLURB)),
            "rate_limit_error"
        );
        assert_eq!(kind(Some(529), b""), "overloaded_error");
        assert_eq!(kind(Some(418), b""), "invalid_request_error");
        assert_eq!(
            kind(None, &body("overloaded_error", BLURB)),
            "overloaded_error"
        );
        assert_eq!(kind(None, b"{}"), "api_error");

        // Claude Code looks for this very spelling in a body to tell an overload.
        let overloaded = error_body(Some(529), b"").to_string();
        assert!(
            overloaded.contains(r#""type":"overloaded_error""#),
            "{overloaded}"
        );
    }

    #[test]
    fn whatever_shape_the_gateway_answers_in_none_of_its_text_is_passed_on() {
        let shapes: [Vec<u8>; 6] = [
            json!({ "error": { "message": BLURB, "type": "server_error", "code": "upstream" } })
                .to_string()
                .into_bytes(),
            json!({ "error": BLURB }).to_string().into_bytes(),
            json!({ "message": BLURB, "request_id": "0199c2f4" })
                .to_string()
                .into_bytes(),
            json!({ "detail": BLURB }).to_string().into_bytes(),
            BLURB.as_bytes().to_vec(),
            format!("<html><title>acme-gateway</title><body>{BLURB}</body></html>").into_bytes(),
        ];
        for shape in shapes {
            let rebuilt = error_body(Some(502), &shape).to_string();
            assert_clean(&rebuilt);
            assert!(
                rebuilt.contains("The model service had an internal error."),
                "{rebuilt}"
            );
        }
    }

    #[test]
    fn errors_claude_code_acts_on_keep_the_wording_of_the_api_without_the_gateways() {
        let message = |said: &str| {
            let rebuilt = error_body(Some(400), &body("invalid_request_error", said));
            assert_clean(&rebuilt.to_string());
            rebuilt["error"]["message"].as_str().unwrap().to_owned()
        };
        assert_eq!(
            message(&format!(
                "prompt is too long: 205431 tokens > 200000 maximum. {BLURB}"
            )),
            "prompt is too long: 205431 tokens > 200000 maximum"
        );
        assert_eq!(
            message(&format!(
                "acme: input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, \
                 decrease input length or `max_tokens` and try again ({BLURB})"
            )),
            "input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, \
             decrease input length or `max_tokens` and try again"
        );
        assert_eq!(
            message(&format!(
                "messages.3: `tool_use` ids were found without `tool_result` blocks immediately \
                 after: toolu_01A09q90qw90lq917835lq9. {BLURB}"
            )),
            "`tool_use` ids were found without `tool_result` blocks immediately after"
        );

        // Figures are only taken from where the API puts them: an id is not a figure.
        assert_eq!(
            message("prompt is too long (request_id: 20261011 tokens > 7)"),
            "prompt is too long"
        );
        assert_eq!(
            message("prompt is too long: 1234567890123 tokens > 200000 maximum"),
            "prompt is too long"
        );
        assert_eq!(
            message("exceed context limit, see request 0199c2f4"),
            "input length and `max_tokens` exceed context limit"
        );
        assert_eq!(message(BLURB), "The model service rejected the request.");
    }

    #[test]
    fn a_success_that_is_an_error_or_a_page_is_not_an_answer() {
        assert!(is_answer(b""));
        assert!(is_answer(b" \n"));
        assert!(is_answer(
            br#"{"type":"message","content":[],"usage":{"input_tokens":9}}"#
        ));
        assert!(is_answer(br#"{"input_tokens":12}"#));
        assert!(is_answer(br#"{"data":[],"error":null}"#));

        assert!(!is_answer(&body("api_error", BLURB)));
        assert!(!is_answer(br#"{"error":{"message":"contact acme"}}"#));
        assert!(!is_answer(br#"{"error":"contact acme"}"#));
        assert!(!is_answer(
            b"<html>acme-gateway is down for maintenance</html>"
        ));
        assert!(!is_answer(br#""contact acme""#));
    }

    /// An event stream as the API sends it, with an error of the gateway's in the middle.
    fn stream(line_end: &str) -> (String, String) {
        let error = String::from_utf8(body("overloaded_error", BLURB)).unwrap();
        let before = [
            "event: message_start",
            r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":25}}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"an \"error\" is a word"}}"#,
            "",
            ": a comment, then an event without a name",
            "data: {\"type\":\"ping\"}",
            "",
        ]
        .map(|line| format!("{line}{line_end}"))
        .concat();
        let failing = format!("event: error{line_end}data: {error}{line_end}{line_end}");
        let after =
            format!("event: ping{line_end}data: {{\"type\": \"ping\"}}{line_end}{line_end}");
        let replaced = format!(
            "event: error\ndata: {}\n\n",
            json!({ "type": "error", "error": {
                "type": "overloaded_error",
                "message": "The model service is overloaded right now.",
            } })
        );
        (
            format!("{before}{failing}{after}"),
            format!("{before}{replaced}{after}"),
        )
    }

    fn scrubbed(stream: &[u8], size: usize) -> (String, Vec<String>) {
        let mut scrub = SseScrub::default();
        let mut out = Vec::new();
        for chunk in stream.chunks(size) {
            out.extend(scrub.feed(chunk));
        }
        out.extend(scrub.finish());
        (String::from_utf8(out).unwrap(), scrub.replaced)
    }

    #[test]
    fn an_error_inside_a_stream_is_replaced_and_the_rest_is_passed_on_as_it_came() {
        for line_end in ["\n", "\r\n"] {
            let (sent, expected) = stream(line_end);
            for size in [1, 2, 7, 64, 100_000] {
                let (out, replaced) = scrubbed(sent.as_bytes(), size);
                assert_eq!(out, expected, "chunks of {size}");
                assert_clean(&out);
                // The operator still gets to read what the gateway said.
                let [original] = replaced.as_slice() else {
                    panic!("one error was replaced, not {replaced:?}");
                };
                assert!(original.contains("request_id: 0199c2f4"), "{original}");
            }
        }
    }

    #[test]
    fn a_stream_without_errors_comes_out_byte_for_byte() {
        let (sent, _) = stream("\n");
        let healthy = sent.split("event: error").next().unwrap();
        // Cut off in the middle of an event, as a broken connection leaves it.
        for cut in [healthy.len(), healthy.len() - 3, 30] {
            for size in [1, 5, 4096] {
                let (out, replaced) = scrubbed(&healthy.as_bytes()[..cut], size);
                assert_eq!(out, healthy[..cut], "cut at {cut}, chunks of {size}");
                assert!(replaced.is_empty());
            }
        }
    }

    #[test]
    fn an_error_is_replaced_wherever_its_name_stands_and_however_it_ends() {
        // The name after the data, which the format allows.
        let (out, _) = scrubbed(format!("data: {BLURB}\nevent:error\n\n").as_bytes(), 9);
        assert!(out.starts_with("event: error\ndata: {"), "{out}");
        assert_clean(&out);
        // The stream ends before the event does.
        let (out, replaced) = scrubbed(format!("event: error\ndata: {BLURB}").as_bytes(), 9);
        assert!(out.starts_with("event: error\ndata: {"), "{out}");
        assert_clean(&out);
        assert_eq!(replaced.len(), 1);
        // An error too long to keep is replaced all the same.
        let long = format!(
            "event: error\ndata: {BLURB}{}\n\nevent: ping\ndata: {{}}\n\n",
            "x".repeat(3 * MAX_HELD)
        );
        let (out, _) = scrubbed(long.as_bytes(), 1000);
        assert!(out.starts_with("event: error\ndata: {"), "{out}");
        assert!(out.ends_with("}\n\nevent: ping\ndata: {}\n\n"), "{out}");
        assert_clean(&out);
        assert!(out.len() < 300);
    }

    #[test]
    fn long_lines_of_other_events_are_passed_on_without_being_kept() {
        let long = format!(
            "event: content_block_delta\ndata: {}\n\ndata: {}\n\nevent: ping\ndata: {{}}\n\n",
            "a".repeat(3 * MAX_HELD),
            "b".repeat(3 * MAX_HELD),
        );
        for size in [1000, 70_000] {
            let mut scrub = SseScrub::default();
            let mut out = Vec::new();
            for chunk in long.as_bytes().chunks(size) {
                out.extend(scrub.feed(chunk));
                assert!(scrub.line.len() + scrub.held.len() <= 2 * MAX_HELD + size);
            }
            out.extend(scrub.finish());
            assert_eq!(String::from_utf8(out).unwrap(), long, "chunks of {size}");
        }
    }

    #[test]
    fn the_workers_host_is_taken_out_of_what_the_bot_says() {
        let host = host_of("https://agent.example.workers.dev");
        assert_eq!(host, "agent.example.workers.dev");
        assert_eq!(host_of("http://127.0.0.1:8787/llm?x=1"), "127.0.0.1:8787");
        assert_eq!(host_of("agent.example.com/"), "agent.example.com");

        assert_eq!(
            redact(
                "If it persists, check your inference gateway (agent.example.workers.dev). \
                 See https://Agent.Example.workers.dev/llm.",
                host
            ),
            "If it persists, check your inference gateway ([hidden]). See https://[hidden]/llm."
        );
        assert_eq!(
            redact("é agent.example.workers.dev é", host),
            "é [hidden] é"
        );
        let untouched = "Nothing to hide in example.workers.dev — 你好.";
        assert!(matches!(redact(untouched, host), Cow::Borrowed(_)));
        assert!(matches!(redact(untouched, ""), Cow::Borrowed(_)));
    }
}
