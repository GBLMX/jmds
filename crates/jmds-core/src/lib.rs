//! Just My DeepSeek: the engine.
//!
//! Everything that is not a terminal lives here — the event bus every layer talks through
//! ([`event`]), where panes sit in the layout ([`pane`]), the configuration file ([`config`]),
//! the log sink ([`logger`]), where the app keeps its files ([`paths`]) and the tools the
//! agent drives the file system with ([`tools`]). Rendering and input
//! stay in `jmds-tui`; the model lives behind `jmds-api`. Both talk to this crate and never to
//! each other, which is what keeps a PTY and a chat completion from having opinions about each
//! other.

pub mod agent;
pub mod config;
pub mod event;
pub mod logger;
pub mod pane;
pub mod paths;
pub mod session;
pub mod tools;
