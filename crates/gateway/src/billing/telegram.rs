use serde_json::{Value, json};
use worker::{Env, Result};

use super::post;
use crate::{
    bot::Bot,
    config::Config,
    platform::{error, expect_ok},
    render,
    update::{Origin, Update},
};

const UNAVAILABLE: &str = "Account services are temporarily unavailable. Please try again shortly.";

/// Money-bearing webhook updates must be retried by Telegram if the ledger is unavailable.
/// A completed purchase still belongs to its payer if the allow-list changed since checkout.
pub async fn payment_update(env: &Env, cfg: &Config, body: &str) -> Result<bool> {
    let Ok(update) = serde_json::from_str::<Value>(body) else {
        return Ok(false);
    };
    let precheck = &update["pre_checkout_query"];
    let message = &update["message"];
    let (kind, event, user) = if !precheck.is_null() {
        ("precheck", precheck, precheck["from"]["id"].as_i64())
    } else if !message["successful_payment"].is_null() {
        (
            "paid",
            &message["successful_payment"],
            message["from"]["id"].as_i64(),
        )
    } else if !message["refunded_payment"].is_null() {
        (
            "refund",
            &message["refunded_payment"],
            message["chat"]["id"].as_i64(),
        )
    } else {
        return Ok(false);
    };
    let user = user.ok_or_else(|| error("payment update missing its payer"))?;
    let payload = json!({
        "kind": kind, "user": user.to_string(), "invoice": event["invoice_payload"],
        "amount": event["total_amount"], "currency": event["currency"],
        "charge": event["telegram_payment_charge_id"], "query": precheck["id"],
    });
    if kind == "precheck" {
        let mut answer = json!({ "pre_checkout_query_id": precheck["id"], "ok": false });
        if cfg.allowed_users.admits(user) {
            let mut response = post(env, "stars", payload).await?;
            if response.status_code() == 200 {
                answer["ok"] = true.into();
            } else {
                let reason = response.json::<Value>().await?;
                answer["error_message"] = reason["error"].as_str().unwrap_or(UNAVAILABLE).into();
            }
        } else {
            answer["error_message"] = "This account cannot buy tokens.".into();
        }
        Bot::new(cfg).call("answerPreCheckoutQuery", answer).await?;
    } else {
        expect_ok(
            post(env, "stars", payload).await?,
            "recording a Telegram payment",
        )
        .await?;
    }
    Ok(true)
}

/// Only private messages can issue browser sign-in codes or invoices. A guest's
/// refusal uses its one reply. Buttons and control commands keep their normal route.
pub async fn intercept(env: &Env, cfg: &Config, update: &Update, origin: &Origin) -> Result<bool> {
    let Some(message) = update.message.as_ref().or(update.guest_message.as_ref()) else {
        return Ok(false);
    };
    let guest = update.guest_message.is_some();
    let text = message.text.as_deref().unwrap_or_default().trim();
    let text = if guest {
        render::without_mentions(text)
    } else {
        text
    };
    let mut words = text.split_whitespace();
    let action = match words.next().and_then(|s| s.split('@').next()) {
        Some("/start") => "welcome",
        Some("/account") => "account",
        Some("/topup" | "/pay") => "topup",
        Some("/subscribe") => "subscribe",
        Some("/unsubscribe") => "unsubscribe",
        Some("/paysupport") => "support",
        Some(word) if word.starts_with('/') => return Ok(false),
        _ if text.is_empty() && !(guest && message.replied().is_some()) => return Ok(false),
        _ => "access",
    };
    let bot = Bot::new(cfg);
    let private = !guest && message.chat.is_private() && message.chat.id == origin.user_id;
    let notice = if !matches!(action, "access" | "welcome") && !private {
        "Send this command in a private chat with me.".to_owned()
    } else {
        let mut payload = json!({ "user": origin.user_id.to_string() });
        if let Some(argument) = words.next() {
            match action {
                "subscribe" => payload["quote"] = argument.into(),
                "topup" => payload["usd"] = argument.into(),
                _ => {}
            }
        }
        let response = post(env, action, payload).await;
        match response {
            Ok(mut response) if response.status_code() == 200 => {
                if matches!(action, "access" | "welcome") {
                    return Ok(false);
                }
                let value: Value = response.json().await?;
                if action == "topup" {
                    let amount: u64 = value["amount"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| error("invalid Stars price"))?;
                    let description = format!(
                        "{} tokens for ${} USD. Use them to chat or pay for the monthly plan.",
                        value["tokens"],
                        value["usd"].as_str().unwrap_or_default()
                    );
                    bot.call("sendInvoice", json!({
                        "chat_id": message.chat.id, "title": "Agent tokens", "description": description,
                        "payload": value["id"], "start_parameter": value["id"], "currency": "XTR",
                        "provider_token": "", "prices": [{"label": "Tokens", "amount": amount}]
                    })).await?;
                    return Ok(true);
                }
                value["text"].as_str().unwrap_or(UNAVAILABLE).to_owned()
            }
            Ok(mut response) => {
                let problem = response.json::<Value>().await.unwrap_or_default();
                problem["error"].as_str().unwrap_or(UNAVAILABLE).to_owned()
            }
            Err(cause) => {
                worker::console_error!("could not reach billing: {cause}");
                UNAVAILABLE.to_owned()
            }
        }
    };
    let html = render::escape(&notice);
    if let Some(query) = &message.guest_query_id {
        bot.answer_guest(query, &html, None).await?;
    } else {
        bot.send_message(message.chat.id, &html, None).await?;
    }
    Ok(true)
}
