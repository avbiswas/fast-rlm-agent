//! Application state and the central `update` reducer.

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::agent::{self, AgentEvent};
use crate::composer::Composer;
use crate::config::Config;
use crate::event::Event;
use crate::session;
use crate::snapshot::Snapshotter;
use crate::tools::{self, Question, Responder, ToolCall, ToolResult, ToolStatus, ToolUpdate};

/// One entry in the conversation transcript — the single vertical "stack"
/// everything renders into. Serializable so sessions can be saved/resumed.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Item {
    User(String),
    Assistant(String),
    Note(String),
    /// A tool call, rendered uniformly as `● Verb(args)` + `⎿ output`.
    Tool(ToolRun),
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
    Diff { path: String, old: String, new: String },
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
    UndoPicker { cursor: usize },
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
    /// Token accounting from the most recent model request.
    pub usage: Option<crate::llm::Usage>,
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
    tx: UnboundedSender<Event>,
}

impl App {
    pub fn new(tx: UnboundedSender<Event>, config: Config) -> Self {
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
            usage: None,
            assistant_open: false,
            next_id: 0,
            task: None,
            history: agent::new_session(),
            session_id: None,
            session_created: 0,
            snapshotter: Snapshotter::new(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ),
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
            CrosstermEvent::Mouse(mouse) => self.on_mouse(mouse),
            _ => {}
        }
    }

    /// Wheel scrolling works in every mode — the transcript is always the
    /// thing behind the pointer.
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
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
        self.task = Some(agent::respond(
            prompt,
            self.config.clone(),
            self.history.clone(),
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
        let count = self.snapshotter.as_ref().map(|s| s.checkpoints().len()).unwrap_or(0);
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
        let Mode::UndoPicker { cursor } = self.mode else { return; };
        let count = self.snapshotter.as_ref().map(|s| s.checkpoints().len()).unwrap_or(0);

        match key.code {
            KeyCode::Esc => self.mode = Mode::Chat,
            KeyCode::Up => self.mode = Mode::UndoPicker { cursor: cursor.saturating_sub(1) },
            KeyCode::Down => {
                self.mode = Mode::UndoPicker { cursor: (cursor + 1).min(count.saturating_sub(1)) };
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
        let Some(ref mut snap) = self.snapshotter else { return; };
        match snap.restore_to(stack_idx) {
            None => self.note("⚠ undo failed — could not restore snapshot".to_string()),
            Some(cp) => {
                self.items.truncate(cp.items_len);
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
            AgentEvent::Delta(text) => self.push_delta(&text),
            AgentEvent::Tool { call, respond } => self.open_tool(call, respond),
            AgentEvent::Usage(usage) => {
                self.usage = Some(usage);
                self.dirty = true;
            }
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

        let (verb, arg, body) = describe(&call);

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
        tokio::spawn(async move {
            let run = tools::execute(call).await;
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
}

/// Build the transcript header bits (verb, arg, body) for a tool call.
/// `Err` means the call is invalid and should fail without user interaction.
fn describe(call: &ToolCall) -> (String, String, Result<ToolBody, String>) {
    let text = Ok(ToolBody::Text { output: None });
    match call {
        ToolCall::Read { path } => ("Read".into(), path.clone(), text),
        ToolCall::Write { path, content } => {
            let old = std::fs::read_to_string(path).unwrap_or_default();
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
            let body = tools::prepare_edit(path, old_string, new_string, *replace_all).map(
                |(old, new, _)| ToolBody::Diff {
                    path: path.clone(),
                    old,
                    new,
                },
            );
            ("Edit".into(), path.clone(), body)
        }
        ToolCall::Bash { command } => ("Bash".into(), command.clone(), text),
        ToolCall::WebSearch { query } => ("Search".into(), query.clone(), text),
        ToolCall::Fetch { url, .. } => ("Fetch".into(), url.clone(), text),
        ToolCall::AskQuestion(_) => ("Ask".into(), String::new(), text),
    }
}
