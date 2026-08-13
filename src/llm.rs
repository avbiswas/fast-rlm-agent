//! OpenAI-compatible chat-completions client with SSE streaming.
//!
//! ## History strategy (KV-cache discipline)
//!
//! The assistant message we put back into history is the **raw message object
//! from the API, verbatim** — we never reconstruct it from fields we happen to
//! know about. Whatever the provider includes (`content`, `tool_calls`,
//! `reasoning_content`, `reasoning_details`, signatures, …) is preserved and
//! echoed back exactly. The next request is then a pure prefix extension and
//! the provider's prompt/KV cache keeps hitting.
//!
//! Because we stream (for the live TUI), the raw message is reassembled from
//! SSE deltas by a *generic* merge — strings append, arrays merge by `index`,
//! objects recurse — with no field whitelist, so fields we've never heard of
//! survive too. Non-streaming clients get `choices[0].message` for free; this
//! merge reconstructs the same object.
//!
//! ## Reasoning replay: required by some providers, rejected by others
//!
//! Thinking-mode models (DeepSeek V4 family, Kimi, GLM, …) **require**
//! `reasoning_content` to be replayed on assistant tool-call turns — omitting
//! it is an HTTP 400. Verbatim echo satisfies that automatically. A minority
//! of providers instead **reject** the field on input. Rather than maintain a
//! provider matrix (see CodeWhale's `provider_accepts_reasoning_content` for
//! how big that gets), we self-heal: replay by default, and if the provider
//! 400s mentioning reasoning, strip reasoning keys for the rest of the
//! session (one cache miss, stable thereafter) and retry.

use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::event::Event;

/// One message in the conversation. Kept as raw JSON so assistant turns can
/// be stored exactly as the provider produced them.
pub type Message = serde_json::Value;

pub fn system_msg(content: impl Into<String>) -> Message {
    serde_json::json!({ "role": "system", "content": content.into() })
}

pub fn user_msg(content: impl Into<String>) -> Message {
    serde_json::json!({ "role": "user", "content": content.into() })
}

pub fn tool_msg(tool_call_id: impl Into<String>, content: impl Into<String>) -> Message {
    serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id.into(),
        "content": content.into(),
    })
}

/// A completed tool call requested by the model — a typed *view* parsed out
/// of the raw message for dispatching. The raw message stays the source of
/// truth for history.
#[derive(Deserialize, Clone, Default)]
pub struct AssistantToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub function: FunctionCall,
}

#[derive(Deserialize, Clone, Default)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: String,
    /// JSON-encoded arguments, exactly as the model produced them.
    #[serde(default)]
    pub arguments: String,
}

/// The result of one streamed model turn.
pub struct Turn {
    /// The reassembled assistant message, verbatim — append this to history.
    pub message: Message,
    /// Typed view of `message.tool_calls` for dispatch.
    pub tool_calls: Vec<AssistantToolCall>,
    /// Token accounting from the final stream chunk, when the provider
    /// reports it. `cached_tokens > 0` is the empirical proof that our
    /// prefix-stable history is actually hitting the provider's KV cache.
    pub usage: Option<Usage>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's cache. Read from
    /// `prompt_tokens_details.cached_tokens` (OpenAI style) or
    /// `prompt_cache_hit_tokens` (DeepSeek style).
    pub cached_tokens: u64,
}

/// Accumulates SSE delta chunks into the raw assistant message. Pure, so the
/// reassembly rules are unit-testable without a network.
#[derive(Default)]
struct StreamAcc {
    message: serde_json::Value,
    usage: Option<Usage>,
}

impl StreamAcc {
    fn new() -> Self {
        Self {
            message: serde_json::json!({}),
            ..Self::default()
        }
    }

    /// Apply one parsed SSE JSON chunk. Returns the visible content delta,
    /// if any, for live forwarding to the UI.
    fn apply(&mut self, value: &serde_json::Value) -> Option<String> {
        // Usage arrives on the final chunk (with `include_usage` requested).
        if let Some(u) = value.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage {
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                cached_tokens: u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                    .unwrap_or(0),
            });
        }

        let delta = &value["choices"][0]["delta"];
        if delta.is_object() {
            merge_delta(&mut self.message, delta);
        }

        match delta["content"].as_str() {
            Some(text) if !text.is_empty() => Some(text.to_string()),
            _ => None,
        }
    }

    fn finish(mut self) -> Turn {
        if let Some(obj) = self.message.as_object_mut() {
            obj.entry("role")
                .or_insert_with(|| serde_json::json!("assistant"));
        }
        let tool_calls = self.message["tool_calls"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Turn {
            message: self.message,
            tool_calls,
            usage: self.usage,
        }
    }
}

/// Generic, field-agnostic delta merge (OpenAI streaming semantics):
/// strings append, arrays merge elements by their `index`, objects recurse,
/// scalars overwrite. `index` keys are positional plumbing and are dropped
/// from the result (non-streaming messages don't carry them).
fn merge_delta(target: &mut serde_json::Value, delta: &serde_json::Value) {
    use serde_json::Value;
    let (Some(t), Some(d)) = (target.as_object_mut(), delta.as_object()) else {
        return;
    };
    for (key, value) in d {
        if key == "index" {
            continue;
        }
        match value {
            Value::Null => {}
            Value::String(s) => match t.get_mut(key) {
                Some(Value::String(existing)) => existing.push_str(s),
                _ => {
                    t.insert(key.clone(), value.clone());
                }
            },
            Value::Array(items) => {
                let entry = t
                    .entry(key.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                let Some(arr) = entry.as_array_mut() else {
                    *entry = value.clone();
                    continue;
                };
                for item in items {
                    match item.get("index").and_then(|i| i.as_u64()) {
                        Some(i) => {
                            let i = i as usize;
                            while arr.len() <= i {
                                arr.push(Value::Object(Default::default()));
                            }
                            merge_delta(&mut arr[i], item);
                        }
                        None => arr.push(item.clone()),
                    }
                }
            }
            Value::Object(_) => {
                let entry = t
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Default::default()));
                if entry.is_object() {
                    merge_delta(entry, value);
                } else {
                    *entry = value.clone();
                }
            }
            _ => {
                t.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Session-sticky fallback: set once if the provider rejects reasoning fields
/// on input, after which every request strips them (stable prefix again).
static STRIP_REASONING: AtomicBool = AtomicBool::new(false);

/// Keys holding thinking tokens across known provider dialects.
const REASONING_KEYS: &[&str] = &["reasoning_content", "reasoning", "reasoning_details"];

/// Abort if the provider sends nothing at all for this long. Generous because
/// thinking models can run long turns — but they emit reasoning deltas the
/// whole way, so a truly silent stream is dead, not thinking.
const STREAM_IDLE_TIMEOUT_SECS: u64 = 180;

/// Remove reasoning keys from assistant messages (for providers that reject
/// them on input). Everything else is left untouched.
fn strip_reasoning_fields(messages: &mut [Message]) {
    for msg in messages {
        if msg["role"] == "assistant" {
            if let Some(obj) = msg.as_object_mut() {
                for key in REASONING_KEYS {
                    obj.remove(*key);
                }
            }
        }
    }
}

/// Does this API error indicate the provider rejected replayed reasoning?
fn is_reasoning_rejection(error: &str) -> bool {
    error.contains("HTTP 4") && error.to_lowercase().contains("reasoning")
}

/// Stream one completion. Content deltas are sent to the UI as they arrive;
/// the assembled turn (raw message + typed tool calls) is returned at the end.
///
/// History goes out verbatim (reasoning included). If the provider rejects
/// that with a reasoning-related 4xx, we flip to stripping reasoning keys for
/// the rest of the session and retry once.
pub async fn stream_chat(
    cfg: &Config,
    messages: &[Message],
    tx: &UnboundedSender<Event>,
) -> Result<Turn, String> {
    let strip = STRIP_REASONING.load(Ordering::Relaxed);
    match attempt_stream(cfg, messages, tx, strip).await {
        Err(err) if !strip && is_reasoning_rejection(&err) => {
            STRIP_REASONING.store(true, Ordering::Relaxed);
            let _ = tx.send(Event::Agent(AgentEvent::Delta(
                "⚠ provider rejected reasoning replay — stripping reasoning fields for this session\n"
                    .to_string(),
            )));
            attempt_stream(cfg, messages, tx, true).await
        }
        other => other,
    }
}

async fn attempt_stream(
    cfg: &Config,
    messages: &[Message],
    tx: &UnboundedSender<Event>,
    strip_reasoning: bool,
) -> Result<Turn, String> {
    let api_key = cfg
        .api_key
        .clone()
        .ok_or("API_KEY is not set — export it (or add it to .envrc) and restart")?;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));

    let messages = if strip_reasoning {
        let mut stripped = messages.to_vec();
        strip_reasoning_fields(&mut stripped);
        stripped
    } else {
        messages.to_vec()
    };

    let body = serde_json::json!({
        "model": cfg.model,
        "messages": messages,
        "tools": tool_schemas(),
        "stream": true,
        // Ask for token accounting on the final chunk so we can verify
        // prompt-cache hits empirically.
        "stream_options": { "include_usage": true },
    });

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("model API returned HTTP {status}: {body}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut acc = StreamAcc::new();

    loop {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(STREAM_IDLE_TIMEOUT_SECS),
            stream.next(),
        )
        .await
        .map_err(|_| {
            format!("stream stalled: no data for {STREAM_IDLE_TIMEOUT_SECS}s — giving up")
        })?;
        let Some(chunk) = next else {
            break; // stream finished
        };
        let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // SSE: process every complete line in the buffer.
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            if let Some(delta) = acc.apply(&value) {
                let _ = tx.send(Event::Agent(AgentEvent::Delta(delta)));
            }
        }
    }

    Ok(acc.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stream_acc_assembles_interleaved_fragments() {
        let chunks = [
            json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"thinking"}}]}),
            json!({"choices":[{"delta":{"reasoning_content":" hard"}}]}),
            json!({"choices":[{"delta":{"content":"Hel"}}]}),
            json!({"choices":[{"delta":{"content":"lo"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"re"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"ad","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"bash","arguments":"{}"}}]}}]}),
        ];

        let mut acc = StreamAcc::new();
        let mut forwarded = String::new();
        for chunk in &chunks {
            if let Some(delta) = acc.apply(chunk) {
                forwarded.push_str(&delta);
            }
        }
        let turn = acc.finish();

        // The raw message is the reassembled non-streaming equivalent.
        assert_eq!(turn.message["role"], "assistant");
        assert_eq!(turn.message["content"], "Hello");
        assert_eq!(turn.message["reasoning_content"], "thinking hard");
        assert_eq!(turn.message["tool_calls"][0]["id"], "call_1");
        assert_eq!(turn.message["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            turn.message["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"x"}"#
        );
        // Streaming plumbing (`index`) is gone from the final object.
        assert!(turn.message["tool_calls"][0].get("index").is_none());

        // The typed view and forwarded deltas agree with it.
        assert_eq!(forwarded, "Hello");
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].function.name, "read");
        assert_eq!(turn.tool_calls[1].function.name, "bash");
    }

    #[test]
    fn merge_preserves_unknown_provider_fields_verbatim() {
        // Fields we've never heard of — OpenRouter-style reasoning_details
        // with signatures, or anything else — must survive reassembly, since
        // the whole message is echoed back into history.
        let chunks = [
            json!({"choices":[{"delta":{
                "role":"assistant",
                "content":"hi",
                "signature":"abc",
                "reasoning_details":[{"index":0,"type":"reasoning.text","text":"th"}]
            }}]}),
            json!({"choices":[{"delta":{
                "signature":"def",
                "reasoning_details":[{"index":0,"text":"ink"}],
                "some_new_thing":{"nested":{"n":1}}
            }}]}),
        ];

        let mut acc = StreamAcc::new();
        for chunk in &chunks {
            acc.apply(chunk);
        }
        let msg = acc.finish().message;

        assert_eq!(msg["signature"], "abcdef"); // unknown strings concatenate
        assert_eq!(msg["reasoning_details"][0]["type"], "reasoning.text");
        assert_eq!(msg["reasoning_details"][0]["text"], "think");
        assert_eq!(msg["some_new_thing"]["nested"]["n"], 1);
    }

    #[test]
    fn stream_acc_parses_usage_both_styles() {
        // OpenAI style: prompt_tokens_details.cached_tokens
        let mut acc = StreamAcc::new();
        acc.apply(&json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 1500, "completion_tokens": 42,
                "prompt_tokens_details": { "cached_tokens": 1280 }
            }
        }));
        let u = acc.finish().usage.unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.cached_tokens),
            (1500, 42, 1280)
        );

        // DeepSeek style: prompt_cache_hit_tokens
        let mut acc = StreamAcc::new();
        acc.apply(&json!({
            "choices": [],
            "usage": { "prompt_tokens": 900, "completion_tokens": 10, "prompt_cache_hit_tokens": 768 }
        }));
        assert_eq!(acc.finish().usage.unwrap().cached_tokens, 768);

        // No usage reported → None, and a null usage field doesn't panic.
        let mut acc = StreamAcc::new();
        acc.apply(&json!({"choices":[{"delta":{"content":"x"}}], "usage": null}));
        assert!(acc.finish().usage.is_none());
    }

    #[test]
    fn strip_only_removes_reasoning_keys_from_assistant_messages() {
        let mut messages = vec![
            system_msg("sys"),
            user_msg("hi"),
            json!({
                "role": "assistant",
                "content": "x",
                "reasoning_content": "secret thoughts",
                "reasoning": "more",
                "reasoning_details": [{"text":"t"}],
                "signature": "keep-me",
                "tool_calls": [{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]
            }),
            // A user message with a coincidental reasoning key is untouched.
            json!({"role": "user", "content": "y", "reasoning_content": "not-assistant"}),
        ];
        strip_reasoning_fields(&mut messages);

        let assistant = &messages[2];
        for key in REASONING_KEYS {
            assert!(assistant.get(*key).is_none(), "{key} not stripped");
        }
        // Non-reasoning fields survive verbatim.
        assert_eq!(assistant["signature"], "keep-me");
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
        assert_eq!(assistant["content"], "x");
        // Other roles untouched.
        assert_eq!(messages[3]["reasoning_content"], "not-assistant");
    }

    #[test]
    fn reasoning_rejection_detection() {
        assert!(is_reasoning_rejection(
            "model API returned HTTP 400 Bad Request: {\"error\":{\"message\":\"reasoning_content is not supported in input messages\"}}"
        ));
        assert!(is_reasoning_rejection(
            "model API returned HTTP 422: invalid field `reasoning`"
        ));
        // Unrelated 400s and reasoning-mentioning 500s don't trigger the strip.
        assert!(!is_reasoning_rejection(
            "model API returned HTTP 400: context length exceeded"
        ));
        assert!(!is_reasoning_rejection(
            "model API returned HTTP 500: reasoning engine crashed"
        ));
        assert!(!is_reasoning_rejection("stream stalled: no data for 180s"));
    }

    #[test]
    fn tool_schemas_are_deterministic() {
        // The tools array is part of the cached prefix on most providers —
        // it must serialize to identical bytes on every request.
        assert_eq!(
            serde_json::to_string(&tool_schemas()).unwrap(),
            serde_json::to_string(&tool_schemas()).unwrap()
        );
    }

    /// Live API tests — run with `cargo test -- --ignored` in a direnv shell.
    #[tokio::test]
    #[ignore]
    async fn live_stream_text() {
        let cfg = Config::from_env();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let messages = vec![user_msg("Reply with exactly: OK")];
        let turn = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("stream failed");
        let content = turn.message["content"].as_str().unwrap_or("");
        assert!(content.contains("OK"), "content: {content}");
        assert_eq!(turn.message["role"], "assistant");
        assert!(turn.tool_calls.is_empty());
        // Deltas were forwarded to the UI channel as they streamed.
        assert!(rx.try_recv().is_ok(), "no deltas were emitted");
    }

    /// Empirical cache verification: a second request whose messages are a
    /// prefix-extension of the first must report cached prompt tokens.
    #[tokio::test]
    #[ignore]
    async fn live_kv_cache_actually_hits() {
        let cfg = Config::from_env();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Padding pushes the shared prefix well past minimum cacheable size
        // (e.g. OpenAI only caches prompts ≥ 1024 tokens).
        let padding = "The quick brown fox jumps over the lazy dog. ".repeat(150);
        let mut messages = vec![
            system_msg(format!("You are a helpful assistant. Context: {padding}")),
            user_msg("Reply with exactly: ONE"),
        ];
        let t1 = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("turn 1 failed");
        let u1 = t1.usage.expect("provider did not report usage");
        assert!(u1.prompt_tokens > 1000, "padding too small: {u1:?}");

        // Extend with the raw message — identical prefix, new tail.
        messages.push(t1.message);
        messages.push(user_msg("Now reply with exactly: TWO"));
        let t2 = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("turn 2 failed");
        let u2 = t2.usage.expect("provider did not report usage");
        assert!(
            u2.cached_tokens > 0,
            "second request reported zero cached tokens — prefix not hitting the KV cache: {u2:?}"
        );
    }

    /// The full history shape we persist — the raw assistant message
    /// (tool_calls, reasoning and all) followed by tool results — must be
    /// accepted by the provider on the next request and actually used.
    #[tokio::test]
    #[ignore]
    async fn live_history_round_trip_with_tools_and_reasoning() {
        let cfg = Config::from_env();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let mut messages = vec![user_msg(
            "Read secret.txt with the read tool, then tell me the magic number it contains.",
        )];
        let t1 = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("turn 1 failed");
        assert!(
            !t1.tool_calls.is_empty(),
            "no tool call; said: {}",
            t1.message["content"]
        );
        let call_id = t1.tool_calls[0].id.clone();

        // Persist the turn exactly as the agent loop does: raw message, verbatim.
        messages.push(t1.message);
        messages.push(tool_msg(call_id, "The magic number is 4217."));

        let t2 = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("turn 2 failed");
        let content = t2.message["content"].as_str().unwrap_or("");
        assert!(
            content.contains("4217"),
            "model didn't use the preserved tool result; said: {content}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_model_uses_edit_tool() {
        let cfg = Config::from_env();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let messages = vec![user_msg(
            "The file config.txt contains exactly one line: `debug = false`. \
             Call the edit tool to change it to `debug = true`. \
             Do not call any other tool and do not answer in text.",
        )];
        let turn = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("stream failed");
        assert!(
            !turn.tool_calls.is_empty(),
            "no tool call; said: {}",
            turn.message["content"]
        );
        let call = &turn.tool_calls[0];
        assert_eq!(call.function.name, "edit");
        let args: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(args["path"].as_str().unwrap(), "config.txt");
        assert_eq!(args["old_string"].as_str().unwrap(), "debug = false");
        assert_eq!(args["new_string"].as_str().unwrap(), "debug = true");
    }

    #[tokio::test]
    #[ignore]
    async fn live_tool_call_assembly() {
        let cfg = Config::from_env();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let messages = vec![user_msg(
            "Use the read tool to read the file Cargo.toml. Do not answer in text.",
        )];
        let turn = stream_chat(&cfg, &messages, &tx)
            .await
            .expect("stream failed");
        assert!(
            !turn.tool_calls.is_empty(),
            "model made no tool call; said: {}",
            turn.message["content"]
        );
        let call = &turn.tool_calls[0];
        assert_eq!(call.function.name, "read");
        let args: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(args["path"].as_str().unwrap(), "Cargo.toml");
        assert!(!call.id.is_empty());
    }
}

/// The tool surface we advertise to the model — one entry per `ToolCall`
/// variant. Keep these in sync with `tools::execute` and `agent::dispatch`.
fn tool_schemas() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file and return its contents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path, relative to the working directory." }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write",
                "description": "Create or overwrite a file. Prefer the edit tool for modifying existing files. The user reviews a diff and may reject the change.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string", "description": "Full new file contents." }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Replace old_string with new_string in a file. old_string must match the file content exactly (including whitespace and indentation) and must be unique in the file unless replace_all is set. The user reviews a diff and may reject the change.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "description": "Exact text to replace. Include enough surrounding context to be unique." },
                        "new_string": { "type": "string", "description": "The replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence. Default false." }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command via `sh -c`. Requires user approval. Returns stdout+stderr and the exit code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web. Returns ranked results with titles, URLs and extracted page text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "fetch",
                "description": "Fetch a URL and return its content. HTML is converted to markdown by default (token-efficient); pass format=\"text\" for plain text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "format": { "type": "string", "enum": ["markdown", "text"] }
                    },
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ask_question",
                "description": "Ask the user a multiple-choice question when you need a decision between concrete options. Returns the selected option label(s).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "header": { "type": "string", "description": "Very short label, max ~12 chars (e.g. \"Language\")." },
                        "question": { "type": "string", "description": "The full question to ask." },
                        "multi_select": { "type": "boolean", "description": "Allow selecting multiple options. Default false." },
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "description": { "type": "string" }
                                },
                                "required": ["label", "description"]
                            }
                        }
                    },
                    "required": ["header", "question", "options"]
                }
            }
        }
    ])
}
