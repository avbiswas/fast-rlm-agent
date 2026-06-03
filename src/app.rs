//! Application state and the central `update` reducer.

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::agent::{self, AgentEvent};
use crate::composer::Composer;
use crate::config::Config;
use crate::event::Event;
use crate::tools::{self, Question, Responder, ToolCall, ToolResult, ToolStatus, ToolUpdate};

/// One entry in the conversation transcript — the single vertical "stack"
/// everything renders into.
pub enum Item {
    User(String),
    Assistant(String),
    Note(String),
    /// A tool call, rendered uniformly as `● Verb(args)` + `⎿ output`.
    Tool(ToolRun),
}

/// A tool call's record in the transcript.
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
        }
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

    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.mode = Mode::Chat; // dropping any responder unblocks the agent
        self.streaming = false;
        self.assistant_open = false;
        self.note("⚠ cancelled".to_string());
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
