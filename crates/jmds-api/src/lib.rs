//! Just My DeepSeek: the DeepSeek client.
//!
//! Everything here is about one provider, which is the point of the project: the shapes are
//! DeepSeek's rather than the largest common denominator of several vendors.
//!
//! - [`message`] — the request side: roles, tool calls, and the reasoning-replay obligation
//! - [`usage`] — what a turn cost, in the three shapes DeepSeek reports it in
//! - [`stream`] — the response side: SSE framing and the deltas inside it
//! - [`client`] — the HTTP call: one request shape, retries that stop once the answer starts
//!
//! The client itself (HTTP, retries, cancellation) sits on top of these; the engine in
//! `jmds-core` is what turns these events into the ones the app reacts to.

pub mod client;
pub mod message;
pub mod stream;
pub mod usage;

pub use client::{ApiError, Client, ClientConfig, ToolSpec};
pub use message::{
    ChatMessage, Role, ToolCall, ToolCallFunction, enforce_reasoning_replay,
    requires_reasoning_replay,
};
pub use stream::{FinishReason, SseBuffer, StreamEvent, parse_delta};
pub use usage::Usage;
