//! The async event hub.
//!
//! Three sources are merged into a single `mpsc` channel so the main loop can
//! `recv()` one stream of `Event`s:
//!   1. terminal input  (crossterm `EventStream`)
//!   2. a render tick    (so streaming text repaints smoothly)
//!   3. agent messages   (sent by `agent::*` over the same `tx`)

use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::StreamExt;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::agent::AgentEvent;
use crate::tools::ToolUpdate;

/// Everything the main loop can react to.
pub enum Event {
    /// A raw terminal event (key, paste, resize, ...).
    Term(CrosstermEvent),
    /// A message streamed back from the agent.
    Agent(AgentEvent),
    /// A tool-execution task reporting its result, to update the transcript.
    Tool(ToolUpdate),
    /// Periodic redraw heartbeat.
    Tick,
}

pub struct Events {
    rx: UnboundedReceiver<Event>,
    /// Clone this to let background tasks (the agent) push events back in.
    pub tx: UnboundedSender<Event>,
}

impl Events {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        // Terminal input reader.
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = EventStream::new();
                while let Some(Ok(ev)) = reader.next().await {
                    if tx.send(Event::Term(ev)).is_err() {
                        break;
                    }
                }
            });
        }

        // Render tick.
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(50));
                loop {
                    interval.tick().await;
                    if tx.send(Event::Tick).is_err() {
                        break;
                    }
                }
            });
        }

        Self { rx, tx }
    }

    /// Headless variant: no terminal reader and no render tick, so the loop
    /// only wakes for agent and tool events.
    pub fn new_headless() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { rx, tx }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
