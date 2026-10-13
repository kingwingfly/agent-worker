//! Everything the bot says, as text: Claude's Markdown turned into the HTML subset
//! Telegram understands and cut into messages that fit, the live status line, permission
//! prompts with their buttons, and the callback data those buttons carry. Also what a
//! guest summon is turned into for the agent, and the one message it gets back.
//!
//! Nothing here touches the network, so it is tested on the host.

use agent_proto::Effort;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde_json::{Value, json};

use crate::{
    remember::{self, Scope},
    store::{Checkpoint, Conversation},
};

/// Telegram's limit is 4096 UTF-16 code units of text after entity parsing. Measuring the
/// HTML source instead, with some slack, is always on the safe side.
const MESSAGE_BUDGET: usize = 3900;
/// The limit itself, for a message put together from parts that were measured.
const MESSAGE_LIMIT: usize = 4096;
/// How much of a guest's one message may go to notes about files and failures.
const NOTES_BUDGET: usize = 1000;
/// Starts the callback data of a button on a guest's reply, ahead of what the button
/// would carry anywhere else. No other kind of data starts with it.
const GUEST_PREFIX: char = 'g';
/// Goes at the end of an answer that went on past the one message a guest gets.
const CUT: &str = "✂️ <i>The rest did not fit: where I am a guest, I get one message.</i>";
/// How much of a conversation's id a button of the `/rewind` list carries. On a guest's
/// reply the button names its chat as well, and the whole id would not fit beside that.
const CONVERSATION_TAG_CHARS: usize = 8;
/// Starts the line that says which message a guest's summons replies to.
const REPLIES_TO: &str = "[It replies to this message";
/// Goes ahead of what a guest is asked. The agent cannot tell from anything else that
/// this turn's answer is cut after one message.
const GUEST_NOTE: &str = "[Sent from a chat where you are a guest. Your reply must be a \
                          single message under 3000 characters.]";
const STATUS_TEXT_TAIL: usize = 1200;
const STATUS_TOOL_LINES: usize = 6;
const PROMPT_INPUT_LIMIT: usize = 1500;
/// The name of the file a request too long for its prompt is sent in.
pub const REQUEST_FILE: &str = "request.txt";
/// How much of a message a button of the `/resume` or `/rewind` list shows.
const PICK_LABEL_CHARS: usize = 36;
/// How much of a message is quoted back once it has been picked.
const PICKED_TEXT_LIMIT: usize = 1500;
/// What an effort setting is called while it is left to the model, as in Claude Code.
const AUTO_EFFORT: &str = "auto";
/// Starts the callback data of an `/effort` button, as `p`, `q`, `s` and `r` start the
/// other kinds.
const EFFORT_PREFIX: &str = "e:";

/// Headings of the progress message: while a turn runs, and for each way it can end.
pub const WORKING: &str = "⏳ Working…";
pub const DONE: &str = "✅ Done";
pub const STOPPED: &str = "⏹ Stopped";
pub const FAILED: &str = "⚠️ Stopped early";

/// How much is shown of what a failed turn said: of each of its lines, and in all. A
/// process that crashes can print pages, with a line of the program it crashed in that
/// is longer than a message may be, and a message Telegram refuses for its length tells
/// the chat nothing at all.
const FAILURE_LINE_CHARS: usize = 300;
const FAILURE_CHARS: usize = 1500;

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_attribute(text: &str) -> String {
    escape(text).replace('"', "&quot;")
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Cuts `text` to at most `limit` characters, marking the cut.
fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

/// Removes tags and decodes entities from HTML produced by this module, for the rare case
/// where Telegram refuses the markup and the message is re-sent as plain text.
pub fn to_plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Renders Markdown as a list of self-contained HTML blocks, one per top-level element.
fn markdown_blocks(markdown: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut blocks = Vec::new();
    let mut html = String::new();
    let mut depth = 0usize;
    // One entry per open list: the next number for ordered lists.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut code_block = false;
    // Telegram allows no markup inside `<pre>`, which is how tables are shown.
    let mut in_table = false;
    // The item marker was just written and nothing has followed it yet.
    let mut item_opened = false;

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => {
                depth += 1;
                // In a list whose items are separated by blank lines, each item's text is
                // a paragraph. It belongs on the line its marker is on, not the next one.
                let opens_item_text = item_opened && matches!(tag, Tag::Paragraph);
                if matches!(tag, Tag::Paragraph | Tag::Item)
                    && !opens_item_text
                    && !html.is_empty()
                    && !html.ends_with('\n')
                {
                    html.push('\n');
                }
                item_opened = matches!(tag, Tag::Item);
                match tag {
                    Tag::Heading { .. } => html.push_str("<b>"),
                    Tag::BlockQuote(_) => html.push_str("<blockquote>"),
                    Tag::CodeBlock(kind) => {
                        code_block = true;
                        match kind {
                            CodeBlockKind::Fenced(language) if !language.is_empty() => {
                                let language = language.split_whitespace().next().unwrap_or("");
                                html.push_str(&format!(
                                    "<pre><code class=\"language-{}\">",
                                    escape_attribute(language)
                                ));
                            }
                            _ => html.push_str("<pre><code>"),
                        }
                    }
                    Tag::List(start) => lists.push(start),
                    Tag::Item => {
                        html.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                        match lists.last_mut() {
                            Some(Some(number)) => {
                                html.push_str(&format!("{number}. "));
                                *number += 1;
                            }
                            _ => html.push_str("• "),
                        }
                    }
                    Tag::Table(_) => {
                        in_table = true;
                        html.push_str("<pre>");
                    }
                    Tag::TableCell if !html.ends_with('\n') && !html.ends_with("<pre>") => {
                        html.push_str(" | ");
                    }
                    Tag::Emphasis if !in_table => html.push_str("<i>"),
                    Tag::Strong if !in_table => html.push_str("<b>"),
                    Tag::Strikethrough if !in_table => html.push_str("<s>"),
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } if !in_table => {
                        html.push_str(&format!("<a href=\"{}\">", escape_attribute(&dest_url)));
                    }
                    _ => {}
                }
            }
            Event::End(tag) => {
                depth = depth.saturating_sub(1);
                match tag {
                    TagEnd::Heading(_) => html.push_str("</b>"),
                    TagEnd::BlockQuote(_) => html.push_str("</blockquote>"),
                    TagEnd::CodeBlock => {
                        code_block = false;
                        html.truncate(html.trim_end_matches('\n').len());
                        html.push_str("</code></pre>");
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                    }
                    TagEnd::Table => {
                        in_table = false;
                        html.truncate(html.trim_end_matches('\n').len());
                        html.push_str("</pre>");
                    }
                    TagEnd::TableHead | TagEnd::TableRow => html.push('\n'),
                    TagEnd::Emphasis if !in_table => html.push_str("</i>"),
                    TagEnd::Strong if !in_table => html.push_str("</b>"),
                    TagEnd::Strikethrough if !in_table => html.push_str("</s>"),
                    TagEnd::Link | TagEnd::Image if !in_table => html.push_str("</a>"),
                    _ => {}
                }
                if depth == 0 && !html.trim().is_empty() {
                    blocks.push(Block {
                        html: std::mem::take(&mut html).trim_end().to_owned(),
                        preformatted: matches!(tag, TagEnd::CodeBlock | TagEnd::Table),
                    });
                }
            }
            Event::Text(text) => {
                item_opened = false;
                html.push_str(&escape(&text));
            }
            Event::Code(code) if code_block || in_table => html.push_str(&escape(&code)),
            Event::Code(code) => html.push_str(&format!("<code>{}</code>", escape(&code))),
            Event::Html(raw) | Event::InlineHtml(raw) => html.push_str(&escape(&raw)),
            Event::SoftBreak | Event::HardBreak => html.push('\n'),
            Event::Rule => blocks.push(Block {
                html: "———".into(),
                preformatted: false,
            }),
            Event::TaskListMarker(done) => html.push_str(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }
    if !html.trim().is_empty() {
        blocks.push(Block {
            html: html.trim_end().to_owned(),
            preformatted: false,
        });
    }
    blocks
}

struct Block {
    html: String,
    /// Shown in a monospace box; keeps that when it has to be split.
    preformatted: bool,
}

/// Splits text into pieces of at most `budget` UTF-16 units, at line breaks where it can.
/// The text is HTML without tags, so a cut must not land inside an entity.
fn split_text(text: &str, budget: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if units(&current) + units(line) <= budget {
            current.push_str(line);
            continue;
        }
        if !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
        }
        let mut rest = line;
        while units(rest) > budget {
            let mut cut = rest
                .char_indices()
                .scan(0, |used, (index, c)| {
                    *used += c.len_utf16();
                    (*used <= budget).then_some(index + c.len_utf8())
                })
                .last()
                .unwrap_or(rest.len());
            if let Some(entity) = rest[..cut].rfind('&')
                && !rest[entity..cut].contains(';')
                && entity > 0
            {
                cut = entity;
            }
            pieces.push(rest[..cut].to_owned());
            rest = &rest[cut..];
        }
        current.push_str(rest);
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Claude's answer as one or more Telegram messages in HTML.
pub fn answer(markdown: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, messages: &mut Vec<String>| {
        if !current.is_empty() {
            messages.push(std::mem::take(current));
        }
    };
    for block in markdown_blocks(markdown) {
        if units(&block.html) > MESSAGE_BUDGET {
            // Too big for one message. Markup cannot be cut safely, so it is dropped.
            flush(&mut current, &mut messages);
            let text = strip_tags(&block.html);
            for piece in split_text(&text, MESSAGE_BUDGET - 16) {
                let piece = piece.trim_end_matches('\n');
                messages.push(if block.preformatted {
                    format!("<pre>{piece}</pre>")
                } else {
                    piece.to_owned()
                });
            }
        } else if units(&current) + units(&block.html) + 2 > MESSAGE_BUDGET {
            flush(&mut current, &mut messages);
            current = block.html;
        } else {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&block.html);
        }
    }
    flush(&mut current, &mut messages);
    messages
}

/// What a chat is told about a turn that ended in an error: which kind it was and, if
/// the agent or its runner said more, the start of that. Lines are cut before the whole
/// is, so that one long line does not push out what the next ones say.
pub fn failure(subtype: &str, detail: Option<&str>) -> String {
    let mut message = format!("⚠️ The agent stopped early ({}).", escape(subtype));
    if let Some(detail) = detail {
        let lines: Vec<String> = detail
            .lines()
            .map(|line| truncate(line, FAILURE_LINE_CHARS))
            .collect();
        let shown = truncate(&lines.join("\n"), FAILURE_CHARS);
        message.push_str(&format!("\n<pre>{}</pre>", escape(&shown)));
    }
    message
}

/// The text Claude is writing, as a draft shows it while it grows: the last of the
/// messages it would be sent as if it ended here, which is the one being written.
/// `None` while there is nothing to show. Markdown that breaks off in the middle renders
/// like any other: what is still open is closed at the end.
pub fn draft(markdown: &str) -> Option<String> {
    answer(markdown).pop()
}

/// All there is to say about a turn in one message, which is what a guest gets: the
/// first of the `messages` the answer would be sent as elsewhere, a line saying so if
/// there were more, and the `notes` on what became of files or went wrong.
pub fn one_message(messages: &[String], notes: &[String]) -> String {
    let mut kept = Vec::new();
    let mut used = 0;
    for note in notes {
        used += units(note) + 1;
        if used > NOTES_BUDGET {
            kept.push("…");
            break;
        }
        kept.push(note.as_str());
    }
    let notes = kept.join("\n");
    let compose = |answer: &str, cut: bool| {
        [answer, if cut { CUT } else { "" }, &notes]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let first = messages.first().map_or("", String::as_str);
    let whole = compose(first, messages.len() > 1);
    if units(&whole) <= MESSAGE_LIMIT {
        return whole;
    }
    // The notes need more than the slack a message of the answer leaves. Markup cannot
    // be cut safely, so the answer loses it along with its end.
    let room = MESSAGE_LIMIT.saturating_sub(units(&compose("", true)) + 2);
    let text = strip_tags(first);
    let start = split_text(&text, room).into_iter().next();
    compose(start.unwrap_or_default().trim_end(), true)
}

/// A message without the mentions it begins with. A guest is summoned by one, so
/// `@the_bot /stop` is the command `/stop`, and the name the bot goes by is no part of
/// what it is asked.
pub fn without_mentions(text: &str) -> &str {
    let mut rest = text.trim();
    while rest.starts_with('@') {
        rest = rest
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim_start());
    }
    rest
}

/// What a guest is asked, as the agent gets it. The message it replies to, by `author`
/// if that is known, goes ahead of it: that is usually what the question is about
/// ("summarise this"), and the agent sees nothing else of the chat.
pub fn guest_request(text: &str, replied: Option<(Option<&str>, &str)>) -> String {
    let mut request = String::from(GUEST_NOTE);
    if let Some((author, words)) = replied {
        request.push_str("\n\n");
        request.push_str(REPLIES_TO);
        if let Some(author) = author {
            request.push_str(&format!(" from {author}"));
        }
        request.push_str(":]");
        for line in words.lines() {
            request.push_str("\n> ");
            request.push_str(line);
        }
    }
    if !text.is_empty() {
        request.push_str("\n\n");
        request.push_str(text);
    }
    request
}

/// What the user wrote, out of a message as a turn was given it. A guest's turn is given
/// more, see [`guest_request`]: a line for the agent and the message that was replied
/// to. Neither is what the lists of `/resume` and `/rewind` should call the message by.
/// A summons that said nothing itself is called by the message it replied to.
fn asked(request: &str) -> String {
    let Some(rest) = request.strip_prefix(GUEST_NOTE) else {
        return request.to_owned();
    };
    let rest = rest.trim_start_matches('\n');
    let Some(reply) = rest.strip_prefix(REPLIES_TO) else {
        return rest.to_owned();
    };
    // The rest of the line that says whose message it is, that message line by line,
    // an empty line, and then the user's own words.
    let mut lines = reply.lines().skip(1);
    let quoted: Vec<&str> = lines
        .by_ref()
        .map_while(|line| line.strip_prefix("> "))
        .collect();
    let words = lines.collect::<Vec<_>>().join("\n");
    if words.is_empty() {
        quoted.join("\n")
    } else {
        words
    }
}

/// Removes tags but leaves entities, so the result is still valid HTML text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// One line describing a tool call, for the status message and for prompts.
fn tool_summary(name: &str, input: &Value) -> String {
    let detail = [
        "command",
        "file_path",
        "path",
        "pattern",
        "skill",
        "url",
        "query",
    ]
    .iter()
    .find_map(|key| input.get(key)?.as_str())
    .map(|value| value.lines().next().unwrap_or_default())
    .unwrap_or_default();
    let name = name
        .strip_prefix("mcp__")
        .unwrap_or(name)
        .replace("__", " › ");
    if detail.is_empty() {
        name
    } else {
        format!("{name}: {}", truncate(detail, 80))
    }
}

/// A turn's progress, built up from Claude's stream: the tools it used and what it said
/// along the way. It is shown while the turn runs and stays in the chat afterwards.
#[derive(Debug, Default)]
pub struct Status {
    tools: Vec<String>,
    /// Tool calls that no longer fit in `tools`.
    earlier: usize,
    /// What Claude last said before using a tool.
    said: String,
    /// What Claude said last. Unless a tool call follows, this is the answer, which is
    /// sent as a message of its own and so is not shown here.
    latest: String,
}

impl Status {
    /// Takes in one stream-json event. Returns whether the status changed.
    pub fn observe(&mut self, event: &Value) -> bool {
        if event["type"] != "assistant" {
            return false;
        }
        let mut changed = false;
        for block in event["message"]["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                        self.latest = text.to_owned();
                    }
                }
                Some("tool_use") => {
                    if !self.latest.is_empty() {
                        self.said = std::mem::take(&mut self.latest);
                    }
                    let name = block["name"].as_str().unwrap_or("tool");
                    self.tools.push(tool_summary(name, &block["input"]));
                    if self.tools.len() > STATUS_TOOL_LINES {
                        self.tools.remove(0);
                        self.earlier += 1;
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    /// Whether there is nothing to show but the heading.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.said.is_empty()
    }

    pub fn render(&self, heading: &str) -> String {
        let mut html = String::from(heading);
        if self.earlier > 0 {
            html.push_str(&format!("\n<i>… {} earlier</i>", self.earlier));
        }
        for tool in &self.tools {
            html.push_str(&format!("\n🔧 <code>{}</code>", escape(tool)));
        }
        if !self.said.is_empty() {
            let skip = self.said.chars().count().saturating_sub(STATUS_TEXT_TAIL);
            let tail: String = self.said.chars().skip(skip).collect();
            html.push_str("\n\n");
            if skip > 0 {
                html.push('…');
            }
            html.push_str(&escape(&tail));
        }
        html
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Allow,
    AllowAlways,
    Deny,
}

/// What a pressed inline button means. Telegram limits callback data to 64 bytes; a
/// request id is a 36 character UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Permission {
        request_id: String,
        choice: Choice,
    },
    Answer {
        request_id: String,
        question: usize,
        option: usize,
    },
}

impl Callback {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Permission { request_id, .. } | Self::Answer { request_id, .. } => request_id,
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Self::Permission { request_id, choice } => {
                let code = match choice {
                    Choice::Allow => 'a',
                    Choice::AllowAlways => 'w',
                    Choice::Deny => 'd',
                };
                format!("p:{request_id}:{code}")
            }
            Self::Answer {
                request_id,
                question,
                option,
            } => format!("q:{request_id}:{question}:{option}"),
        }
    }

    pub fn parse(data: &str) -> Option<Self> {
        let mut parts = data.split(':');
        let (kind, request_id) = (parts.next()?, parts.next()?.to_owned());
        let callback = match kind {
            "p" => Self::Permission {
                request_id,
                choice: match parts.next()? {
                    "a" => Choice::Allow,
                    "w" => Choice::AllowAlways,
                    "d" => Choice::Deny,
                    _ => return None,
                },
            },
            "q" => Self::Answer {
                request_id,
                question: parts.next()?.parse().ok()?,
                option: parts.next()?.parse().ok()?,
            },
            _ => return None,
        };
        parts.next().is_none().then_some(callback)
    }
}

fn button(text: &str, callback: Callback) -> Value {
    json!({ "text": text, "callback_data": callback.encode() })
}

/// Makes a keyboard's buttons those of a guest's reply in the chat `chat_id`. A press on
/// such a button comes with the user who pressed it but without the chat, and a guest's
/// session is one user's in one chat, so the data has to name the chat.
pub fn for_guest(mut keyboard: Value, chat_id: i64) -> Value {
    let rows = keyboard["inline_keyboard"].as_array_mut();
    for row in rows.into_iter().flatten() {
        for button in row.as_array_mut().into_iter().flatten() {
            if let Some(data) = button["callback_data"].as_str() {
                button["callback_data"] = format!("{GUEST_PREFIX}{chat_id}:{data}").into();
            }
        }
    }
    keyboard
}

/// Reads the data of a button on a guest's reply: the chat the reply is in, and what the
/// button would carry anywhere else. `None` for the data of any other button.
pub fn guest_chat(data: &str) -> Option<(i64, &str)> {
    let (chat_id, rest) = data.strip_prefix(GUEST_PREFIX)?.split_once(':')?;
    Some((chat_id.parse().ok()?, rest))
}

/// A tool's input as JSON, with the fields that take least room first. What a call is
/// aimed at, a path or an address, is short, and what it carries there can be any
/// length. In the order of their names a file's content would come before its path and,
/// in a prompt that has to be cut, push it out of sight.
fn shortest_first(input: &Value) -> String {
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let Some(fields) = input.as_object().filter(|fields| !fields.is_empty()) else {
        return pretty(input);
    };
    let mut fields: Vec<(String, String)> = fields
        .iter()
        // A string's line breaks are escaped in JSON, so every break here is one of
        // the layout's and takes the indentation of the field it is in.
        .map(|(name, value)| (pretty(&json!(name)), pretty(value).replace('\n', "\n  ")))
        .collect();
    // Stable, so fields of one length stay in the order of their names.
    fields.sort_by_key(|(_, value)| value.len());
    let lines: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("  {name}: {value}"))
        .collect();
    format!("{{\n{}\n}}", lines.join(",\n"))
}

/// What a permission prompt says is being asked for: the command for the shell, and the
/// whole input for any other tool.
fn requested(tool_name: &str, input: &Value) -> String {
    match (tool_name, input["command"].as_str()) {
        ("Bash", Some(command)) => command.to_owned(),
        _ => shortest_first(input),
    }
}

/// The whole of a request that its prompt can only show the start of, or `None` if the
/// prompt shows all of it. An answer covers everything that was asked for, so the part
/// that does not fit has to reach the user some other way: as a file, sent ahead of the
/// prompt and named [`REQUEST_FILE`].
pub fn permission_overflow(tool_name: &str, input: &Value) -> Option<String> {
    let whole = requested(tool_name, input);
    (whole.chars().count() > PROMPT_INPUT_LIMIT).then_some(whole)
}

/// Message and buttons asking whether a tool call may run.
pub fn permission_prompt(request_id: &str, tool_name: &str, input: &Value) -> (String, Value) {
    let mut html = format!(
        "🔐 <b>Permission needed</b>\n{}",
        escape(&tool_summary(tool_name, &Value::Null))
    );
    let body = requested(tool_name, input);
    html.push_str(&format!(
        "\n<pre>{}</pre>",
        escape(&truncate(&body, PROMPT_INPUT_LIMIT))
    ));
    let total = body.chars().count();
    if total > PROMPT_INPUT_LIMIT {
        html.push_str(&format!(
            "\n✂️ <b>This is only the start</b> of {total} characters. \
             All of it is in <code>{REQUEST_FILE}</code> above."
        ));
    }
    if let Some(description) = input["description"].as_str() {
        html.push_str(&format!("\n<i>{}</i>", escape(description)));
    }
    let choice = |text, choice| {
        let request_id = request_id.to_owned();
        button(text, Callback::Permission { request_id, choice })
    };
    let mut rows = vec![json!([
        choice("✅ Allow", Choice::Allow),
        choice("⛔ Deny", Choice::Deny),
    ])];
    // The prompt shows one request, so the button has to say how far "always" reaches.
    let always = match remember::scope(tool_name) {
        Scope::ExactCommand => Some("♾ Always allow this exact command"),
        Scope::Tool => Some("♾ Always allow this tool"),
        Scope::Nothing => None,
    };
    if let Some(label) = always {
        rows.push(json!([choice(label, Choice::AllowAlways)]));
    }
    (html, json!({ "inline_keyboard": rows }))
}

/// What to add to a permission prompt once it has been answered.
pub fn permission_outcome(tool_name: &str, choice: Choice) -> String {
    let scope = match choice {
        Choice::Allow => return "<b>✅ Allowed</b>".into(),
        Choice::Deny => return "<b>⛔ Denied</b>".into(),
        Choice::AllowAlways => remember::scope(tool_name),
    };
    let covered = match scope {
        Scope::Nothing => return "<b>✅ Allowed</b>".into(),
        Scope::ExactCommand => "This exact command".to_owned(),
        Scope::Tool => format!(
            "Every use of {}",
            escape(&tool_summary(tool_name, &Value::Null))
        ),
    };
    format!("<b>♾ Always allowed</b>\n<i>{covered} now runs without asking, until /new.</i>")
}

/// Message and buttons for one of Claude's clarifying questions, or `None` when `index`
/// is past the last question.
pub fn question_prompt(request_id: &str, input: &Value, index: usize) -> Option<(String, Value)> {
    let questions = input["questions"].as_array()?;
    let question = questions.get(index)?;
    let mut html = format!("❓ <b>{}</b>", escape(question["question"].as_str()?));
    if questions.len() > 1 {
        html.push_str(&format!(" ({}/{})", index + 1, questions.len()));
    }
    let mut rows = Vec::new();
    for (option, entry) in question["options"].as_array()?.iter().enumerate() {
        let label = entry["label"].as_str()?;
        if let Some(description) = entry["description"].as_str() {
            html.push_str(&format!(
                "\n• <b>{}</b>: {}",
                escape(label),
                escape(description)
            ));
        }
        let callback = Callback::Answer {
            request_id: request_id.to_owned(),
            question: index,
            option,
        };
        rows.push(json!([button(&truncate(label, 40), callback)]));
    }
    Some((html, json!({ "inline_keyboard": rows })))
}

/// Label of the chosen option and the question text it answers.
pub fn question_answer(input: &Value, question: usize, option: usize) -> Option<(String, String)> {
    let entry = input["questions"].get(question)?;
    let label = entry["options"].get(option)?["label"].as_str()?;
    Some((entry["question"].as_str()?.to_owned(), label.to_owned()))
}

/// The tool input that hands the collected answers back to Claude.
pub fn answered_questions(input: &Value, answers: &serde_json::Map<String, Value>) -> Value {
    json!({ "questions": input["questions"], "answers": answers })
}

/// A pressed button of the `/resume` or `/rewind` list. Like [`Callback`], it has to fit
/// in 64 bytes, and on a guest's reply in what [`for_guest`] leaves of them: a
/// conversation's id is a UUID and a checkpoint is named by a time in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Go on with this conversation.
    Resume { conversation: String },
    /// Take the conversation back to before the turn this checkpoint was saved after.
    /// The conversation is named by its [`conversation_tag`], and by its whole id on a
    /// button from before there were tags.
    Rewind {
        conversation: String,
        checkpoint: u64,
    },
}

/// The start of a conversation's id, by which a `/rewind` button says what it is a list
/// of. It only has to tell the chat's conversations apart, of which few are kept, so
/// that a list from before `/new` or `/resume` is not taken for one of the conversation
/// the chat is in now.
pub fn conversation_tag(id: &str) -> &str {
    id.get(..CONVERSATION_TAG_CHARS).unwrap_or(id)
}

impl Pick {
    pub fn encode(&self) -> String {
        match self {
            Self::Resume { conversation } => format!("s:{conversation}"),
            Self::Rewind {
                conversation,
                checkpoint,
            } => format!("r:{conversation}:{checkpoint}"),
        }
    }

    pub fn parse(data: &str) -> Option<Self> {
        let mut parts = data.split(':');
        let (kind, conversation) = (parts.next()?, parts.next()?.to_owned());
        let pick = match kind {
            "s" => Self::Resume { conversation },
            "r" => Self::Rewind {
                conversation,
                checkpoint: parts.next()?.parse().ok()?,
            },
            _ => return None,
        };
        parts.next().is_none().then_some(pick)
    }
}

/// How long ago something was, as briefly as a button needs it.
fn ago(now_ms: u64, then_ms: u64) -> String {
    match now_ms.saturating_sub(then_ms) / 60_000 {
        0 => "just now".into(),
        minutes @ 1..60 => format!("{minutes} min ago"),
        minutes @ 60..1440 => format!("{} h ago", minutes / 60),
        minutes => format!("{} d ago", minutes / 1440),
    }
}

/// One row of a list: the start of a message, on one line, and its age.
fn pick_row(text: &str, then_ms: u64, now_ms: u64, pick: Pick) -> Value {
    let one_line = asked(text).split_whitespace().collect::<Vec<_>>().join(" ");
    let label = format!(
        "{} · {}",
        truncate(&one_line, PICK_LABEL_CHARS),
        ago(now_ms, then_ms)
    );
    json!([{ "text": label, "callback_data": pick.encode() }])
}

/// The `/resume` list: every saved conversation but the one the chat is in, the one
/// used last first. `None` if there is no other.
pub fn resume_picker(
    conversations: &[Conversation],
    current: &str,
    now_ms: u64,
) -> Option<(String, Value)> {
    let rows: Vec<Value> = conversations
        .iter()
        .filter(|conversation| conversation.id != current)
        .map(|conversation| {
            let pick = Pick::Resume {
                conversation: conversation.id.clone(),
            };
            pick_row(&conversation.title, conversation.used_ms, now_ms, pick)
        })
        .collect();
    let html = "💬 <b>Earlier conversations</b>\n\
                Pick one to go on with it. The one you are in now is kept.";
    (!rows.is_empty()).then(|| (html.to_owned(), json!({ "inline_keyboard": rows })))
}

/// What the `/resume` list turns into once a conversation has been picked. `last` is
/// the newest message in it.
pub fn resumed(conversation: &Conversation, last: Option<&str>) -> String {
    let mut html = format!(
        "💬 <b>Resumed</b>\n<i>{}</i>",
        escape(&truncate(&asked(&conversation.title), PICKED_TEXT_LIMIT))
    );
    if let Some(last) = last.filter(|last| *last != conversation.title) {
        html.push_str(&format!(
            "\n\nYour last message in it:\n<pre>{}</pre>",
            escape(&truncate(&asked(last), PICKED_TEXT_LIMIT))
        ));
    }
    html
}

/// The checkpoints whose turn can be undone, newest first like `checkpoints`. Undoing a
/// turn means going back to what it started from, so that has to be kept still, or to
/// have been nothing.
pub fn rewindable(checkpoints: &[Checkpoint]) -> Vec<&Checkpoint> {
    checkpoints
        .iter()
        .filter(|checkpoint| match checkpoint.parent {
            Some(0) => true,
            Some(parent) => checkpoints.iter().any(|other| other.at_ms == parent),
            None => false,
        })
        .collect()
}

/// The `/rewind` list: the user's recent messages in `conversation`, newest first.
/// `None` if no turn can be undone.
pub fn rewind_picker(
    conversation: &str,
    checkpoints: &[Checkpoint],
    now_ms: u64,
) -> Option<(String, Value)> {
    let rows: Vec<Value> = rewindable(checkpoints)
        .into_iter()
        .map(|checkpoint| {
            let pick = Pick::Rewind {
                conversation: conversation_tag(conversation).to_owned(),
                checkpoint: checkpoint.at_ms,
            };
            pick_row(&checkpoint.text, checkpoint.at_ms, now_ms, pick)
        })
        .collect();
    let html = "⏪ <b>Rewind</b>\n\
                Pick a message. The conversation and the files go back to how they were \
                before you sent it, and what came after is forgotten.";
    (!rows.is_empty()).then(|| (html.to_owned(), json!({ "inline_keyboard": rows })))
}

/// What the `/rewind` list turns into once a message has been picked. The message is
/// quoted so that it can be copied and sent again.
pub fn rewound(text: &str) -> String {
    format!(
        "⏪ <b>Rewound</b>\nThe conversation and the files are as they were before this \
         message:\n<pre>{}</pre>\nSend it again, or something else.",
        escape(&truncate(&asked(text), PICKED_TEXT_LIMIT))
    )
}

/// How an effort setting is written: its level, or `auto` when none is set and the
/// model's own default applies.
pub fn effort_name(effort: Option<Effort>) -> &'static str {
    effort.map_or(AUTO_EFFORT, Effort::as_str)
}

/// The setting `word` names, as typed after `/effort`. `None` if it names none.
pub fn parse_effort(word: &str) -> Option<Option<Effort>> {
    if word.eq_ignore_ascii_case(AUTO_EFFORT) {
        Some(None)
    } else {
        Effort::parse(word).map(Some)
    }
}

/// The setting a pressed button of the `/effort` list stands for.
pub fn picked_effort(data: &str) -> Option<Option<Effort>> {
    data.strip_prefix(EFFORT_PREFIX).and_then(parse_effort)
}

/// The `/effort` list: every level and `auto`, with the one in use marked.
pub fn effort_picker(current: Option<Effort>) -> (String, Value) {
    let html = format!(
        "🧠 <b>Effort: {}</b>\n\
         How much work I put into an answer. More takes longer and uses more tokens; \
         <code>{AUTO_EFFORT}</code> leaves it to the model.",
        effort_name(current)
    );
    let buttons: Vec<Value> = Effort::ALL
        .into_iter()
        .map(Some)
        .chain([None])
        .map(|effort| {
            let name = effort_name(effort);
            let mark = if effort == current { "● " } else { "" };
            json!({ "text": format!("{mark}{name}"), "callback_data": format!("{EFFORT_PREFIX}{name}") })
        })
        .collect();
    let rows: Vec<&[Value]> = buttons.chunks(3).collect();
    (html, json!({ "inline_keyboard": rows }))
}

/// The `/effort` list where a message can have no buttons to pick from: the setting,
/// and the words to send after the command.
pub fn effort_levels(current: Option<Effort>) -> String {
    let names: Vec<String> = Effort::ALL
        .into_iter()
        .map(Some)
        .chain([None])
        .map(|effort| format!("<code>{}</code>", effort_name(effort)))
        .collect();
    format!(
        "🧠 <b>Effort: {}</b>\n\
         How much work I put into an answer. Send /effort with one of {}.",
        effort_name(current),
        names.join(", ")
    )
}

/// What to say once `/effort` has set something, and what its list turns into once a
/// button has been pressed. A turn that is running keeps the level it started at.
pub fn effort_set(effort: Option<Effort>) -> String {
    format!(
        "🧠 <b>Effort: {}</b>\nIt applies from your next message.",
        effort_name(effort)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(at_ms: u64, parent: Option<u64>, text: &str) -> Checkpoint {
        Checkpoint {
            at_ms,
            parent,
            text: text.into(),
        }
    }

    #[test]
    fn a_guests_buttons_name_their_chat_and_still_fit_telegrams_limit() {
        let request_id = "0b0e7f0a-52a5-4b56-9f6f-6f1e8f0c2a11";
        let input = json!({ "questions": [{
            "question": "Which?",
            "options": [{ "label": "One" }, { "label": "Two" }],
        }]});
        let (_, question) = question_prompt(request_id, &input, 0).unwrap();
        let (_, permission) = permission_prompt(request_id, "Bash", &json!({ "command": "ls" }));
        // The longest ids there are: a supergroup's, and a user's of 52 bits.
        for chat_id in [-1_009_999_999_999, 4_503_599_627_370_495, 7] {
            for keyboard in [&question, &permission] {
                let plain: Vec<&Value> = keyboard["inline_keyboard"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|row| row.as_array().unwrap())
                    .collect();
                let guest = for_guest(keyboard.clone(), chat_id);
                let guest: Vec<&Value> = guest["inline_keyboard"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|row| row.as_array().unwrap())
                    .collect();
                assert_eq!(plain.len(), guest.len());
                for (plain, guest) in plain.iter().zip(guest) {
                    let data = guest["callback_data"].as_str().unwrap();
                    assert!(data.len() <= 64, "{data}");
                    assert_eq!(
                        guest_chat(data),
                        Some((chat_id, plain["callback_data"].as_str().unwrap()))
                    );
                    assert_eq!(guest["text"], plain["text"]);
                    // Nothing reads it as a button of a chat the bot is in.
                    assert_eq!(Callback::parse(data), None);
                    assert_eq!(Pick::parse(data), None);
                    assert_eq!(picked_effort(data), None);
                }
            }
        }
        for plain in [
            "p:0b0e7f0a:a",
            "q:0b0e7f0a:0:1",
            "s:0b0e7f0a",
            "e:high",
            "g:x",
            "gx:p",
        ] {
            assert_eq!(guest_chat(plain), None, "{plain}");
        }
    }

    #[test]
    fn a_guest_gets_the_answer_and_the_notes_in_one_message() {
        let notes = ["📎 <code>a.pdf</code> went elsewhere.".to_owned()];
        assert_eq!(one_message(&["Short.".into()], &[]), "Short.");
        assert_eq!(
            one_message(&["Short.".into()], &notes),
            format!("Short.\n\n{}", notes[0])
        );
        // No answer at all still leaves the notes.
        assert_eq!(one_message(&[], &notes), notes[0]);

        // An answer of several messages keeps the first and says that there was more.
        let long = "word ".repeat(2000);
        let messages = answer(&long);
        assert!(messages.len() > 1);
        let one = one_message(&messages, &notes);
        assert!(one.starts_with(&messages[0]));
        assert!(one.ends_with(&format!("{CUT}\n\n{}", notes[0])));
        assert!(units(&one) <= MESSAGE_LIMIT);

        // Notes that need more room than a message leaves take it from the answer, and
        // more of them than anyone would read are cut short themselves.
        let many: Vec<String> = (0..80)
            .map(|n| format!("📎 <code>file-{n}.pdf</code> went elsewhere."))
            .collect();
        let bold = format!("<b>{}</b>", "bold ".repeat(770).trim_end());
        assert_eq!(units(&bold), 3856);
        let one = one_message(&[bold], &many);
        assert!(units(&one) <= MESSAGE_LIMIT, "{}", units(&one));
        assert!(one.starts_with("bold bold"));
        assert!(one.contains(CUT));
        assert!(one.contains("file-0.pdf") && one.ends_with('…'));
        assert!(!one.contains("file-79.pdf"));
    }

    #[test]
    fn a_draft_is_the_message_being_written() {
        assert_eq!(draft(""), None);
        assert_eq!(draft("Here is **the"), Some("Here is **the".into()));
        assert_eq!(
            draft("Here is **the answer**.\n\n- first"),
            Some("Here is <b>the answer</b>.\n\n• first".into())
        );
        // A code block that is still being written is closed like a finished one.
        assert_eq!(
            draft("Run:\n\n```sh\necho"),
            Some("Run:\n\n<pre><code class=\"language-sh\">echo</code></pre>".into())
        );
        // Past one message, it is the one the text has got to.
        let long = format!("{}\n\nThe end is near", "word ".repeat(2000));
        assert_eq!(draft(&long), Some("The end is near".into()));
    }

    #[test]
    fn a_guest_request_drops_the_summons_and_quotes_what_it_replies_to() {
        assert_eq!(without_mentions("@the_bot what is this?"), "what is this?");
        assert_eq!(
            without_mentions("  @the_bot   @other /stop now "),
            "/stop now"
        );
        assert_eq!(without_mentions("@the_bot"), "");
        assert_eq!(
            without_mentions("mail me@example.com"),
            "mail me@example.com"
        );
        assert_eq!(without_mentions("tell @bea hello"), "tell @bea hello");

        assert_eq!(
            guest_request("what is this?", None),
            "[Sent from a chat where you are a guest. Your reply must be a single message \
             under 3000 characters.]\n\nwhat is this?"
        );
        assert_eq!(
            guest_request("summarise", Some((Some("Bea"), "It rained.\nAll week."))),
            format!(
                "{GUEST_NOTE}\n\n[It replies to this message from Bea:]\n> It rained.\n> All week.\n\nsummarise"
            )
        );
        // A bare summons in reply to a message asks about that message.
        assert_eq!(
            guest_request("", Some((None, "It rained."))),
            format!("{GUEST_NOTE}\n\n[It replies to this message:]\n> It rained.")
        );
    }

    #[test]
    fn the_effort_levels_can_be_listed_without_buttons() {
        let listed = effort_levels(Some(Effort::High));
        assert!(listed.starts_with("🧠 <b>Effort: high</b>"));
        for name in ["low", "medium", "high", "xhigh", "max", "auto"] {
            assert!(listed.contains(&format!("<code>{name}</code>")), "{name}");
            assert!(parse_effort(name).is_some());
        }
    }

    #[test]
    fn a_pick_survives_callback_data_and_fits_telegrams_limit() {
        let conversation = "0b0e7f0a-52a5-4b56-9f6f-6f1e8f0c2a11".to_owned();
        for pick in [
            Pick::Resume {
                conversation: conversation.clone(),
            },
            Pick::Rewind {
                conversation,
                checkpoint: 9_999_999_999_999,
            },
        ] {
            let data = pick.encode();
            assert!(data.len() <= 64, "{data}");
            assert_eq!(Pick::parse(&data), Some(pick));
            // The two kinds of button never read each other's data.
            assert_eq!(Callback::parse(&data), None);
        }
        assert_eq!(Pick::parse("p:0b0e7f0a:a"), None);
        assert_eq!(Pick::parse("r:0b0e7f0a"), None);
        assert_eq!(Pick::parse("r:0b0e7f0a:12:extra"), None);
    }

    #[test]
    fn a_turn_can_be_rewound_only_while_what_it_started_from_is_kept() {
        // Newest first. The oldest kept one started from a checkpoint that is gone, and
        // the one before a state saved without a checkpoint has nothing to go back to.
        let kept = [
            checkpoint(400, Some(300), "fourth"),
            checkpoint(300, Some(200), "third"),
            checkpoint(200, Some(100), "second"),
        ];
        let offered: Vec<_> = rewindable(&kept).iter().map(|kept| kept.at_ms).collect();
        assert_eq!(offered, [400, 300]);

        // A conversation's first turn started from nothing, which is always there.
        let short = [
            checkpoint(200, Some(100), "second"),
            checkpoint(100, Some(0), "first"),
        ];
        assert_eq!(rewindable(&short).len(), 2);
        assert!(rewindable(&[checkpoint(100, None, "after an old state")]).is_empty());
        assert_eq!(rewind_picker("c", &kept[2..], 500), None);
    }

    #[test]
    fn a_guests_lists_fit_their_buttons_and_call_messages_by_what_was_written() {
        let current = "0b0e7f0a-52a5-4b56-9f6f-6f1e8f0c2a11";
        let other = "7c1d2e3f-0000-4b56-9f6f-6f1e8f0c2a11";
        // What a guest's turns are given, which is more than the user wrote.
        let plain = guest_request("what is the plan?\nAnd who pays?", None);
        let reply = guest_request(
            "summarise this",
            Some((Some("Bea"), "It rained.\n\nAll week.")),
        );
        let bare = guest_request("", Some((None, "It rained.\nAll week.")));
        let kept = [
            checkpoint(9_999_999_999_999, Some(9_999_999_879_999), &bare),
            checkpoint(9_999_999_879_999, Some(9_999_992_799_999), &reply),
            checkpoint(9_999_992_799_999, Some(0), &plain),
        ];
        let (_, rewind) = rewind_picker(current, &kept, 9_999_999_999_999).expect("a list");
        let saved = [Conversation {
            id: other.into(),
            title: reply.clone(),
            used_ms: 0,
            head: 1,
        }];
        let (_, resume) = resume_picker(&saved, current, 60_000).expect("a list");

        let texts = |keyboard: &Value| -> Vec<String> {
            let rows = keyboard["inline_keyboard"].as_array().unwrap();
            rows.iter()
                .map(|row| row[0]["text"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            texts(&rewind),
            [
                "It rained. All week. · just now",
                "summarise this · 2 min ago",
                "what is the plan? And who pays? · 2 h ago",
            ]
        );
        assert_eq!(texts(&resume), ["summarise this · 1 min ago"]);
        assert_eq!(
            rewound(&reply),
            "⏪ <b>Rewound</b>\nThe conversation and the files are as they were before this \
             message:\n<pre>summarise this</pre>\nSend it again, or something else."
        );
        assert_eq!(
            resumed(&saved[0], Some(&plain)),
            "💬 <b>Resumed</b>\n<i>summarise this</i>\n\n\
             Your last message in it:\n<pre>what is the plan?\nAnd who pays?</pre>"
        );
        // A message of a chat the bot is in is what it always was.
        assert_eq!(
            asked("[It replies to this message"),
            "[It replies to this message"
        );

        // With the longest ids there are, every button still names its chat, and what
        // is left of its data says the same as on a list in a chat.
        for chat_id in [-1_009_999_999_999, 4_503_599_627_370_495] {
            for keyboard in [&rewind, &resume] {
                let guest = for_guest(keyboard.clone(), chat_id);
                for (row, plain) in guest["inline_keyboard"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(keyboard["inline_keyboard"].as_array().unwrap())
                {
                    let data = row[0]["callback_data"].as_str().unwrap();
                    assert!(data.len() <= 64, "{data}");
                    let (named, rest) = guest_chat(data).expect("a guest's button");
                    assert_eq!(named, chat_id);
                    assert_eq!(rest, plain[0]["callback_data"].as_str().unwrap());
                    assert!(Pick::parse(rest).is_some());
                }
            }
        }
        let pressed = Pick::parse(
            rewind["inline_keyboard"][0][0]["callback_data"]
                .as_str()
                .unwrap(),
        );
        assert_eq!(
            pressed,
            Some(Pick::Rewind {
                conversation: "0b0e7f0a".into(),
                checkpoint: 9_999_999_999_999,
            })
        );
        // The list is told from one of another conversation, and a button from before
        // ids were cut short still says which conversation it is of.
        assert_eq!(conversation_tag(current), "0b0e7f0a");
        assert_eq!(conversation_tag("0b0e7f0a"), conversation_tag(current));
        assert_ne!(conversation_tag(other), conversation_tag(current));
        assert_eq!(conversation_tag(""), "");
    }

    #[test]
    fn the_rewind_list_shows_each_message_on_one_line_with_its_age() {
        let kept = [
            checkpoint(
                7_200_000,
                Some(60_000),
                "Now make\nit a PDF, with a long title that goes on",
            ),
            checkpoint(60_000, Some(0), "Summarise the chat"),
        ];
        let (html, keyboard) = rewind_picker("c1", &kept, 7_260_000).expect("a list");
        assert!(html.starts_with("⏪ <b>Rewind</b>"));
        assert_eq!(
            keyboard,
            json!({ "inline_keyboard": [
                [{ "text": "Now make it a PDF, with a long title… · 1 min ago", "callback_data": "r:c1:7200000" }],
                [{ "text": "Summarise the chat · 2 h ago", "callback_data": "r:c1:60000" }],
            ] })
        );
        assert_eq!(ago(90_000_000, 0), "1 d ago");
        assert_eq!(ago(10, 20), "just now");
    }

    #[test]
    fn the_resume_list_leaves_out_the_conversation_the_chat_is_in() {
        let conversation = |id: &str, title: &str, used_ms| Conversation {
            id: id.into(),
            title: title.into(),
            used_ms,
            head: used_ms,
        };
        let saved = [
            conversation("now", "The current one", 300_000),
            conversation("old", "Plan the outing", 0),
        ];
        let (_, keyboard) = resume_picker(&saved, "now", 300_000).expect("a list");
        assert_eq!(
            keyboard,
            json!({ "inline_keyboard": [
                [{ "text": "Plan the outing · 5 min ago", "callback_data": "s:old" }],
            ] })
        );
        assert_eq!(resume_picker(&saved[..1], "now", 300_000), None);

        assert_eq!(
            resumed(&saved[1], Some("And <b>who</b> pays?")),
            "💬 <b>Resumed</b>\n<i>Plan the outing</i>\n\n\
             Your last message in it:\n<pre>And &lt;b&gt;who&lt;/b&gt; pays?</pre>"
        );
        // A conversation of one message is not shown that message twice.
        assert_eq!(
            resumed(&saved[1], Some("Plan the outing")),
            "💬 <b>Resumed</b>\n<i>Plan the outing</i>"
        );
    }

    #[test]
    fn effort_is_picked_from_a_list_that_marks_the_level_in_use() {
        let labels = |keyboard: &Value| -> Vec<Vec<String>> {
            let rows = keyboard["inline_keyboard"].as_array().unwrap();
            let label = |button: &Value| button["text"].as_str().unwrap().to_owned();
            rows.iter()
                .map(|row| row.as_array().unwrap().iter().map(label).collect())
                .collect()
        };
        let (html, keyboard) = effort_picker(Some(Effort::High));
        assert!(html.starts_with("🧠 <b>Effort: high</b>\n"), "{html}");
        assert_eq!(
            labels(&keyboard),
            [["low", "medium", "● high"], ["xhigh", "max", "auto"]]
        );
        let (html, keyboard) = effort_picker(None);
        assert!(html.starts_with("🧠 <b>Effort: auto</b>\n"), "{html}");
        assert_eq!(labels(&keyboard)[1], ["xhigh", "max", "● auto"]);

        // Each button comes back as the setting it shows, and as no other kind of button.
        let settings = Effort::ALL.into_iter().map(Some).chain([None]);
        let buttons = keyboard["inline_keyboard"].as_array().unwrap().iter();
        for (button, setting) in buttons
            .flat_map(|row| row.as_array().unwrap())
            .zip(settings)
        {
            let data = button["callback_data"].as_str().unwrap();
            assert_eq!(picked_effort(data), Some(setting), "{data}");
            assert_eq!(Pick::parse(data), None);
            assert_eq!(Callback::parse(data), None);
        }
        for data in ["e:extreme", "e:", "high", "s:high"] {
            assert_eq!(picked_effort(data), None, "{data}");
        }
    }

    #[test]
    fn effort_can_be_named_after_the_command() {
        assert_eq!(parse_effort("xhigh"), Some(Some(Effort::Xhigh)));
        assert_eq!(parse_effort("Max"), Some(Some(Effort::Max)));
        assert_eq!(parse_effort("auto"), Some(None));
        assert_eq!(parse_effort("extreme"), None);
        assert_eq!(
            effort_set(Some(Effort::Low)),
            "🧠 <b>Effort: low</b>\nIt applies from your next message."
        );
        assert!(effort_set(None).starts_with("🧠 <b>Effort: auto</b>"));
    }

    #[test]
    fn markdown_becomes_telegram_html() {
        let markdown = "# Title\n\nSome **bold** and *italic* with `code` & <tags>.\n\n\
                        - one\n- two\n  1. nested\n\n```rust\nlet a = 1 < 2;\n```\n\n\
                        [link](https://example.com/?a=1&b=\"2\")";
        let [message] = answer(markdown).try_into().expect("one message");
        assert_eq!(
            message,
            "<b>Title</b>\n\n\
             Some <b>bold</b> and <i>italic</i> with <code>code</code> &amp; &lt;tags&gt;.\n\n\
             • one\n• two\n  1. nested\n\n\
             <pre><code class=\"language-rust\">let a = 1 &lt; 2;</code></pre>\n\n\
             <a href=\"https://example.com/?a=1&amp;b=&quot;2&quot;\">link</a>"
        );
    }

    #[test]
    fn tables_are_shown_preformatted_without_inner_markup() {
        let [message] = answer("| a | **b** |\n|---|---|\n| 1 | `2` |")
            .try_into()
            .expect("one message");
        assert_eq!(message, "<pre>a | b\n1 | 2</pre>");
    }

    #[test]
    fn long_answers_are_split_between_blocks() {
        let paragraph = "word ".repeat(300);
        let markdown = [paragraph.as_str(); 5].join("\n\n");
        let messages = answer(&markdown);
        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| units(message) <= MESSAGE_BUDGET)
        );
        let words: usize = messages.iter().map(|m| m.split_whitespace().count()).sum();
        assert_eq!(words, 1500, "nothing is lost");
    }

    #[test]
    fn an_oversized_code_block_is_split_into_several_preformatted_messages() {
        let line = "let x = a < b && c > d;";
        let messages = answer(&format!("```\n{}```", format!("{line}\n").repeat(400)));
        assert!(messages.len() > 1);
        let mut lines = 0;
        for message in &messages {
            assert!(units(message) <= MESSAGE_BUDGET);
            let inner = message
                .strip_prefix("<pre>")
                .and_then(|rest| rest.strip_suffix("</pre>"))
                .expect("each piece is preformatted");
            assert!(!inner.contains(['<', '>']), "markup inside <pre>");
            for restored in to_plain(inner).lines() {
                assert_eq!(restored, line);
                lines += 1;
            }
        }
        assert_eq!(lines, 400, "nothing is lost");
    }

    #[test]
    fn a_cut_never_lands_inside_an_entity() {
        let text = "&amp;".repeat(100);
        for piece in split_text(&text, 23) {
            assert_eq!(piece.len() % 5, 0, "{piece}");
        }
    }

    #[test]
    fn what_a_failed_turn_said_is_cut_to_fit_a_message() {
        assert_eq!(
            failure("error_max_turns", None),
            "⚠️ The agent stopped early (error_max_turns)."
        );
        assert_eq!(
            failure(
                "runner_error",
                Some("claude exited: exit status: 1\n<oops>")
            ),
            "⚠️ The agent stopped early (runner_error).\n\
             <pre>claude exited: exit status: 1\n&lt;oops&gt;</pre>"
        );

        // A crash prints the line of the program it happened in, which can be any
        // length, then what went wrong, then where.
        let dump = format!(
            "claude exited: exit status: 1\n{}\nTypeError: 𝑥 is not a function\n{}",
            "𝑥".repeat(20_000),
            "    at somewhere\n".repeat(200),
        );
        let message = failure("runner_error", Some(&dump));
        assert!(units(&message) <= MESSAGE_LIMIT, "{}", units(&message));
        assert!(message.contains("claude exited: exit status: 1\n𝑥𝑥"));
        assert!(message.contains("𝑥…\nTypeError: 𝑥 is not a function\n    at somewhere"));
        assert!(message.ends_with("…</pre>"));
    }

    #[test]
    fn plain_fallback_undoes_the_markup() {
        assert_eq!(
            to_plain("<b>a &lt; b</b> &amp; <code>c</code>"),
            "a < b & c"
        );
    }

    #[test]
    fn status_shows_recent_tools_and_what_was_said_before_them() {
        let mut status = Status::default();
        assert!(!status.observe(&json!({ "type": "system" })));
        assert!(status.is_empty());
        assert_eq!(status.render(WORKING), WORKING);

        let event = json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "Let me check <that>." },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls -la\nsecond line" } },
            { "type": "tool_use", "name": "mcp__domain__lookup", "input": { "id": 7 } },
        ] } });
        assert!(status.observe(&event));
        assert!(!status.is_empty());
        assert_eq!(
            status.render(WORKING),
            "⏳ Working…\n🔧 <code>Bash: ls -la</code>\n🔧 <code>domain › lookup</code>\n\n\
             Let me check &lt;that&gt;."
        );
    }

    fn said(text: &str) -> Value {
        json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": text },
        ] } })
    }

    fn used(tool: &str) -> Value {
        json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "name": tool, "input": {} },
        ] } })
    }

    #[test]
    fn status_leaves_the_answer_out() {
        let mut status = Status::default();
        // Words become part of the progress only once a tool call follows them.
        assert!(!status.observe(&said("I'll read the file first.")));
        assert!(status.is_empty());
        assert!(status.observe(&used("Read")));
        // The last words of a turn are the answer.
        assert!(!status.observe(&said("The file has three lines.")));
        assert_eq!(
            status.render(DONE),
            "✅ Done\n🔧 <code>Read</code>\n\nI'll read the file first."
        );

        // A turn that only answers has no progress to keep.
        let mut status = Status::default();
        assert!(!status.observe(&said("Hello!")));
        assert!(status.is_empty());
    }

    #[test]
    fn status_counts_the_tools_it_no_longer_lists() {
        let mut status = Status::default();
        for index in 0..STATUS_TOOL_LINES + 2 {
            status.observe(&used(&format!("tool{index}")));
        }
        let html = status.render(WORKING);
        assert!(html.starts_with("⏳ Working…\n<i>… 2 earlier</i>\n🔧 <code>tool2</code>\n"));
        assert!(html.ends_with(&format!("<code>tool{}</code>", STATUS_TOOL_LINES + 1)));
        assert_eq!(html.matches("🔧").count(), STATUS_TOOL_LINES);
    }

    #[test]
    fn callbacks_round_trip_within_telegrams_size_limit() {
        let request_id = "123e4567-e89b-42d3-a456-426614174000".to_owned();
        let callbacks = [
            Callback::Permission {
                request_id: request_id.clone(),
                choice: Choice::AllowAlways,
            },
            Callback::Answer {
                request_id,
                question: 3,
                option: 3,
            },
        ];
        for callback in callbacks {
            let data = callback.encode();
            assert!(data.len() <= 64, "{data}");
            assert_eq!(Callback::parse(&data), Some(callback));
        }
        for data in [
            "",
            "p:id",
            "p:id:x",
            "q:id:1",
            "q:id:a:b",
            "p:id:a:extra",
            "z:id:a",
        ] {
            assert_eq!(Callback::parse(data), None, "{data}");
        }
    }

    #[test]
    fn permission_prompt_shows_the_command_and_offers_three_choices() {
        let input = json!({ "command": "rm -rf <dir>", "description": "Remove it" });
        let (html, keyboard) = permission_prompt("id", "Bash", &input);
        assert_eq!(
            html,
            "🔐 <b>Permission needed</b>\nBash\n<pre>rm -rf &lt;dir&gt;</pre>\n<i>Remove it</i>"
        );
        let buttons = |keyboard: &Value| -> Vec<(String, String)> {
            keyboard["inline_keyboard"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|row| row.as_array().unwrap())
                .map(|button| {
                    let field = |name: &str| button[name].as_str().unwrap().to_owned();
                    (field("text"), field("callback_data"))
                })
                .collect()
        };
        let pair = |text: &str, data: &str| (text.to_owned(), data.to_owned());
        assert_eq!(
            buttons(&keyboard),
            [
                pair("✅ Allow", "p:id:a"),
                pair("⛔ Deny", "p:id:d"),
                pair("♾ Always allow this exact command", "p:id:w"),
            ]
        );

        // "Always" says how far it reaches, and is not offered where nothing is remembered.
        let (_, keyboard) = permission_prompt("id", "mcp__domain__lookup", &json!({ "id": 7 }));
        assert_eq!(
            buttons(&keyboard)[2],
            pair("♾ Always allow this tool", "p:id:w")
        );
        let (_, keyboard) = permission_prompt("id", agent_proto::ASK_USER_QUESTION, &json!({}));
        assert_eq!(buttons(&keyboard).len(), 2);
    }

    #[test]
    fn a_prompt_shows_where_a_call_is_aimed_before_what_it_carries() {
        let content = "line\n".repeat(1000);
        let input = json!({ "content": content, "file_path": "/home/agent/notes.txt" });
        let (html, _) = permission_prompt("id", "Write", &input);
        let path = html
            .find("/home/agent/notes.txt")
            .expect("the path is shown");
        assert!(path < html.find("\"content\"").expect("the content is begun"));

        // Short inputs are still the JSON they always were, nested values included.
        assert_eq!(
            shortest_first(&json!({ "b": { "deep": [1, 2] }, "a": 1 })),
            "{\n  \"a\": 1,\n  \"b\": {\n    \"deep\": [\n      1,\n      2\n    ]\n  }\n}"
        );
        assert_eq!(shortest_first(&json!({})), "{}");
        assert_eq!(shortest_first(&json!("text")), "\"text\"");
    }

    #[test]
    fn a_request_too_long_for_its_prompt_is_also_given_whole() {
        let short = json!({ "command": "ls -l" });
        assert_eq!(permission_overflow("Bash", &short), None);
        assert!(
            !permission_prompt("id", "Bash", &short)
                .0
                .contains("request.txt")
        );

        let command = format!("echo {}", "a".repeat(PROMPT_INPUT_LIMIT));
        let long = json!({ "command": command });
        assert_eq!(permission_overflow("Bash", &long), Some(command.clone()));
        let (html, _) = permission_prompt("id", "Bash", &long);
        assert!(html.contains("This is only the start</b> of 1505 characters"));
        assert!(html.contains("<code>request.txt</code>"));
        assert!(units(&html) < MESSAGE_BUDGET);

        // Other tools are measured by what the prompt would show of them.
        let input = json!({ "file_path": "/x", "content": "c".repeat(PROMPT_INPUT_LIMIT) });
        let whole = permission_overflow("Write", &input).expect("too long to show");
        assert_eq!(serde_json::from_str::<Value>(&whole).unwrap(), input);
    }

    #[test]
    fn the_outcome_of_always_names_what_was_approved() {
        assert_eq!(
            permission_outcome("Bash", Choice::Allow),
            "<b>✅ Allowed</b>"
        );
        assert_eq!(permission_outcome("Bash", Choice::Deny), "<b>⛔ Denied</b>");
        assert_eq!(
            permission_outcome("Bash", Choice::AllowAlways),
            "<b>♾ Always allowed</b>\n<i>This exact command now runs without asking, until /new.</i>"
        );
        assert_eq!(
            permission_outcome("mcp__domain__lookup", Choice::AllowAlways),
            "<b>♾ Always allowed</b>\n<i>Every use of domain › lookup now runs without asking, until /new.</i>"
        );
    }

    #[test]
    fn list_items_separated_by_blank_lines_keep_their_text_on_the_marker_line() {
        let markdown = "1. Foo\n\n2. Bar\n\n   More about bar.\n\n- loose\n\n- bullets";
        assert_eq!(
            answer(markdown),
            ["1. Foo\n2. Bar\nMore about bar.\n\n• loose\n• bullets"]
        );
    }

    #[test]
    fn questions_are_asked_one_at_a_time_and_answers_keyed_by_question_text() {
        let input = json!({ "questions": [
            { "question": "Which format?", "header": "Format", "multiSelect": false, "options": [
                { "label": "PDF", "description": "A PDF file." },
                { "label": "Markdown", "description": "A .md file." },
            ] },
            { "question": "How long?", "options": [{ "label": "Short" }, { "label": "Long" }] },
        ] });
        let (html, keyboard) = question_prompt("id", &input, 0).unwrap();
        assert_eq!(
            html,
            "❓ <b>Which format?</b> (1/2)\n• <b>PDF</b>: A PDF file.\n• <b>Markdown</b>: A .md file."
        );
        assert_eq!(
            keyboard["inline_keyboard"][1][0]["callback_data"],
            "q:id:0:1"
        );
        assert!(question_prompt("id", &input, 2).is_none());

        let (question, label) = question_answer(&input, 1, 1).unwrap();
        assert_eq!((question.as_str(), label.as_str()), ("How long?", "Long"));
        assert!(question_answer(&input, 1, 5).is_none());

        let mut answers = serde_json::Map::new();
        answers.insert(question, label.into());
        let updated = answered_questions(&input, &answers);
        assert_eq!(updated["answers"], json!({ "How long?": "Long" }));
        assert_eq!(updated["questions"], input["questions"]);
    }
}
