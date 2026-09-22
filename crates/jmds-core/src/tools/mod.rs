//! The four tools, and the shared rules they obey.
//!
//! DeepSeek is the only model here and the file system is the interface, so the tool set is small
//! on purpose: [`read`], `write`, `edit` and `bash` — no search tool (that is `bash`), no listing
//! tool (that is `bash`), no way to ask a question of a language server.
//!
//! What they share lives here rather than in each of them:
//!
//! - [`truncate`] — how output is bounded, and how the model is told what it is not seeing. The
//!   bound is not a detail: a tool that silently drops half a file teaches the model that the file
//!   is short.
//! - [`queue`] — one writer per file. A turn can ask for several edits at once; without a queue
//!   those edits interleave and the last one wins.
//!
//! Each tool's *schema* (what the model may pass) is part of this module too, because the schema
//! and the implementation drifting apart is the failure mode that matters: a parameter the model
//! is told about but that nothing reads is worse than no parameter.

pub mod edit;
pub mod queue;
pub mod read;
pub mod truncate;
pub mod write;
// `bash` lands next; it bounds its output with `truncate` the same way `read` does.
