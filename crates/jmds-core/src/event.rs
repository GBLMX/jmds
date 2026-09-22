//! The one channel every layer talks through.
//!
//! Panes never call each other. An agent tool call, a file that changed on disk, a pane opening
//! and a session being written all become events on a `tokio::sync::broadcast` bus, and whoever
//! cares subscribes. `broadcast` rather than `mpsc` because the consumers are independent: the
//! TUI wants [`AgentEvent::Content`] to draw, the session writer wants the same event to append
//! to JSONL, and neither should have to forward it to the other.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use tokio::sync::broadcast;

use crate::pane::{PaneId, PaneSpec};

/// What the model is doing. Deltas are deltas: a subscriber appends, it does not replace.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A turn's request went out. The model name is here because the cost per token depends on
    /// it, and the status line shows both.
    TurnStarted {
        model: String,
    },
    /// Visible answer text.
    Content(String),
    /// `reasoning_content` — DeepSeek's thinking mode. Kept apart from [`Self::Content`] because
    /// the two are rendered in different places and one is usually collapsed.
    Thinking(String),
    /// The model asked for a tool call. `arguments` is the raw JSON string: parsing it belongs to
    /// the tool, not to the bus.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        ok: bool,
        /// One line for the transcript. Full output belongs in the pane the tool opened.
        summary: String,
    },
    /// The conversation so far, for a session that is being continued.
    ///
    /// Sent once, before the first turn of a resumed session, because a pane that only ever hears
    /// deltas would show an empty transcript while the model answered questions about things it had
    /// been told in a conversation nobody could see.
    History(Vec<jmds_api::ChatMessage>),
    /// Token accounting for the turn, with the cache split out: DeepSeek bills a prompt token
    /// that hit the prefix cache at a fraction of one that missed, so a single prompt number
    /// cannot be turned into a price.
    Usage(TurnUsage),
    TurnFinished {
        reason: FinishReason,
    },
    Error(String),
}

/// What one turn cost, as the API reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub prompt_tokens: u64,
    /// Prompt tokens served from the prefix cache.
    pub cache_hit_tokens: u64,
    /// Prompt tokens that had to be processed.
    pub cache_miss_tokens: u64,
    pub completion_tokens: u64,
    /// How much of `completion_tokens` was reasoning. Read-only, like the API's field: it is
    /// already counted in `completion_tokens` and must not be added to it.
    pub reasoning_tokens: u64,
}

/// Why the model stopped. `ToolCalls` is the one the app acts on: it runs them and asks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    /// `content_filter`, or a reason a newer API version introduced.
    Other(String),
}

/// Something happened to a file under the project root.
///
/// [`Self::EditorWrote`] is the app's own write, and it exists so the watcher can be told which
/// changes are ours: without it every save from the editor comes back as a change, and a pane
/// that reloads on change reloads what it just wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    Changed { path: PathBuf },
    Created { path: PathBuf },
    Removed { path: PathBuf },
    EditorWrote { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaneEvent {
    Opened {
        spec: PaneSpec,
    },
    Closed {
        id: PaneId,
    },
    Focused {
        id: PaneId,
    },
    /// 持有该 pane 的那次分割移动了 —— 比例而不是尺寸（几何在 Pane 树里）。
    Ratio {
        id: PaneId,
        ratio: f32,
    },
    /// A pane renamed itself after it learnt what it is showing — a terminal that ran `cargo
    /// test`, a chat pane that switched model.
    Title {
        id: PaneId,
        title: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Started { id: String },
    Saved { id: String, path: PathBuf },
    Restored { id: String },
    Branched { from: String, to: String },
}

/// Every event, in one type, because a subscriber wants one stream rather than four.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Agent(AgentEvent),
    File(FileEvent),
    Pane(PaneEvent),
    Session(SessionEvent),
}

macro_rules! from_event {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        $(impl From<$ty> for Event {
            fn from(event: $ty) -> Self {
                Event::$variant(event)
            }
        })+
    };
}

from_event!(
    Agent(AgentEvent),
    File(FileEvent),
    Pane(PaneEvent),
    Session(SessionEvent),
);

/// A handle to the bus. Cloning it is how a layer gets one: the clone shares the channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// `capacity` is how far behind a subscriber may fall before it is told it lagged. It is a
    /// memory bound, not a queue depth: a slow subscriber never holds the sender up.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publishing into a bus nobody is listening to is not an error: the engine does not know
    /// whether a TUI is attached, and a headless run must not care.
    pub fn publish(&self, event: impl Into<Event>) {
        let _ = self.tx.send(event.into());
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> Event {
        AgentEvent::Content(text.to_string()).into()
    }

    #[test]
    fn a_subscriber_gets_what_is_published() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.publish(delta("hello"));
        assert_eq!(rx.try_recv().unwrap(), delta("hello"));
    }

    #[test]
    fn every_subscriber_gets_its_own_copy() {
        let bus = EventBus::new(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.subscribers(), 2);
        bus.publish(AgentEvent::TurnStarted {
            model: "deepseek-chat".into(),
        });
        assert_eq!(a.try_recv().unwrap(), b.try_recv().unwrap());
    }

    #[test]
    fn publishing_into_an_empty_bus_is_not_an_error() {
        let bus = EventBus::new(8);
        assert_eq!(bus.subscribers(), 0);
        bus.publish(delta("nobody is listening"));
    }

    #[test]
    fn a_subscriber_that_falls_behind_is_told_so_instead_of_blocking_the_sender() {
        // The point of the bound: a pane that stops reading (a blocked write, a suspended
        // terminal) must not stall the agent loop, and must be *told* it missed something rather
        // than shown a transcript with a silent hole in it.
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();
        for i in 0..5 {
            bus.publish(delta(&i.to_string()));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        // Then it reads what is still buffered — the newest `capacity` messages, oldest first.
        assert_eq!(rx.try_recv().unwrap(), delta("3"));
        assert_eq!(rx.try_recv().unwrap(), delta("4"));
        assert!(rx.try_recv().is_err());
    }
}
