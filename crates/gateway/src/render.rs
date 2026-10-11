//! Everything the bot says, as text: Claude's Markdown turned into the HTML subset
//! Telegram understands and cut into messages that fit, the live status line, permission
//! prompts with their buttons, and the callback data those buttons carry.
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
const STATUS_TEXT_TAIL: usize = 1200;
const STATUS_TOOL_LINES: usize = 6;
const PROMPT_INPUT_LIMIT: usize = 1500;
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

/// Message and buttons asking whether a tool call may run.
pub fn permission_prompt(request_id: &str, tool_name: &str, input: &Value) -> (String, Value) {
    let mut html = format!(
        "🔐 <b>Permission needed</b>\n{}",
        escape(&tool_summary(tool_name, &Value::Null))
    );
    let body = match (tool_name, input["command"].as_str()) {
        ("Bash", Some(command)) => command.to_owned(),
        _ => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    html.push_str(&format!(
        "\n<pre>{}</pre>",
        escape(&truncate(&body, PROMPT_INPUT_LIMIT))
    ));
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
/// in 64 bytes: a conversation's id is a UUID and a checkpoint is named by a time in
/// milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Go on with this conversation.
    Resume { conversation: String },
    /// Take the conversation back to before the turn this checkpoint was saved after.
    Rewind {
        conversation: String,
        checkpoint: u64,
    },
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
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
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
        escape(&truncate(&conversation.title, PICKED_TEXT_LIMIT))
    );
    if let Some(last) = last.filter(|last| *last != conversation.title) {
        html.push_str(&format!(
            "\n\nYour last message in it:\n<pre>{}</pre>",
            escape(&truncate(last, PICKED_TEXT_LIMIT))
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
                conversation: conversation.to_owned(),
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
        escape(&truncate(text, PICKED_TEXT_LIMIT))
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
