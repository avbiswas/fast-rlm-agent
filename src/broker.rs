//! Authenticated loopback bridge from FastRLM's Pyodide tools to the Rust
//! workspace tool/approval pipeline.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::task::JoinHandle;

use crate::agent::AgentEvent;
use crate::event::Event;
use crate::tools::{ToolCall, ToolResult};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

pub struct Broker {
    url: String,
    token: String,
    task: JoinHandle<()>,
}

impl Broker {
    pub async fn start(tx: UnboundedSender<Event>) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("could not bind workspace tool broker: {e}"))?;
        let address = listener
            .local_addr()
            .map_err(|e| format!("could not inspect workspace tool broker: {e}"))?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let token = format!("{}-{nonce:x}", std::process::id());
        let expected = Arc::new(token.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, &expected, tx).await;
                });
            }
        });
        Ok(Self {
            url: format!("http://{address}/tool"),
            token,
            task,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Deserialize)]
struct Request {
    token: String,
    #[serde(flatten)]
    call: WireCall,
}

#[derive(Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
enum WireCall {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        old_string: String,
        new_string: String,
        #[serde(default)]
        replace_all: bool,
    },
    #[serde(alias = "run_command")]
    Bash {
        command: String,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    Skill {
        #[serde(default)]
        path: Option<String>,
    },
}

impl From<WireCall> for ToolCall {
    fn from(call: WireCall) -> Self {
        match call {
            WireCall::ReadFile { path } => Self::Read { path },
            WireCall::WriteFile { path, content } => Self::Write { path, content },
            WireCall::EditFile {
                path,
                old_string,
                new_string,
                replace_all,
            } => Self::Edit {
                path,
                old_string,
                new_string,
                replace_all,
            },
            WireCall::Bash {
                command,
                timeout_seconds,
            } => Self::Bash {
                command,
                timeout_seconds,
            },
            WireCall::Skill { path } => Self::Skill { path },
        }
    }
}

async fn handle(
    mut stream: TcpStream,
    expected_token: &str,
    tx: UnboundedSender<Event>,
) -> Result<(), String> {
    let body = read_http_body(&mut stream).await?;
    let request: Request = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if request.token != expected_token {
        return write_json(&mut stream, 403, false, "invalid broker token").await;
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(Event::Agent(AgentEvent::Tool(
        request.call.into(),
        reply_tx,
    )))
    .map_err(|_| "application event loop closed".to_string())?;

    match reply_rx.await {
        Ok(ToolResult::Output { ok, text }) => write_json(&mut stream, 200, ok, &text).await,
        Ok(ToolResult::Question { .. }) => {
            write_json(&mut stream, 500, false, "unexpected question result").await
        }
        Err(_) => write_json(&mut stream, 503, false, "tool request cancelled").await,
    }
}

async fn read_http_body(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let header_end = loop {
        if data.len() >= MAX_REQUEST_BYTES {
            return Err("workspace tool request is too large".to_string());
        }
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("incomplete workspace tool request".to_string());
        }
        data.extend_from_slice(&chunk[..count]);
        if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&data[..header_end]).map_err(|e| e.to_string())?;
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .map(str::to_owned)
        })
        .ok_or("missing Content-Length")?
        .parse::<usize>()
        .map_err(|_| "invalid Content-Length".to_string())?;
    if length > MAX_REQUEST_BYTES || header_end + length > MAX_REQUEST_BYTES {
        return Err("workspace tool request is too large".to_string());
    }
    while data.len() < header_end + length {
        let count = stream
            .read_buf(&mut data)
            .await
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("incomplete workspace tool body".to_string());
        }
    }
    Ok(data[header_end..header_end + length].to_vec())
}

async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    ok: bool,
    text: &str,
) -> Result<(), String> {
    let body = serde_json::to_vec(&serde_json::json!({ "ok": ok, "text": text }))
        .map_err(|e| e.to_string())?;
    let reason = if status == 200 { "OK" } else { "Error" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn forwards_authenticated_request_and_returns_tool_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let broker = Broker::start(tx).await.unwrap();
        let client = reqwest::Client::new();
        let request = client.post(broker.url()).json(&serde_json::json!({
            "token": broker.token(),
            "tool": "write_file",
            "path": "demo.txt",
            "content": "hello"
        }));
        let response_task = tokio::spawn(async move { request.send().await.unwrap() });

        let Event::Agent(AgentEvent::Tool(ToolCall::Write { path, content }, responder)) =
            rx.recv().await.unwrap()
        else {
            panic!("expected write tool event")
        };
        assert_eq!(path, "demo.txt");
        assert_eq!(content, "hello");
        assert!(responder
            .send(ToolResult::Output {
                ok: true,
                text: "wrote demo.txt".to_string(),
            })
            .is_ok());

        let response = response_task.await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body,
            serde_json::json!({"ok": true, "text": "wrote demo.txt"})
        );
        broker.stop();
    }

    #[test]
    fn parses_bash_and_skill_requests() {
        let bash: Request = serde_json::from_value(serde_json::json!({
            "token": "test",
            "tool": "bash",
            "command": "cargo test",
            "timeout_seconds": 300
        }))
        .unwrap();
        assert!(matches!(
            ToolCall::from(bash.call),
            ToolCall::Bash {
                command,
                timeout_seconds: Some(300)
            } if command == "cargo test"
        ));

        let legacy: Request = serde_json::from_value(serde_json::json!({
            "token": "test",
            "tool": "run_command",
            "command": "cargo test"
        }))
        .unwrap();
        assert!(matches!(
            ToolCall::from(legacy.call),
            ToolCall::Bash {
                command,
                timeout_seconds: None
            } if command == "cargo test"
        ));

        let skill: Request = serde_json::from_value(serde_json::json!({
            "token": "test",
            "tool": "skill",
            "path": ".agents/skills/testing/SKILL.md"
        }))
        .unwrap();
        assert!(matches!(
            ToolCall::from(skill.call),
            ToolCall::Skill { path: Some(path) }
                if path == ".agents/skills/testing/SKILL.md"
        ));
    }
}
