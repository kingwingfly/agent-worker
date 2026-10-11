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

    async fn post(&self, method: &str, content_type: &str, body: JsValue) -> Result<Value> {
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
                Some(seconds)
                    if waits < RATE_LIMIT_RETRIES && seconds <= RATE_LIMIT_MAX_WAIT_SECS =>
                {
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
        self.post(method, "application/json", body).await
    }

    /// Sends HTML and returns the new message's id. If Telegram cannot parse the markup,
    /// the text goes out plain rather than not at all.
    pub async fn send_message(
        &self,
        chat_id: i64,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<i64> {
        let html = &scrub::redact(html, &self.hidden);
        let mut payload = json!({
            "chat_id": chat_id,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(keyboard) = keyboard {
            payload["reply_markup"] = keyboard.clone();
        }
        let sent = match self.call("sendMessage", payload.clone()).await {
            Err(error) if error.to_string().contains("can't parse entities") => {
                payload["text"] = render::to_plain(html).into();
                payload
                    .as_object_mut()
                    .expect("object")
                    .remove("parse_mode");
                self.call("sendMessage", payload).await?
            }
            other => other?,
        };
        sent["message_id"]
            .as_i64()
            .ok_or_else(|| error("telegram sendMessage: no message id"))
    }

    /// Replaces a message's text. Omitting the keyboard removes any buttons it had.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<()> {
        let html = &scrub::redact(html, &self.hidden);
        let mut payload = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(keyboard) = keyboard {
            payload["reply_markup"] = keyboard.clone();
        }
        match self.call("editMessageText", payload).await {
            Err(error) if !error.to_string().contains("message is not modified") => Err(error),
            _ => Ok(()),
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

    pub async fn send_document(&self, chat_id: i64, file_name: &str, bytes: &[u8]) -> Result<()> {
        // Made up for this upload alone. The file is whatever an agent wrote, and one
        // that contained a boundary known beforehand would end its own part early and
        // have the rest of it read as further fields of the request.
        let boundary = format!("agent-worker-{}", random_uuid());
        let file_name: String = file_name
            .chars()
            .map(|c| if c == '"' || c.is_control() { '_' } else { c })
            .collect();
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n{chat_id}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"document\"; \
             filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut body = Vec::with_capacity(head.len() + bytes.len() + tail.len());
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(tail.as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let body = Uint8Array::from(body.as_slice()).into();
        self.post("sendDocument", &content_type, body)
            .await
            .map(drop)
    }
}
