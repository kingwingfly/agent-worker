//! Plays the Durable Object's part against a running runner, for trying things locally:
//!
//! ```text
//! cargo run -p agent-runner --example drive -- [OPTIONS] PROMPT...
//! ```
//!
//! Claims the runner, sends each prompt as one turn and prints what comes back.
//!
//! - `--addr HOST:PORT`: where the runner listens (default `127.0.0.1:8080`)
//! - `--deny`: decline permission requests instead of allowing them
//! - `--interrupt-after-ms N`: interrupt each turn N milliseconds after it starts
//! - `--raw`: print every frame as JSON
//!
//! Clarifying questions (`AskUserQuestion`) are answered with each question's first option.

use std::time::Duration;

use agent_proto::{Decision, FromRunner, ToRunner};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::header::AUTHORIZATION},
};

const SECRET: &str = "local-development-control-secret-0000";

fn describe(event: &Value) -> Option<String> {
    let kind = event["type"].as_str()?;
    match kind {
        "stream_event" => None,
        "assistant" | "user" => {
            let blocks = event["message"]["content"].as_array()?;
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| match block["type"].as_str()? {
                    "text" => Some(block["text"].as_str()?.to_owned()),
                    "tool_use" => Some(format!("tool_use {} {}", block["name"], block["input"])),
                    "tool_result" => Some(format!("tool_result {}", block["content"])),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| format!("{kind}: {}", parts.join(" | ")))
        }
        "system" if event["subtype"] == "init" => Some(format!(
            "init: model={} permissionMode={} mcp_servers={}",
            event["model"], event["permissionMode"], event["mcp_servers"]
        )),
        _ => Some(format!(
            "{kind}/{}",
            event["subtype"].as_str().unwrap_or("-")
        )),
    }
}

/// Tool input that answers every question with its first option.
fn first_options(input: &Value) -> Value {
    let answers: serde_json::Map<String, Value> = input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let text = question["question"].as_str()?.to_owned();
            Some((text, question["options"][0]["label"].clone()))
        })
        .collect();
    json!({ "questions": input["questions"], "answers": answers })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut addr = "127.0.0.1:8080".to_owned();
    let (mut deny, mut raw) = (false, false);
    let mut interrupt_after = None;
    let mut prompts = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => addr = args.next().expect("--addr needs a value"),
            "--deny" => deny = true,
            "--interrupt-after-ms" => {
                let millis = args.next().expect("--interrupt-after-ms needs a value");
                interrupt_after = Some(Duration::from_millis(millis.parse()?));
            }
            "--raw" => raw = true,
            _ => prompts.push(arg),
        }
    }

    // Raw HTTP/1.0 keeps this example free of an HTTP client dependency.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(&addr).await?;
        let request = format!(
            "POST /claim HTTP/1.0\r\nContent-Length: {}\r\n\r\n{SECRET}",
            SECRET.len()
        );
        stream.write_all(request.as_bytes()).await?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await?;
        let status = response.lines().next().unwrap_or_default();
        let claimed = status.contains("201") || status.contains("204");
        anyhow::ensure!(claimed, "claim failed: {status}");
    }

    let mut request = format!("ws://{addr}/ws").into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {SECRET}").parse()?);
    let (mut socket, _) = connect_async(request).await?;

    // Turn ids must be unique for the runner's lifetime, across invocations of this tool.
    let run = uuid::Uuid::new_v4().simple().to_string();
    let mut prompts = prompts.into_iter().enumerate();
    let mut interrupt_at = None;
    loop {
        let interrupt = async {
            match interrupt_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let message = tokio::select! {
            message = socket.next() => message,
            () = interrupt => {
                interrupt_at = None;
                println!("  -> interrupt");
                let frame = serde_json::to_string(&ToRunner::Interrupt)?;
                socket.send(Message::Text(frame.into())).await?;
                continue;
            }
        };
        let Some(message) = message else { break };
        let Message::Text(text) = message? else {
            continue;
        };
        let frame: FromRunner = serde_json::from_str(&text)?;
        if raw && frame != FromRunner::Heartbeat {
            println!("{text}");
        }
        let reply = match frame {
            FromRunner::Hello { .. } | FromRunner::TurnDone { .. } => {
                if let FromRunner::TurnDone { result, .. } = &frame {
                    println!("turn done: {}", serde_json::to_string(result)?);
                }
                let Some((index, text)) = prompts.next() else {
                    break;
                };
                println!("\n> {text}");
                interrupt_at = interrupt_after.map(|delay| tokio::time::Instant::now() + delay);
                Some(ToRunner::UserMessage {
                    turn_id: format!("{run}-{index}"),
                    text,
                })
            }
            FromRunner::PermissionRequest {
                request_id,
                tool_name,
                input,
                ..
            } => {
                println!("permission request: {tool_name} {input}");
                let decision = if deny {
                    Decision::Deny {
                        message: "The user declined.".into(),
                    }
                } else {
                    Decision::Allow {
                        updated_input: (tool_name == agent_proto::ASK_USER_QUESTION)
                            .then(|| first_options(&input)),
                    }
                };
                println!("  -> {}", if deny { "deny" } else { "allow" });
                Some(ToRunner::PermissionDecision {
                    request_id,
                    decision,
                })
            }
            FromRunner::Event { event, .. } => {
                if let Some(line) = describe(&event).filter(|_| !raw) {
                    println!("{line}");
                }
                None
            }
            FromRunner::Heartbeat => None,
        };
        if let Some(reply) = reply {
            socket
                .send(Message::Text(serde_json::to_string(&reply)?.into()))
                .await?;
        }
    }
    Ok(())
}
