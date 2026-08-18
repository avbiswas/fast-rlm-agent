//! FastRLM process bridge and live event adapter.
//!
//! Each user turn is passed to FastRLM as a real structured dictionary. The
//! Python runner owns the recursive REPL/session; this module reads its NDJSON
//! event stream and translates it into reducer-friendly `AgentEvent`s.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::context::PromptContext;
use crate::event::Event;
use crate::tools::{Responder, ToolCall};

/// System instruction appended to every agent's prompt.
///
/// Two rules here exist because of concrete failures observed in run logs:
///
/// * The REPL is a Pyodide sandbox with its own empty filesystem. Models
///   assume `open()` and `os.listdir` see the project, write the artifact into
///   the sandbox, then shell out to verify it and find nothing there.
/// * Sub-agents inherit no MCP access (FastRLM grants servers per call). A
///   parent that delegates "review this file" with only a *path* hands the
///   child a name it has no way to resolve — and the child, unable to admit
///   it, fabricates a review of a file it never read.
const INSTRUCTION: &str = concat!(
    "You are an RLM agent powered by Fast-RLM. Act as a coding agent. ",
    "Use the structured context fields directly. ",
    "Use the workspace MCP tools through await mcp_call('workspace', tool_name, **arguments) ",
    "to inspect and change files. ",
    "Before coding, call the skill tool without a path, then read any relevant AGENTS.md and ",
    "SKILL.md documents it lists. ",
    "Use bash for general shell commands and run relevant tests after edits. ",
    "Mutating tools pause for user approval. ",
    "Return a concise final answer in Markdown.\n\n",
    "FILESYSTEM: your Python REPL runs in an isolated Pyodide sandbox with its own empty ",
    "filesystem. It is NOT the project directory. open(), pathlib, os.listdir and os.getcwd ",
    "all operate on that sandbox, so a file you write with open() does not exist in the ",
    "project and bash will not find it. The only way to touch project files is ",
    "mcp_call('workspace', ...). Build content in REPL variables, then persist it with ",
    "write_file or edit_file.\n\n",
    "TOOL RESULTS: workspace tools return their outcome as text instead of raising. A bash ",
    "command that exits non-zero comes back as normal output ending in '(exit N)'; a failed ",
    "edit comes back as an explanatory message. Check the returned text and adapt — do not ",
    "wrap every call in try/except, and do not assume a cell died because one command failed.\n\n",
    "SUB-AGENTS: a sub-agent starts with none of your variables and sees ONLY the context you ",
    "pass, but it does inherit the workspace MCP tools, so it can read and search the project ",
    "itself. Therefore:\n",
    "- Prefer passing a file's CONTENTS as data when you already have them in a variable: ",
    "await llm_query({\"source\": src}, instruction=\"...\"). It saves the child a round trip.\n",
    "- A sub-agent can also call mcp_call('workspace', ...) on its own. Pass mcp=[] if you want ",
    "a child to have no workspace access at all.\n",
    "- If a sub-agent reports that it could not find or read something, treat its conclusions ",
    "as void and redo the work yourself. Never repeat a sub-agent's findings as fact when it ",
    "had no access to the material.\n\n",
    "VERIFICATION: before describing what you built, re-read the file you actually wrote with ",
    "read_file and base your claims on that text. Do not describe behaviour from a local ",
    "reimplementation, from the draft in a variable, or from a sub-agent that could not read ",
    "the file. Delegate independent read-only analysis to parallel sub-agents when useful; ",
    "keep mutations in the root agent."
);

const BRIDGE_SOURCE: &str = include_str!("../scripts/fast_rlm_bridge.py");
const WORKSPACE_MCP_SOURCE: &str = include_str!("../scripts/workspace_mcp.ts");

/// Lightweight UI transcript persisted alongside the FastRLM session. FastRLM
/// owns its full model/REPL history in `rlm-sessions`; these values only let the
/// terminal restore the visible conversation.
pub type Message = serde_json::Value;
pub type SharedHistory = Arc<Mutex<Vec<Message>>>;

pub fn new_session() -> SharedHistory {
    Arc::new(Mutex::new(vec![serde_json::json!({
        "role": "system",
        "content": "Conversation transcript for a FastRLM-backed coding harness."
    })]))
}

#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RlmStep {
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub depth: usize,
    pub step: usize,
    pub event_type: String,
    pub code: String,
    pub output: Option<String>,
    pub has_error: bool,
    pub reasoning: Option<String>,
    pub usage: Usage,
    pub total_usage: Usage,
}

pub enum AgentEvent {
    Step(Box<RlmStep>),
    Tool(ToolCall, Responder),
    Final {
        depth: usize,
        result: serde_json::Value,
    },
    Usage(Usage),
    Error(String),
    Done,
}

#[derive(Serialize)]
struct BridgeRequest<'a> {
    context: &'a PromptContext,
    model: &'a str,
    session_dir: String,
    session_id: &'a str,
    log_dir: String,
    instruction: &'static str,
    broker_url: &'a str,
    broker_token: &'a str,
    workspace_mcp_script: String,
    /// Let sub-agents reach the workspace MCP server without an explicit
    /// per-call grant. Mutating tools stay approval-gated either way.
    inherit_mcp: bool,
    inherit_tools: bool,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BridgeMessage {
    RlmEvent {
        event: RawRlmEvent,
    },
    Complete {
        result: serde_json::Value,
        #[serde(default)]
        usage: Usage,
        #[allow(dead_code)]
        log_file: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Deserialize, Default)]
struct RawRlmEvent {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    parent_run_id: Option<String>,
    #[serde(default)]
    depth: usize,
    #[serde(default)]
    step: usize,
    #[serde(default)]
    code: String,
    #[serde(default)]
    output: Option<String>,
    #[serde(default, rename = "hasError")]
    has_error: bool,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    usage: Usage,
    #[serde(default, rename = "totalUsage")]
    total_usage: Usage,
    #[serde(default)]
    result: serde_json::Value,
}

pub fn respond(
    context: PromptContext,
    config: Config,
    history: SharedHistory,
    workspace_root: PathBuf,
    session_id: String,
    tx: UnboundedSender<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run(
            context,
            &config,
            &history,
            &workspace_root,
            &session_id,
            &tx,
        )
        .await
        {
            let _ = tx.send(Event::Agent(AgentEvent::Error(error)));
        }
        let _ = tx.send(Event::Agent(AgentEvent::Done));
    })
}

async fn run(
    context: PromptContext,
    config: &Config,
    history: &SharedHistory,
    workspace_root: &Path,
    session_id: &str,
    tx: &UnboundedSender<Event>,
) -> Result<(), String> {
    history.lock().unwrap().push(serde_json::json!({
        "role": "user",
        "content": context.to_user_content()
    }));

    let broker = crate::broker::Broker::start(tx.clone()).await?;

    let state_root = app_data_dir();
    std::fs::create_dir_all(&state_root)
        .map_err(|e| format!("could not create FastRLM state directory: {e}"))?;
    let workspace_mcp_script = state_root.join("workspace_mcp.ts");
    std::fs::write(&workspace_mcp_script, WORKSPACE_MCP_SOURCE)
        .map_err(|e| format!("could not install workspace MCP bridge: {e}"))?;
    let request = BridgeRequest {
        context: &context,
        model: &config.model,
        session_dir: state_root.join("rlm-sessions").display().to_string(),
        session_id,
        log_dir: state_root.join("rlm-logs").display().to_string(),
        instruction: INSTRUCTION,
        inherit_mcp: true,
        inherit_tools: true,
        broker_url: broker.url(),
        broker_token: broker.token(),
        workspace_mcp_script: workspace_mcp_script.display().to_string(),
    };
    let input = serde_json::to_vec(&request).map_err(|e| e.to_string())?;

    let mut command = Command::new(find_python()?);
    command
        .arg("-c")
        .arg(BRIDGE_SOURCE)
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(key) = &config.api_key {
        command.env("RLM_MODEL_API_KEY", key);
    }
    command.env("RLM_MODEL_BASE_URL", &config.base_url);

    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start FastRLM: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("FastRLM stdin unavailable")?
        .write_all(&input)
        .await
        .map_err(|e| format!("could not send context to FastRLM: {e}"))?;

    let stdout = child.stdout.take().ok_or("FastRLM stdout unavailable")?;
    let mut stderr = child.stderr.take().ok_or("FastRLM stderr unavailable")?;
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut final_result = None;
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("FastRLM event stream failed: {e}"))?
    {
        let message: BridgeMessage = serde_json::from_str(&line)
            .map_err(|e| format!("invalid FastRLM bridge event: {e}: {line}"))?;
        match message {
            BridgeMessage::RlmEvent { event } => match event.event_type.as_str() {
                "code_generated" | "execution_result" => {
                    let _ = tx.send(Event::Agent(AgentEvent::Step(Box::new(RlmStep {
                        run_id: event.run_id,
                        parent_run_id: event.parent_run_id,
                        depth: event.depth,
                        step: event.step,
                        event_type: event.event_type,
                        code: event.code,
                        output: event.output,
                        has_error: event.has_error,
                        reasoning: event.reasoning,
                        usage: event.usage,
                        total_usage: event.total_usage,
                    }))));
                }
                "final_result" => {
                    final_result = Some(event.result.clone());
                    let _ = tx.send(Event::Agent(AgentEvent::Final {
                        depth: event.depth,
                        result: event.result,
                    }));
                }
                _ => {}
            },
            BridgeMessage::Complete { result, usage, .. } => {
                if final_result.is_none() {
                    let _ = tx.send(Event::Agent(AgentEvent::Final {
                        depth: 0,
                        result: result.clone(),
                    }));
                    final_result = Some(result);
                }
                let _ = tx.send(Event::Agent(AgentEvent::Usage(usage)));
            }
            BridgeMessage::Error { message } => return Err(message),
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("could not wait for FastRLM: {e}"))?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let detail = stderr.lines().last().unwrap_or("unknown error");
        return Err(format!("FastRLM exited with {status}: {detail}"));
    }

    if let Some(result) = final_result {
        history
            .lock()
            .unwrap()
            .push(serde_json::json!({ "role": "assistant", "content": format_result(&result) }));
    }
    broker.stop();
    Ok(())
}

fn format_result(result: &serde_json::Value) -> String {
    match result {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn app_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".fast-rlm-agent")
}

fn find_python() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("FAST_RLM_PYTHON") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            let candidate = ancestor.join(".venv").join("bin").join("python");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let development = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".venv")
        .join("bin")
        .join("python");
    if development.is_file() {
        return Ok(development);
    }

    Ok(PathBuf::from("python3"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn parses_execution_and_completion_protocol_events() {
        let step: BridgeMessage = serde_json::from_str(
            r#"{"kind":"rlm_event","event":{"event_type":"execution_result","run_id":"abc","parent_run_id":null,"depth":1,"step":2,"code":"print(1)","output":"1","hasError":false,"usage":{"total_tokens":9},"totalUsage":{"total_tokens":12}}}"#,
        )
        .unwrap();
        let BridgeMessage::RlmEvent { event } = step else {
            panic!("expected RLM event");
        };
        assert_eq!(event.depth, 1);
        assert_eq!(event.output.as_deref(), Some("1"));
        assert_eq!(event.total_usage.total_tokens, 12);

        let complete: BridgeMessage = serde_json::from_str(
            r#"{"kind":"complete","result":{"ok":true},"usage":{"prompt_tokens":20,"cost":0.01},"log_file":"run.jsonl"}"#,
        )
        .unwrap();
        let BridgeMessage::Complete { result, usage, .. } = complete else {
            panic!("expected completion");
        };
        assert_eq!(result["ok"], true);
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.cost, 0.01);
    }

    #[tokio::test]
    #[ignore = "requires a configured live model and spends provider credits"]
    async fn live_fast_rlm_can_edit_a_workspace_file() {
        let workspace =
            std::env::temp_dir().join(format!("fast-rlm-agent-live-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("proof.txt"), "BEFORE\n").unwrap();

        let prompt = "Change proof.txt from BEFORE to FAST_RLM_EDIT_WORKS while preserving its newline. You must use edit_file, then read_file to verify it. Do not use write_file or create other files.";
        let context = PromptContext::from_prompt(prompt.to_string(), &workspace);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = respond(
            context,
            Config::from_env(),
            new_session(),
            workspace.clone(),
            format!("live-write-{}", std::process::id()),
            tx,
        );

        tokio::time::timeout(std::time::Duration::from_secs(300), async {
            while let Some(event) = rx.recv().await {
                match event {
                    Event::Agent(AgentEvent::Tool(call, responder)) => {
                        // This opt-in test auto-approves only in its fresh temp workspace.
                        let result = crate::tools::execute(&workspace, call).await;
                        let _ = responder.send(crate::tools::ToolResult::Output {
                            ok: result.ok,
                            text: result.for_agent,
                        });
                    }
                    Event::Agent(AgentEvent::Done) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("live FastRLM write timed out");
        task.await.unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.join("proof.txt")).unwrap(),
            "FAST_RLM_EDIT_WORKS\n"
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    #[ignore = "requires a configured live model and spends provider credits"]
    async fn live_fast_rlm_can_run_bash() {
        const COMMAND: &str = "printf FAST_RLM_BASH_OK";

        let workspace =
            std::env::temp_dir().join(format!("fast-rlm-agent-live-bash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();

        let prompt = format!(
            "Use the workspace MCP bash tool exactly once with command `{COMMAND}`. \
             Do not use read_file, write_file, or edit_file. The MCP result is an object \
             with a text field; report that text in your final answer."
        );
        let context = PromptContext::from_prompt(prompt, &workspace);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = respond(
            context,
            Config::from_env(),
            new_session(),
            workspace.clone(),
            format!("live-bash-{}", std::process::id()),
            tx,
        );
        let mut saw_expected_bash = false;
        let mut final_reported_marker = false;

        tokio::time::timeout(std::time::Duration::from_secs(300), async {
            while let Some(event) = rx.recv().await {
                match event {
                    Event::Agent(AgentEvent::Tool(call @ ToolCall::Skill { .. }, responder)) => {
                        let run = crate::tools::execute(&workspace, call).await;
                        let _ = responder.send(crate::tools::ToolResult::Output {
                            ok: run.ok,
                            text: run.for_agent,
                        });
                    }
                    Event::Agent(AgentEvent::Tool(
                        ToolCall::Bash {
                            command,
                            timeout_seconds,
                        },
                        responder,
                    )) => {
                        if command == COMMAND {
                            let run = crate::tools::execute(
                                &workspace,
                                ToolCall::Bash {
                                    command,
                                    timeout_seconds,
                                },
                            )
                            .await;
                            saw_expected_bash =
                                run.ok && run.for_agent.contains("FAST_RLM_BASH_OK");
                            let _ = responder.send(crate::tools::ToolResult::Output {
                                ok: run.ok,
                                text: run.for_agent,
                            });
                        } else {
                            let _ = responder.send(crate::tools::ToolResult::Output {
                                ok: false,
                                text: "unexpected Bash command rejected by live test".to_string(),
                            });
                        }
                    }
                    Event::Agent(AgentEvent::Tool(_, responder)) => {
                        let _ = responder.send(crate::tools::ToolResult::Output {
                            ok: false,
                            text: "unexpected tool call rejected by live Bash test".to_string(),
                        });
                    }
                    Event::Agent(AgentEvent::Final { depth: 0, result }) => {
                        final_reported_marker = result.to_string().contains("FAST_RLM_BASH_OK");
                    }
                    Event::Agent(AgentEvent::Done) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("live FastRLM Bash test timed out");
        task.await.unwrap();

        assert!(
            saw_expected_bash,
            "FastRLM did not execute the expected Bash command"
        );
        assert!(
            final_reported_marker,
            "FastRLM did not report the Bash output in its final answer"
        );
        let _ = std::fs::remove_dir_all(workspace);
    }
}
