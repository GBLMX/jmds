//! Just My DeepSeek: the terminal side.
//!
//! `jmds-core` deliberately does not depend on `ratatui` — the engine has no opinion about how
//! anything is drawn — so the parts of the terminal that are stated in `ratatui`'s terms live
//! here instead. That split is the whole reason this crate exists: [`terminal`] holds the
//! capability probes (colour depth, kitty keyboard protocol, synchronized output, bracketed
//! paste, tmux), the OSC 11 background probe and the colour maths the two are built on.
//!
//! Rendering and input will live here too, beside [`terminal`], and reach the engine through
//! `jmds-core`'s event bus.

pub mod terminal;
