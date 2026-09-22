//! What the terminal can do, and how to talk to it.
//!
//! Four subjects, one file each behind this facade:
//!
//! - `capability` — what the terminal is: colour depth, cursor shape, the tmux it sits behind.
//! - `color` — the colour maths those answers are built on: the palettes, and the luminance a
//!   colour works out to.
//! - `background` — the terminal's own background, with the per-platform probe under it
//!   (`background::unix`, `background::windows`).
//! - `sequences` — the escape sequences the app emits.
//!
//! Every item is re-exported here, so callers keep addressing `jmds_tui::terminal::<item>`.
//! `sequences` and `background` reach each other through the items themselves (a `pub(super)`
//! helper, a child module), not through this facade.

mod background;
mod capability;
// The colour maths' other caller is the theme layer colour.rs names in its own header — the one
// that down-samples a colour for the terminal's palette and asks what the result still means.
// That layer is not ported yet, so `palette_rgb` and `color_luminance` have no caller outside the
// tests; the allow is exactly that gap, and it goes away with the layer.
#[allow(dead_code)]
mod color;
mod sequences;

pub use background::{
    BACKGROUND, Background, BackgroundFill, BackgroundMode, parse_colorfgbg, parse_osc11_luminance,
    query_background_luminance,
};
pub use capability::{COLOR_MODE, ColorMode, CursorStyle, color_mode_from, tmux_detected};
pub use color::{rgb_to_16, rgb_to_256};
pub use sequences::{
    begin_synchronized_update, disable_terminal_modes, enable_terminal_modes,
    end_synchronized_update, notify,
};

// The `pub(crate)` colour items keep their path here. The tests in `background` are what read
// them through this facade — the runtime callers name `color` directly — so a build without
// `--tests` reads none of them here at all, which is what the allow is for.
#[allow(unused_imports)]
pub(crate) use color::{
    ANSI_16, background_from_luminance, color_luminance, palette_rgb, rgb_luminance,
};
