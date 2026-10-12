//! The handful of Telegram Bot API calls the service makes.

use std::time::Duration;

use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{Delay, Fetch, Headers, Method, Request, RequestInit, Result, js_sys::Uint8Array};

use crate::{
    config::Config,
    platform::{error, random_uuid},
    render, scrub,
};

const RATE_LIMIT_RETRIES: u32 = 2;
/// Longer waits are not sat out: the caller is usually holding up a session's event loop.
const RATE_LIMIT_MAX_WAIT_SECS: u64 = 5;

/// How Telegram's refusal of markup it cannot read begins.
const UNPARSED: &str = "can't parse entities";

/// Sets an optional field of a call.
fn set(payload: &mut Value, field: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        payload[field] = value.into();
    }
}

pub struct Bot {
    base: String,
    /// This Worker's host, which is never shown in the chat; see [`scrub::redact`].
    hidden: String,
}

impl Bot {
    pub fn new(cfg: &Config) -> Self {
        Self {
            base: format!("{}/bot{}", cfg.telegram_api, cfg.bot_token),
            hidden: scrub::host_of(&cfg.public_origin).to_owned(),
        }
    }

    /// Makes a call. `patience` is how many times a rate limit is waited out before it
    /// counts as a failure.
    async fn post(
        &self,
        method: &str,
        content_type: &str,
        body: JsValue,
        patience: u32,
    ) -> Result<Value> {
        let mut waits = 0;
        loop {
            let headers = Headers::new();
            headers.set("content-type", content_type)?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(body.clone()));
            let request = Request::new_with_init(&format!("{}/{method}", self.base), &init)?;
            let reply: Value = Fetch::Request(request).send().await?.json().await?;
            if reply["ok"] == true {
                return Ok(reply["result"].clone());
            }
            // When Telegram rate-limits it says how long to stay away. Waiting that out
            // beats dropping a message, as long as it is short.
            match reply["parameters"]["retry_after"].as_u64() {
                Some(seconds) if waits < patience && seconds <= RATE_LIMIT_MAX_WAIT_SECS => {
                    waits += 1;
                    Delay::from(Duration::from_secs(seconds + 1)).await;
                }
                _ => {
                    let description = reply["description"].as_str().unwrap_or("unknown error");
                    return Err(error(format!("telegram {method}: {description}")));
                }
            }
        }
    }

    async fn call(&self, method: &str, payload: Value) -> Result<Value> {
        let body = JsValue::from_str(&payload.to_string());
        self.post(method, "application/json", body, RATE_LIMIT_RETRIES)
            .await
    }

    /// Makes a call that carries HTML. `payload` builds it from the text and, while
    /// that is HTML, the parse mode. If Telegram cannot parse the markup, the text goes
    /// out plain rather than not at all.
    async fn call_with_html(
        &self,
        method: &str,
        html: &str,
        payload: impl Fn(&str, Option<&str>) -> Value,
    ) -> Result<Value> {
        let html = &scrub::redact(html, &self.hidden);
        match self.call(method, payload(html, Some("HTML"))).await {
            Err(error) if error.to_string().contains(UNPARSED) => {
                self.call(method, payload(&render::to_plain(html), None))
                    .await
            }
            other => other,
        }
    }

    /// Sends HTML and returns the new message's id.
    pub async fn send_message(
        &self,
        chat_id: i64,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<i64> {
        let payload = |text: &str, parse_mode: Option<&str>| {
            let mut payload = json!({
                "chat_id": chat_id,
                "text": text,
                "link_preview_options": { "is_disabled": true },
            });
            set(&mut payload, "parse_mode", parse_mode);
            set(&mut payload, "reply_markup", keyboard.cloned());
            payload
        };
        let sent = self.call_with_html("sendMessage", html, payload).await?;
        sent["message_id"]
            .as_i64()
            .ok_or_else(|| error("telegram sendMessage: no message id"))
    }

    /// Replaces the text of the message `target` names. Omitting the keyboard removes
    /// any buttons it had.
    async fn edit(&self, target: Value, html: &str, keyboard: Option<&Value>) -> Result<()> {
        let payload = |text: &str, parse_mode: Option<&str>| {
            let mut payload = target.clone();
            payload["text"] = text.into();
            payload["link_preview_options"] = json!({ "is_disabled": true });
            set(&mut payload, "parse_mode", parse_mode);
            set(&mut payload, "reply_markup", keyboard.cloned());
            payload
        };
        match self.call_with_html("editMessageText", html, payload).await {
            Err(error) if !error.to_string().contains("message is not modified") => Err(error),
            _ => Ok(()),
        }
    }

    /// Replaces the text of a message in a chat the bot is in.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<()> {
        let target = json!({ "chat_id": chat_id, "message_id": message_id });
        self.edit(target, html, keyboard).await
    }

    /// Answers the message that summoned the bot as a guest, in a chat it is not a
    /// member of. That can be done once, and nothing else can be sent there. Returns
    /// the id the answer is changed by afterwards, see [`Self::edit_guest_reply`].
    pub async fn answer_guest(
        &self,
        query_id: &str,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<String> {
        let payload = |text: &str, parse_mode: Option<&str>| {
            let mut content = json!({
                "message_text": text,
                "link_preview_options": { "is_disabled": true },
            });
            set(&mut content, "parse_mode", parse_mode);
            // The answer has the form of an inline query's result. Nobody picks it from
            // a list, so its id and title are never seen.
            let mut result = json!({
                "type": "article",
                "id": "reply",
                "title": "Reply",
                "input_message_content": content,
            });
            set(&mut result, "reply_markup", keyboard.cloned());
            json!({ "guest_query_id": query_id, "result": result })
        };
        let sent = self
            .call_with_html("answerGuestQuery", html, payload)
            .await?;
        sent["inline_message_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| error("telegram answerGuestQuery: no message id"))
    }

    /// Replaces the text of the reply a guest summon got.
    pub async fn edit_guest_reply(
        &self,
        inline_message_id: &str,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<()> {
        let target = json!({ "inline_message_id": inline_message_id });
        self.edit(target, html, keyboard).await
    }

    /// Shows `html` in a private chat as the message being written, in place of what
    /// the draft `draft_id` showed before, and with a button to stop it. Without text
    /// the draft is a placeholder. A draft is a preview that goes away by itself: what
    /// is to stay in the chat has to be sent as a message.
    ///
    /// Returns whether it is on show. A rate limit is not waited out and no markup is
    /// retried as plain text, because the next draft replaces this one anyway. An error
    /// is Telegram not taking drafts here at all.
    pub async fn send_draft(&self, chat_id: i64, draft_id: i64, html: &str) -> Result<bool> {
        let html = &scrub::redact(html, &self.hidden);
        let mut payload = json!({
            "chat_id": chat_id,
            "draft_id": draft_id,
            "text": html,
            "can_stop": true,
        });
        set(
            &mut payload,
            "parse_mode",
            (!html.is_empty()).then_some("HTML"),
        );
        let body = JsValue::from_str(&payload.to_string());
        match self
            .post("sendMessageDraft", "application/json", body, 0)
            .await
        {
            Ok(_) => Ok(true),
            // Only what Telegram said counts as a refusal. A request that did not get
            // there says nothing about the next one.
            Err(worker::Error::RustError(said))
                if !said.contains(UNPARSED) && !said.contains("Too Many Requests") =>
            {
                Err(error(said))
            }
            Err(_) => Ok(false),
        }
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let payload = json!({ "chat_id": chat_id, "message_id": message_id });
        self.call("deleteMessage", payload).await.map(drop)
    }

    /// Stops the spinner on a pressed button, optionally with a toast.
    pub async fn answer_callback(&self, callback_id: &str, text: Option<&str>) -> Result<()> {
        let mut payload = json!({ "callback_query_id": callback_id });
        if let Some(text) = text {
            payload["text"] = text.into();
        }
        self.call("answerCallbackQuery", payload).await.map(drop)
    }

    /// Uploads a file to a chat, with `caption` under it if there is one.
    pub async fn send_document(
        &self,
        chat_id: i64,
        file_name: &str,
        bytes: &[u8],
        caption: Option<&str>,
    ) -> Result<()> {
        // Made up for this upload alone. The file is whatever an agent wrote, and one
        // that contained a boundary known beforehand would end its own part early and
        // have the rest of it read as further fields of the request.
        let boundary = format!("agent-worker-{}", random_uuid());
        let file_name: String = file_name
            .chars()
            .map(|c| if c == '"' || c.is_control() { '_' } else { c })
            .collect();
        let field = |name: &str, value: &str| {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
        };
        let mut head = field("chat_id", &chat_id.to_string());
        if let Some(caption) = caption {
            head.push_str(&field("caption", caption));
        }
        head.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; \
             filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        ));
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut body = Vec::with_capacity(head.len() + bytes.len() + tail.len());
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(tail.as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let body = Uint8Array::from(body.as_slice()).into();
        self.post("sendDocument", &content_type, body, RATE_LIMIT_RETRIES)
            .await
            .map(drop)
    }
}
