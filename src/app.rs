//! Application state and the central `update` reducer.

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::agent::{self, AgentEvent};
use crate::composer::Composer;
use crate::config::Config;
use crate::context::PromptContext;
use crate::event::Event;
use crate::session;
use crate::snapshot::Snapshotter;
use crate::tools::{self, Question, Responder, ToolCall, ToolResult, ToolStatus, ToolUpdate};

const PROCESSING_NOTE: &str = "Processing...";

/// One entry in the conversation transcript — the single vertical "stack"
/// everything renders into. Serializable so sessions can be saved/resumed.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Item {
    User(String),
    Assistant(String),
    Note(String),
    /// A tool call, rendered uniformly as `● Verb(args)` + `⎿ output`.
    Tool(ToolRun),
    /// One generated-code / REPL-execution step from FastRLM.
    Rlm(agent::RlmStep),
}

/// A tool call's record in the transcript.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolRun {
    pub id: u64,
    /// e.g. "Bash", "Read", "Update", "Search".
    pub verb: String,
    /// The primary argument, shown in parens (command, path, query).
    pub arg: String,
    pub body: ToolBody,
    pub status: ToolStatus,
    /// The `⎿` summary line, once known.
    pub summary: Option<String>,
}

/// How a tool's output area renders.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum ToolBody {
    /// A file write: render a diff of `old` → `new`.
    Diff {
        path: String,
        old: String,
        new: String,
    },
    /// A command-style tool: render its captured output text (when present).
    Text { output: Option<String> },
}

/// What input is routed to. Approvals capture keys but still render inline.
pub enum Mode {
    Chat,
    /// Awaiting y/n for the tool at transcript `index`.
    Approve {
        index: usize,
        call: ToolCall,
        responder: Responder,
    },
    Question(QuestionModal),
    /// The `/resume` session picker.
    Resume(ResumePicker),
    /// The `/undo` checkpoint picker.
    UndoPicker {
        cursor: usize,
    },
}

pub struct ResumePicker {
    pub sessions: Vec<session::Summary>,
    pub cursor: usize,
}

pub struct QuestionModal {
    pub question: Question,
    pub responder: Responder,
    pub cursor: usize,
    pub selected: Vec<bool>,
}

pub struct App {
    pub config: Config,
    pub items: Vec<Item>,
    pub composer: Composer,
    pub mode: Mode,
    pub scroll: u16,
    pub follow: bool,
    pub streaming: bool,
    pub should_quit: bool,
    pub dirty: bool,
    /// Items before this index have been written into terminal scrollback.
    pub committed_items: usize,
    /// Token accounting from the most recent model request.
    pub usage: Option<agent::Usage>,
    assistant_open: bool,
    next_id: u64,
    task: Option<JoinHandle<()>>,
    /// Append-only conversation history (see `agent` docs on cache discipline).
    history: agent::SharedHistory,
    /// Set once the first real message is sent; identifies the session file.
    session_id: Option<String>,
    session_created: u64,
    /// Shadow-git snapshotter for /undo. None when git is unavailable.
    pub snapshotter: Option<Snapshotter>,
    /// Canonical directory that bounds model-requested filesystem tools.
    workspace_root: PathBuf,
    tx: UnboundedSender<Event>,
}

impl App {
    pub fn new(tx: UnboundedSender<Event>, config: Config) -> Self {
        let workspace_root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        Self {
            config,
            items: Vec::new(),
            composer: Composer::default(),
            mode: Mode::Chat,
            scroll: 0,
            follow: true,
            streaming: false,
            should_quit: false,
            dirty: false,
            committed_items: 0,
            usage: None,
            assistant_open: false,
            next_id: 0,
            task: None,
            history: agent::new_session(),
            session_id: None,
            session_created: 0,
            snapshotter: Snapshotter::new(workspace_root.clone()),
            workspace_root,
            tx,
        }
    }

    pub fn update(&mut self, event: Event) {
        match event {
            Event::Term(term) => self.on_terminal(term),
            Event::Agent(msg) => self.on_agent(msg),
            Event::Tool(update) => self.on_tool_update(update),
            Event::Tick => {}
        }
    }

    // ---- input -----------------------------------------------------------

    fn on_terminal(&mut self, event: CrosstermEvent) {
        match event {
            CrosstermEvent::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    return;
                }
                self.on_key(key);
            }
            CrosstermEvent::Paste(text) => {
                if matches!(self.mode, Mode::Chat) {
                    self.composer.insert_str(&text);
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        match &self.mode {
            Mode::Chat => self.on_key_chat(key),
            Mode::Approve { .. } => self.on_key_approve(key),
            Mode::Question(_) => self.on_key_question(key),
            Mode::Resume(_) => self.on_key_resume(key),
            Mode::UndoPicker { .. } => self.on_key_undo_picker(key),
        }
    }

    fn on_key_resume(&mut self, key: KeyEvent) {
        let Mode::Resume(picker) = &mut self.mode else {
            return;
        };
        let n = picker.sessions.len();
        match key.code {
            KeyCode::Esc => self.mode = Mode::Chat,
            KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Down => picker.cursor = (picker.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Enter => {
                let Mode::Resume(picker) = std::mem::replace(&mut self.mode, Mode::Chat) else {
                    return;
                };
                if let Some(summary) = picker.sessions.get(picker.cursor) {
                    let path = summary.path.clone();
                    self.resume_session(&path);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn on_key_chat(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let c = &mut self.composer;

        match key.code {
            KeyCode::Esc => {
                if self.streaming {
                    self.cancel();
                } else {
                    self.should_quit = true;
                }
            }

            KeyCode::Enter if alt || shift => c.newline(),
            KeyCode::Enter => self.submit(),

            KeyCode::Backspace if ctrl || alt => c.delete_word_back(),
            KeyCode::Backspace => c.backspace(),
            KeyCode::Delete => c.delete_forward(),
            KeyCode::Char('w') if ctrl => c.delete_word_back(),
            KeyCode::Char('u') if ctrl => c.kill_to_line_start(),
            KeyCode::Char('k') if ctrl => c.kill_to_line_end(),
            KeyCode::Char('d') if alt => c.delete_word_forward(),

            KeyCode::Left if ctrl || alt => c.word_left(),
            KeyCode::Right if ctrl || alt => c.word_right(),
            KeyCode::Left => c.left(),
            KeyCode::Right => c.right(),
            KeyCode::Up => c.up(),
            KeyCode::Down => c.down(),
            KeyCode::Home => c.line_start(),
            KeyCode::End => c.line_end(),
            KeyCode::Char('a') if ctrl => c.line_start(),
            KeyCode::Char('e') if ctrl => c.line_end(),
            KeyCode::Char('b') if alt => c.word_left(),
            KeyCode::Char('f') if alt => c.word_right(),

            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),

            KeyCode::Char(ch) if !ctrl && !alt => c.insert(ch),
            _ => {}
        }
    }

    fn on_key_approve(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => self.resolve_approval(true),
            KeyCode::Char('n') | KeyCode::Esc => self.resolve_approval(false),
            KeyCode::Up | KeyCode::PageUp => self.scroll_by(-3),
            KeyCode::Down | KeyCode::PageDown => self.scroll_by(3),
            _ => {}
        }
    }

    fn on_key_question(&mut self, key: KeyEvent) {
        let Mode::Question(modal) = &mut self.mode else {
            return;
        };
        let n = modal.question.options.len();
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Up => modal.cursor = modal.cursor.saturating_sub(1),
            KeyCode::Down => modal.cursor = (modal.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Char(d @ '1'..='9') => {
                let idx = (d as usize) - ('1' as usize);
                if idx < n {
                    modal.cursor = idx;
                    if !modal.question.multi_select {
                        self.resolve_question();
                        return;
                    }
                    modal.selected[idx] = !modal.selected[idx];
                }
            }
            KeyCode::Char(' ') if modal.question.multi_select => {
                let i = modal.cursor;
                modal.selected[i] = !modal.selected[i];
            }
            KeyCode::Enter => self.resolve_question(),
            _ => {}
        }
    }

    // ---- chat actions ----------------------------------------------------

    fn submit(&mut self) {
        if self.streaming || self.composer.is_empty() {
            return;
        }
        let prompt = self.composer.text().trim().to_string();
        self.composer.clear();

        // Slash commands are handled by the harness, not the model.
        if let Some(command) = prompt.strip_prefix('/') {
            self.handle_command(command.trim());
            return;
        }

        // First real message starts the session file.
        if self.session_id.is_none() {
            self.session_id = Some(session::new_id());
            self.session_created = session::now();
        }

        // Snapshot filesystem state before the agent touches anything.
        if let Some(ref mut snap) = self.snapshotter {
            snap.capture(self.items.len(), self.history.lock().unwrap().len());
        }

        self.items.push(Item::User(prompt.clone()));
        self.streaming = true;
        self.assistant_open = false;
        self.follow = true;
        let context = PromptContext::from_prompt(prompt, &self.workspace_root);
        if !context.links.is_empty() || !context.files.is_empty() {
            self.items.push(Item::Note(format!(
                "◇ structured context · {} link{} · {} file{}",
                context.links.len(),
                if context.links.len() == 1 { "" } else { "s" },
                context.files.len(),
                if context.files.len() == 1 { "" } else { "s" },
            )));
        }
        let session_id = self.session_id.clone().unwrap_or_else(session::new_id);
        self.task = Some(agent::respond(
            context,
            self.config.clone(),
            self.history.clone(),
            self.workspace_root.clone(),
            session_id,
            self.tx.clone(),
        ));
    }

    // ---- slash commands & sessions ----------------------------------------

    fn handle_command(&mut self, command: &str) {
        match command {
            "resume" => {
                let sessions = session::list_in(&session::default_dir());
                if sessions.is_empty() {
                    self.note("no saved sessions yet".to_string());
                } else {
                    self.mode = Mode::Resume(ResumePicker {
                        sessions,
                        cursor: 0,
                    });
                }
            }
            "undo" => self.open_undo_picker(),
            other => self.note(format!("unknown command: /{other}")),
        }
        self.pin();
    }

    /// Persist the current session (no-op until a first message exists).
    pub fn save_session(&self) {
        let Some(id) = &self.session_id else {
            return;
        };
        let title = self
            .items
            .iter()
            .find_map(|i| match i {
                Item::User(text) => Some(text.lines().next().unwrap_or("").to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "(untitled)".to_string());
        let title = title.chars().take(64).collect();

        let saved = session::SavedSession {
            id: id.clone(),
            title,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            created_at: self.session_created,
            updated_at: session::now(),
            history: self.history.lock().unwrap().clone(),
            items: self.items.clone(),
        };
        let _ = session::save_in(&session::default_dir(), &saved);
    }

    fn resume_session(&mut self, path: &std::path::Path) {
        match session::load(path) {
            Ok(loaded) => {
                // Don't lose the conversation we're leaving.
                self.save_session();

                self.items = loaded.items;
                self.committed_items = 0;
                *self.history.lock().unwrap() = loaded.history;
                self.session_id = Some(loaded.id);
                self.session_created = loaded.created_at;
                self.usage = None;
                self.assistant_open = false;
                self.follow = true;
                self.note(format!("⟲ resumed: {}", loaded.title));
            }
            Err(e) => self.note(format!("failed to load session: {e}")),
        }
        self.pin();
    }

    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.mode = Mode::Chat; // dropping any responder unblocks the agent
        self.streaming = false;
        self.assistant_open = false;
        self.note("⚠ cancelled".to_string());
        self.save_session();
    }

    fn open_undo_picker(&mut self) {
        let count = self
            .snapshotter
            .as_ref()
            .map(|s| s.checkpoints().len())
            .unwrap_or(0);
        if count == 0 {
            self.note(if self.snapshotter.is_none() {
                "⚠ /undo not available — git not found".to_string()
            } else {
                "nothing to undo".to_string()
            });
            self.pin();
            return;
        }
        self.mode = Mode::UndoPicker { cursor: 0 };
        self.dirty = true;
    }

    fn on_key_undo_picker(&mut self, key: KeyEvent) {
        let Mode::UndoPicker { cursor } = self.mode else {
            return;
        };
        let count = self
            .snapshotter
            .as_ref()
            .map(|s| s.checkpoints().len())
            .unwrap_or(0);

        match key.code {
            KeyCode::Esc => self.mode = Mode::Chat,
            KeyCode::Up => {
                self.mode = Mode::UndoPicker {
                    cursor: cursor.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                self.mode = Mode::UndoPicker {
                    cursor: (cursor + 1).min(count.saturating_sub(1)),
                };
            }
            KeyCode::Enter => {
                self.mode = Mode::Chat;
                // Picker shows newest first: cursor 0 = checkpoints[count-1].
                let stack_idx = count.saturating_sub(1 + cursor);
                self.restore_to_checkpoint(stack_idx);
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn restore_to_checkpoint(&mut self, stack_idx: usize) {
        let Some(ref mut snap) = self.snapshotter else {
            return;
        };
        match snap.restore_to(stack_idx) {
            None => self.note("⚠ undo failed — could not restore snapshot".to_string()),
            Some(cp) => {
                self.items.truncate(cp.items_len);
                self.committed_items = self.committed_items.min(self.items.len());
                self.history.lock().unwrap().truncate(cp.history_len);
                self.assistant_open = false;
                self.follow = true;
                self.note("⟲ undone — files and conversation restored".to_string());
            }
        }
        self.pin();
    }

    // ---- agent events ----------------------------------------------------

    fn on_agent(&mut self, msg: AgentEvent) {
        match msg {
            AgentEvent::Step(step) => {
                self.usage = Some(step.total_usage);
                if step.depth == 0 && step.step == 0 {
                    let already_shown = self
                        .items
                        .iter()
                        .rev()
                        .take_while(|item| !matches!(item, Item::User(_)))
                        .any(|item| matches!(item, Item::Note(text) if text == PROCESSING_NOTE));
                    if !already_shown {
                        self.note(PROCESSING_NOTE.to_string());
                    } else {
                        self.pin();
                    }
                    return;
                }
                self.items.push(Item::Rlm(*step));
                self.assistant_open = false;
                self.pin();
            }
            AgentEvent::Tool(call, responder) => self.open_tool(call, responder),
            AgentEvent::Final { depth, result } => {
                let text = match result {
                    serde_json::Value::String(text) => text,
                    other => format!(
                        "```json\n{}\n```",
                        serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string())
                    ),
                };
                if depth == 0 {
                    self.push_delta(&text);
                } else {
                    self.note(format!("↳ depth {depth} sub-agent returned a result"));
                }
            }
            AgentEvent::Usage(usage) => {
                self.usage = Some(usage);
                self.dirty = true;
            }
            AgentEvent::Error(error) => self.note(format!("⚠ FastRLM: {error}")),
            AgentEvent::Done => {
                self.streaming = false;
                self.assistant_open = false;
                self.task = None;
                self.save_session();
            }
        }
    }

    fn push_delta(&mut self, text: &str) {
        if self.assistant_open {
            if let Some(Item::Assistant(content)) = self.items.last_mut() {
                content.push_str(text);
            }
        } else {
            self.items.push(Item::Assistant(text.to_string()));
            self.assistant_open = true;
        }
        self.pin();
    }

    fn note(&mut self, content: String) {
        self.items.push(Item::Note(content));
        self.assistant_open = false;
        self.pin();
    }

    /// A tool call arrived. Record it, then either ask for approval or run it.
    fn open_tool(&mut self, call: ToolCall, responder: Responder) {
        self.assistant_open = false;
        self.follow = true;

        // Questions have their own selection UI.
        if let ToolCall::AskQuestion(question) = call {
            let selected = vec![false; question.options.len()];
            self.mode = Mode::Question(QuestionModal {
                question,
                responder,
                cursor: 0,
                selected,
            });
            self.pin();
            return;
        }

        let id = self.next_id;
        self.next_id += 1;

        let (verb, arg, body) = describe(&self.workspace_root, &call);

        // Invalid calls (e.g. an edit whose old_string isn't found) fail fast:
        // record the failure and reply to the agent without asking the user.
        let body = match body {
            Ok(body) => body,
            Err(err) => {
                self.items.push(Item::Tool(ToolRun {
                    id,
                    verb,
                    arg,
                    body: ToolBody::Text { output: None },
                    status: ToolStatus::Failed,
                    summary: Some(err.clone()),
                }));
                let _ = responder.send(ToolResult::Output {
                    ok: false,
                    text: format!("ERROR: {err}"),
                });
                self.pin();
                return;
            }
        };

        let needs_approval = call.needs_approval();
        let status = if needs_approval {
            ToolStatus::Pending
        } else {
            ToolStatus::Running
        };
        self.items.push(Item::Tool(ToolRun {
            id,
            verb,
            arg,
            body,
            status,
            summary: None,
        }));
        let index = self.items.len() - 1;

        if needs_approval {
            self.mode = Mode::Approve {
                index,
                call,
                responder,
            };
        } else {
            self.spawn_exec(id, call, responder);
        }
        self.pin();
    }

    fn on_tool_update(&mut self, update: ToolUpdate) {
        for item in &mut self.items {
            if let Item::Tool(run) = item {
                if run.id == update.id {
                    run.status = update.status;
                    if update.summary.is_some() {
                        run.summary = update.summary;
                    }
                    if let ToolBody::Text { output } = &mut run.body {
                        if update.output.is_some() {
                            *output = update.output;
                        }
                    }
                    break;
                }
            }
        }
        self.pin();
    }

    /// Spawn a task that runs the tool, returns output to the agent, and posts
    /// a `ToolUpdate` to refresh this record in the transcript.
    fn spawn_exec(&self, id: u64, call: ToolCall, responder: Responder) {
        let tx = self.tx.clone();
        let workspace_root = self.workspace_root.clone();
        tokio::spawn(async move {
            let run = tools::execute(&workspace_root, call).await;
            let _ = responder.send(ToolResult::Output {
                ok: run.ok,
                text: run.for_agent,
            });
            let _ = tx.send(Event::Tool(ToolUpdate {
                id,
                status: if run.ok {
                    ToolStatus::Done
                } else {
                    ToolStatus::Failed
                },
                summary: Some(run.summary),
                output: run.output,
            }));
        });
    }

    fn resolve_approval(&mut self, approved: bool) {
        let Mode::Approve {
            index,
            call,
            responder,
        } = std::mem::replace(&mut self.mode, Mode::Chat)
        else {
            return;
        };

        if approved {
            let id = match self.items.get_mut(index) {
                Some(Item::Tool(run)) => {
                    run.status = ToolStatus::Running;
                    run.id
                }
                _ => return,
            };
            self.spawn_exec(id, call, responder);
        } else {
            if let Some(Item::Tool(run)) = self.items.get_mut(index) {
                run.status = ToolStatus::Rejected;
                run.summary = Some("rejected".to_string());
            }
            let _ = responder.send(ToolResult::Output {
                ok: false,
                text: "User rejected this tool call.".to_string(),
            });
        }
        self.pin();
    }

    fn resolve_question(&mut self) {
        let Mode::Question(modal) = std::mem::replace(&mut self.mode, Mode::Chat) else {
            return;
        };
        let selected: Vec<usize> = if modal.question.multi_select {
            modal
                .selected
                .iter()
                .enumerate()
                .filter(|(_, &on)| on)
                .map(|(i, _)| i)
                .collect()
        } else {
            vec![modal.cursor]
        };

        let chosen: Vec<&str> = selected
            .iter()
            .filter_map(|&i| modal.question.options.get(i))
            .map(|o| o.label.as_str())
            .collect();
        let note = format!("❯ {}: {}", modal.question.header, chosen.join(", "));

        let _ = modal.responder.send(ToolResult::Question { selected });
        self.note(note);
    }

    // ---- scrolling -------------------------------------------------------

    fn scroll_by(&mut self, delta: i32) {
        let next = self.scroll as i32 + delta;
        self.scroll = next.max(0) as u16;
        self.follow = false;
        self.dirty = true;
    }

    fn pin(&mut self) {
        if self.follow {
            self.dirty = true;
        }
    }

    /// Return the longest prefix whose rendering can no longer change. The
    /// main loop moves this prefix into native terminal scrollback.
    pub fn committable_items_end(&self) -> usize {
        self.items
            .iter()
            .enumerate()
            .skip(self.committed_items)
            .take_while(|(index, item)| match item {
                Item::Tool(run) => !matches!(run.status, ToolStatus::Pending | ToolStatus::Running),
                Item::Assistant(_) => !(self.assistant_open && *index + 1 == self.items.len()),
                _ => true,
            })
            .last()
            .map_or(self.committed_items, |(index, _)| index + 1)
    }
}

/// Build the transcript header bits (verb, arg, body) for a tool call.
/// `Err` means the call is invalid and should fail without user interaction.
fn describe(
    workspace_root: &std::path::Path,
    call: &ToolCall,
) -> (String, String, Result<ToolBody, String>) {
    let text = Ok(ToolBody::Text { output: None });
    match call {
        ToolCall::Read { path } => ("Read".into(), path.clone(), text),
        ToolCall::Write { path, content } => {
            let resolved = match crate::workspace::resolve_for_write(workspace_root, path) {
                Ok(path) => path,
                Err(error) => return ("Update".into(), path.clone(), Err(error)),
            };
            let old = std::fs::read_to_string(resolved).unwrap_or_default();
            let verb = if old.is_empty() { "Create" } else { "Update" };
            (
                verb.into(),
                path.clone(),
                Ok(ToolBody::Diff {
                    path: path.clone(),
                    old,
                    new: content.clone(),
                }),
            )
        }
        ToolCall::Edit {
            path,
            old_string,
            new_string,
            replace_all,
        } => {
            let body =
                tools::prepare_edit(workspace_root, path, old_string, new_string, *replace_all)
                    .map(|(old, new, _)| ToolBody::Diff {
                        path: path.clone(),
                        old,
                        new,
                    });
            ("Edit".into(), path.clone(), body)
        }
        ToolCall::Bash { command } => ("Bash".into(), command.clone(), text),
        ToolCall::WebSearch { query } => ("Search".into(), query.clone(), text),
        ToolCall::Fetch { url, .. } => ("Fetch".into(), url.clone(), text),
        ToolCall::AskQuestion(_) => ("Ask".into(), String::new(), text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn write_preview_rejects_paths_outside_workspace() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let call = ToolCall::Write {
            path: "../outside.txt".to_string(),
            content: "must not be previewed or written".to_string(),
        };

        let (_, _, body) = describe(&root, &call);
        let Err(error) = body else {
            panic!("outside path unexpectedly produced an approval preview");
        };
        assert!(error.contains("path escapes workspace"));
    }

    #[test]
    fn fast_rlm_write_request_opens_the_approval_ui() {
        let root = std::env::temp_dir().join(format!("fra-approval-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(event_tx, Config::from_env());
        app.workspace_root = root.clone();
        let (reply_tx, _reply_rx) = oneshot::channel();

        app.on_agent(AgentEvent::Tool(
            ToolCall::Write {
                path: "proof.txt".to_string(),
                content: "new content\n".to_string(),
            },
            reply_tx,
        ));

        assert!(matches!(app.mode, Mode::Approve { .. }));
        assert!(matches!(
            app.items.last(),
            Some(Item::Tool(ToolRun {
                status: ToolStatus::Pending,
                ..
            }))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_finalized_items_are_committable_to_terminal_scrollback() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(event_tx, Config::from_env());
        app.items.push(Item::User("change it".to_string()));
        app.items.push(Item::Tool(ToolRun {
            id: 0,
            verb: "Edit".to_string(),
            arg: "file.txt".to_string(),
            body: ToolBody::Text { output: None },
            status: ToolStatus::Pending,
            summary: None,
        }));
        app.items.push(Item::Rlm(agent::RlmStep {
            run_id: "root".to_string(),
            parent_run_id: None,
            depth: 0,
            step: 1,
            event_type: "execution_result".to_string(),
            code: String::new(),
            output: None,
            has_error: false,
            reasoning: None,
            usage: Default::default(),
            total_usage: Default::default(),
        }));

        assert_eq!(app.committable_items_end(), 1);
        if let Item::Tool(run) = &mut app.items[1] {
            run.status = ToolStatus::Done;
        }
        assert_eq!(app.committable_items_end(), 3);

        app.items.push(Item::Assistant("final".to_string()));
        app.assistant_open = true;
        assert_eq!(app.committable_items_end(), 3);
        app.assistant_open = false;
        assert_eq!(app.committable_items_end(), 4);
    }

    #[test]
    fn root_bootstrap_events_collapse_to_one_processing_note() {
        fn step(step: usize, event_type: &str) -> agent::RlmStep {
            agent::RlmStep {
                run_id: "root".to_string(),
                parent_run_id: None,
                depth: 0,
                step,
                event_type: event_type.to_string(),
                code: "print(context)".to_string(),
                output: None,
                has_error: false,
                reasoning: None,
                usage: Default::default(),
                total_usage: Default::default(),
            }
        }

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(event_tx, Config::from_env());
        app.items.push(Item::User("hello".to_string()));

        app.on_agent(AgentEvent::Step(Box::new(step(0, "code_generated"))));
        app.on_agent(AgentEvent::Step(Box::new(step(0, "execution_result"))));

        assert_eq!(
            app.items
                .iter()
                .filter(|item| matches!(item, Item::Note(text) if text == PROCESSING_NOTE))
                .count(),
            1
        );
        assert!(!app.items.iter().any(|item| matches!(item, Item::Rlm(_))));

        app.on_agent(AgentEvent::Step(Box::new(step(1, "execution_result"))));
        assert!(matches!(app.items.last(), Some(Item::Rlm(step)) if step.step == 1));
    }
}
