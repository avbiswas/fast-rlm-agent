//! fast-rlm-agent — a minimal ratatui + crossterm + tokio chat harness.
//!
//! Architecture (Elm-ish, like the Codex CLI):
//!   * `App`   — all mutable state. Knows how to `update(event)` itself.
//!   * `ui`    — a pure `draw(frame, &mut app)` that rebuilds the view each frame.
//!   * `event` — a single async event hub fanning terminal input, a render tick,
//!               and background agent messages into one `mpsc` channel.
//!   * `agent` — the model backend. Currently a mock that *streams* a reply.
//!
//! The whole app is single-threaded at the state level: mutations only ever
//! happen on the main loop in response to an `Event`. Background work (the
//! agent) communicates back over the channel — never by touching `App`.

mod agent;
mod app;
mod composer;
mod config;
mod context;
mod event;
mod llm;
mod markdown;
mod session;
mod snapshot;
mod tools;
mod ui;
mod workspace;

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::App;
use crate::event::{Event, Events};

type Tui = Terminal<CrosstermBackend<Stdout>>;

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    let result = run(&mut terminal).await;

    // Always restore the terminal, even if `run` errored.
    restore_terminal()?;
    result
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
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
