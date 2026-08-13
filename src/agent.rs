//! The agent: a real model-driven tool loop.
//!
//! Each user message starts a turn: we stream a completion from the model
//! (`llm::stream_chat`); if it requests tools, we dispatch them through the
//! harness — which renders them, gates destructive ones behind approval, and
//! executes — then feed the results back and loop until the model answers
//! with plain text (or we hit the round cap).
//!
//! ## History & KV-cache discipline
//!
//! The session history is shared (`SharedHistory`) and **append-only**: the
//! system prompt is computed once, nothing is ever rewritten or dropped, and
//! assistant turns are stored as the **raw message object from the API,
//! verbatim** — content, tool_calls, reasoning, and any provider-specific
//! fields we've never heard of. We never reconstruct messages from parsed
//! fields. Every request is therefore a byte-stable prefix extension of the
//! previous one — exactly what provider prompt/KV caches need to hit.
//!
//! Commits are *round-complete*: a model turn and all of its tool results are
//! appended together, only after the tools finish. A cancelled turn can never
//! leave a dangling `tool_calls` without matching `tool` replies (which
//! providers reject) — the partial round is simply dropped.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::context::PromptContext;
use crate::event::Event;
use crate::llm::{self, AssistantToolCall, Message};
use crate::tools::{Choice, FetchFormat, Question, ToolCall, ToolResult};

/// Hard cap on model⇄tool rounds per user message.
const MAX_ROUNDS: usize = 16;

/// The append-only conversation history, shared between the app (owner) and
/// in-flight agent tasks (appenders).
pub type SharedHistory = Arc<Mutex<Vec<Message>>>;

/// Start a session: a fresh history seeded with the (stable) system prompt.
pub fn new_session() -> SharedHistory {
    Arc::new(Mutex::new(vec![llm::system_msg(system_prompt())]))
}

/// Messages produced by the agent as it works.
pub enum AgentEvent {
    /// A chunk of assistant text to append to the in-flight reply.
    Delta(String),
    /// A request for the harness to run a tool; `respond` carries the result.
    Tool {
        call: ToolCall,
        respond: oneshot::Sender<ToolResult>,
    },
    /// Token accounting for the latest model request (incl. cache hits).
    Usage(llm::Usage),
    /// The turn is complete.
    Done,
}

/// Kick off a response. Returns the task handle so the harness can `.abort()`
/// to cancel an in-flight turn.
pub fn respond(
    context: PromptContext,
    config: Config,
    history: SharedHistory,
    tx: UnboundedSender<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run(context, &config, &history, &tx).await {
            let _ = tx.send(Event::Agent(AgentEvent::Delta(format!("⚠ {err}"))));
        }
        let _ = tx.send(Event::Agent(AgentEvent::Done));
    })
}

async fn run(
    context: PromptContext,
    cfg: &Config,
    history: &SharedHistory,
    tx: &UnboundedSender<Event>,
) -> Result<(), String> {
    // Commit the user message, then work on a local copy of the history.
    let mut messages = {
        let mut h = history.lock().unwrap();
        h.push(llm::user_msg(context.to_user_content()));
        h.clone()
    };

    for _ in 0..MAX_ROUNDS {
        let turn = llm::stream_chat(cfg, &messages, tx).await?;
        if let Some(usage) = turn.usage {
            let _ = tx.send(Event::Agent(AgentEvent::Usage(usage)));
        }

        // The raw assistant message goes into history verbatim — never
        // reconstructed from parsed fields (fast-rlm strategy).
        let assistant = turn.message;

        if turn.tool_calls.is_empty() {
            // Plain answer — commit and finish the turn.
            history.lock().unwrap().push(assistant);
            return Ok(());
        }

        // Run all tools first, then commit the round atomically so history
        // never holds tool_calls without their results.
        let mut round = vec![assistant];
        for call in &turn.tool_calls {
            let result = dispatch(call, tx).await?;
            round.push(llm::tool_msg(call.id.clone(), result));
        }
        messages.extend(round.iter().cloned());
        history.lock().unwrap().extend(round);
    }

    Err(format!("stopped after {MAX_ROUNDS} tool rounds"))
}

fn system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    format!(
        "You are fast-rlm-agent, a coding agent running in a terminal harness.\n\
         Working directory: {cwd}\n\n\
         User messages are JSON context dictionaries with `prompt`, `links`, and \
         `files` keys. Follow `prompt`; use `links` for referenced URLs and \
         `files` for preloaded workspace files (each has `path` and `content`).\n\n\
         Use the available tools to inspect and modify the project, run commands, \
         search the web, and fetch pages. File paths are relative to the working \
         directory. Prefer edit over write for changing existing files. \
         Destructive tools (write, edit, bash) require user approval and may \
         be rejected — respect rejections instead of retrying. Use ask_question \
         when you need the user to choose between concrete options.\n\n\
         Keep responses concise and use markdown. When the task is done, briefly \
         summarize what you did."
    )
}

/// Map a model tool call onto the harness's `ToolCall`, run it through the
/// UI (rendering + approval + execution), and format the result for the model.
async fn dispatch(call: &AssistantToolCall, tx: &UnboundedSender<Event>) -> Result<String, String> {
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
    let str_arg = |key: &str| args[key].as_str().unwrap_or("").to_string();

    let tool_call = match call.function.name.as_str() {
        "read" => ToolCall::Read {
            path: str_arg("path"),
        },
        "write" => ToolCall::Write {
            path: str_arg("path"),
            content: str_arg("content"),
        },
        "edit" => ToolCall::Edit {
            path: str_arg("path"),
            old_string: str_arg("old_string"),
            new_string: str_arg("new_string"),
            replace_all: args["replace_all"].as_bool().unwrap_or(false),
        },
        "bash" => ToolCall::Bash {
            command: str_arg("command"),
        },
        "web_search" => ToolCall::WebSearch {
            query: str_arg("query"),
        },
        "fetch" => ToolCall::Fetch {
            url: str_arg("url"),
            format: match args["format"].as_str() {
                Some("text") => FetchFormat::Text,
                _ => FetchFormat::Markdown,
            },
        },
        "ask_question" => {
            let options: Vec<Choice> = args["options"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|o| Choice {
                            label: o["label"].as_str().unwrap_or("").to_string(),
                            description: o["description"].as_str().unwrap_or("").to_string(),
                        })
                        .filter(|c| !c.label.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if options.is_empty() {
                return Ok("error: ask_question requires a non-empty options array".to_string());
            }
            ToolCall::AskQuestion(Question {
                header: {
                    let h = str_arg("header");
                    if h.is_empty() {
                        "Question".to_string()
                    } else {
                        h
                    }
                },
                question: str_arg("question"),
                multi_select: args["multi_select"].as_bool().unwrap_or(false),
                options,
            })
        }
        unknown => return Ok(format!("error: unknown tool '{unknown}'")),
    };

    // Keep option labels so we can express the user's selection back as text.
    let labels: Vec<String> = match &tool_call {
        ToolCall::AskQuestion(q) => q.options.iter().map(|o| o.label.clone()).collect(),
        _ => Vec::new(),
    };

    match call_tool(tx, tool_call).await? {
        ToolResult::Output { ok, text } => Ok(if ok { text } else { format!("ERROR: {text}") }),
        ToolResult::Question { selected } => {
            let chosen: Vec<&str> = selected
                .iter()
                .filter_map(|&i| labels.get(i).map(String::as_str))
                .collect();
            Ok(format!("User selected: {}", chosen.join(", ")))
        }
    }
}

/// Send a tool call to the harness and await its reply.
async fn call_tool(tx: &UnboundedSender<Event>, call: ToolCall) -> Result<ToolResult, String> {
    let (respond, rx) = oneshot::channel();
    tx.send(Event::Agent(AgentEvent::Tool { call, respond }))
        .map_err(|_| "UI channel closed".to_string())?;
    // Err => the harness dropped the responder (turn cancelled).
    rx.await.map_err(|_| "tool call cancelled".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_prefix_is_stable() {
        // The cache-critical invariant: a session's opening bytes are
        // deterministic — two sessions in the same process serialize the
        // system prompt identically.
        let a = new_session();
        let b = new_session();
        let ser = |h: &SharedHistory| serde_json::to_string(&*h.lock().unwrap()).unwrap();
        assert_eq!(ser(&a), ser(&b));

        let first = ser(&a);
        assert!(first.contains("\"role\":\"system\""));

        // Appending extends the serialized array without disturbing the
        // existing prefix (append-only ⇒ prefix-stable requests).
        a.lock().unwrap().push(llm::user_msg("hello"));
        let second = ser(&a);
        let prefix = first.trim_end_matches(']');
        assert!(
            second.starts_with(prefix),
            "history serialization is not prefix-stable"
        );
    }
}
