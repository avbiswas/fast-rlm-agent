//! fast-rlm-agent — a minimal ratatui + crossterm + tokio chat harness.
//!
//! Architecture (Elm-ish, like the Codex CLI):
//! * `App` — all mutable state and the central event reducer.
//! * `ui` — a pure draw function that rebuilds the view each frame.
//! * `event` — terminal input, render ticks, and background agent events.
//! * `agent` — the FastRLM subprocess bridge and live event adapter.
//!
//! The whole app is single-threaded at the state level: mutations only ever
//! happen on the main loop in response to an `Event`. Background work (the
//! agent) communicates back over the channel — never by touching `App`.

mod agent;
mod app;
mod broker;
mod composer;
mod config;
mod context;
mod event;
mod markdown;
mod session;
mod skills;
mod snapshot;
mod tools;
mod ui;
mod workspace;

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    backend::CrosstermBackend,
    widgets::{Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};

use crate::app::App;
use crate::event::{Event, Events};

type Tui = Terminal<CrosstermBackend<Stdout>>;
const LIVE_VIEWPORT_HEIGHT: u16 = 12;

#[tokio::main]
async fn main() -> Result<()> {
    // `--headless <prompt>` runs exactly one turn with no TUI, printing the
    // transcript to stdout. It auto-approves mutating tools, so it is gated
    // behind an explicit second flag and meant for tests in scratch
    // workspaces — never for a directory you care about.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(index) = args.iter().position(|a| a == "--headless") {
        let Some(prompt) = args.get(index + 1) else {
            anyhow::bail!("--headless requires a prompt argument");
        };
        if !args.iter().any(|a| a == "--dangerously-auto-approve") {
            anyhow::bail!(
                "--headless auto-approves writes and shell commands; \
                 pass --dangerously-auto-approve to confirm"
            );
        }
        return run_headless(prompt.clone()).await;
    }

    install_panic_hook();
    let mut terminal = init_terminal()?;

    let result = run(&mut terminal).await;

    // Always restore the terminal, even if `run` errored.
    restore_terminal()?;
    result
}

async fn run_headless(prompt: String) -> Result<()> {
    let mut events = Events::new_headless();
    let mut app = App::new(events.tx.clone(), config::Config::from_env());
    app.auto_approve = true;
    app.submit_text(&prompt);

    let mut printed = 0;
    while let Some(event) = events.next().await {
        let finished = matches!(event, Event::Agent(agent::AgentEvent::Done));
        if !matches!(event, Event::Tick) {
            app.update(event);
        }

        // Stream newly settled transcript items as plain text.
        while printed < app.items.len() {
            print!("{}", ui::render_items(&app.items[printed..printed + 1]));
            printed += 1;
        }
        use std::io::Write;
        io::stdout().flush()?;

        if finished || app.should_quit {
            break;
        }
    }
    app.save_session();
    Ok(())
}

async fn run(terminal: &mut Tui) -> Result<()> {
    let mut events = Events::new();
    let mut app = App::new(events.tx.clone(), config::Config::from_env());

    // Initial paint.
    terminal.draw(|f| ui::draw(f, &mut app))?;

    while let Some(event) = events.next().await {
        // Tick is purely a redraw heartbeat; everything else mutates state.
        let needs_redraw = match event {
            Event::Tick => app.dirty,
            other => {
                app.update(other);
                true
            }
        };

        flush_stable_transcript(terminal, &mut app)?;

        if app.should_quit {
            app.save_session();
            break;
        }
        if needs_redraw {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            app.dirty = false;
        }
    }
    Ok(())
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnableBracketedPaste)?;
    let height = crossterm::terminal::size()?.1.min(LIVE_VIEWPORT_HEIGHT);
    let terminal = Terminal::with_options(
        CrosstermBackend::new(stdout),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    Ok(terminal)
}

fn flush_stable_transcript(terminal: &mut Tui, app: &mut App) -> Result<()> {
    let end = app.committable_items_end();
    if end <= app.committed_items {
        return Ok(());
    }

    let text = ui::render_items(&app.items[app.committed_items..end]);
    let width = terminal.size()?.width;
    let height = ui::visual_row_count(&text, width);
    if height > 0 {
        terminal.insert_before(height, |buffer| {
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .render(buffer.area, buffer);
        })?;
    }
    app.committed_items = end;
    Ok(())
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), DisableBracketedPaste)?;
    Ok(())
}

/// Make sure a panic doesn't leave the user's terminal in raw/alt-screen mode.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original(info);
    }));
}
