//! What the terminal is: the colour depth it can show, the shape of its cursor, and the
//! terminal-family and multiplexer it sits behind.
//!
//! Everything here is a fact about the terminal, or a decision made from one. The bytes that
//! act on those facts live in `sequences`.
//!
//! Image protocols are deliberately absent: jmds draws no images, so the kitty/iTerm2/sixel
//! encoders and the query that chose between them are not part of this crate.

use std::{env, sync::LazyLock};

use serde::{Deserialize, Serialize};

/// Shape of the terminal's cursor while an input field has focus.
///
/// Kitty's `tui.json` and opencode's are the model: the app asks for a shape, and the
/// terminal's own default stays available as a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    /// Whatever the user configured in the terminal.
    #[default]
    Default,
    Block,
    Underline,
    Bar,
}

impl CursorStyle {
    /// Every style, in the order a picker should offer them.
    ///
    /// The two names are inherent rather than a trait's: a config picker is the only thing that
    /// wants them, and it can ask the enum directly.
    pub const ALL: &'static [Self] = &[Self::Default, Self::Block, Self::Underline, Self::Bar];

    /// The name the setting is written as.
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Block => "block",
            Self::Underline => "underline",
            Self::Bar => "bar",
        }
    }

    /// The escape sequence this style is written as.
    pub fn command(self) -> crossterm::cursor::SetCursorStyle {
        use crossterm::cursor::SetCursorStyle;
        match self {
            Self::Default => SetCursorStyle::DefaultUserShape,
            Self::Block => SetCursorStyle::SteadyBlock,
            Self::Underline => SetCursorStyle::SteadyUnderScore,
            Self::Bar => SetCursorStyle::SteadyBar,
        }
    }
}

/// Whether the app is running inside tmux.
///
/// Two markers, both tmux's own: `TERM` starts with `tmux` (tmux's `default-terminal`), and
/// `TERM_PROGRAM` is exactly `tmux` where tmux sets it. `screen` is deliberately not matched —
/// the prefix alone cannot tell tmux from screen.
///
/// The predicate is the one `ratatui-image` builds in `tmux_detected`
/// (`detect_tmux_and_outer_protocol_from_env`, `ratatui-image-11.1.0/src/picker.rs:341`) — kept
/// here because the multiplexer question outlives the graphics protocol that first needed it.
pub fn tmux_detected(lookup: &impl Fn(&str) -> Option<String>) -> bool {
    lookup("TERM").is_some_and(|term| term.starts_with("tmux"))
        || matches!(lookup("TERM_PROGRAM").as_deref(), Some("tmux"))
}

/// Whether the terminal is kitty or ghostty, told from the variables each of them sets.
///
/// For [`notify`](super::notify), which picks kitty's richer notification sequence when it
/// recognises one; `TERM_PROGRAM` alone is not enough, because a shell that never set it does
/// not pass it on to the app.
pub(super) fn is_kitty_terminal(lookup: &impl Fn(&str) -> Option<String>) -> bool {
    if lookup("KITTY_WINDOW_ID").is_some()
        || lookup("KITTY_PID").is_some()
        || lookup("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return true;
    }

    if matches!(lookup("TERM_PROGRAM").as_deref(), Some("kitty" | "ghostty")) {
        return true;
    }

    matches!(
        lookup("TERM").as_deref(),
        Some(t) if t.to_lowercase().contains("kitty") || t == "xterm-ghostty"
    )
}

/// How many colors the terminal can display; themes are down-sampled to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    #[default]
    TrueColor,
    Ansi256,
    Basic,
}

/// Terminals that advertise true color in their own font/graphics era even when
/// `COLORTERM` is missing (older builds, ssh sessions without the variable).
const TRUE_COLOR_PROGRAMS: [&str; 6] = [
    "kitty",
    "wezterm",
    "alacritty",
    "ghostty",
    "iTerm.app",
    "vscode",
];

/// Color capability of the current terminal, detected once from the environment.
pub static COLOR_MODE: LazyLock<ColorMode> =
    LazyLock::new(|| color_mode_from(|key| env::var(key).ok()));

/// Detect the color capability from environment values.
///
/// `COLORTERM`/`WT_SESSION` mean true color; `dumb` and the Linux VGA console only have
/// the 16 base colors; `-256color` terms get the 256-color palette; anything else is
/// treated as 256 colors, which every terminal of the last two decades supports.
pub fn color_mode_from(lookup: impl Fn(&str) -> Option<String>) -> ColorMode {
    if let Some(value) = lookup("COLORTERM") {
        let value = value.to_ascii_lowercase();
        if value.contains("truecolor") || value.contains("24bit") {
            return ColorMode::TrueColor;
        }
    }
    if lookup("WT_SESSION").is_some() {
        return ColorMode::TrueColor;
    }

    let term = lookup("TERM").unwrap_or_default().to_ascii_lowercase();
    if term.is_empty() || term == "dumb" || term == "linux" {
        return ColorMode::Basic;
    }
    if term.contains("256") {
        return ColorMode::Ansi256;
    }
    if let Some(program) = lookup("TERM_PROGRAM")
        && TRUE_COLOR_PROGRAMS
            .iter()
            .any(|known| program.eq_ignore_ascii_case(known))
    {
        return ColorMode::TrueColor;
    }
    ColorMode::Ansi256
}

#[cfg(test)]
mod terminal_mode_tests {
    use std::collections::HashMap;

    use super::{
        super::{
            begin_synchronized_update, disable_terminal_modes, enable_terminal_modes,
            end_synchronized_update,
        },
        *,
    };

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    /// Kitty is recognised from the variables kitty itself sets, not only from `TERM_PROGRAM` —
    /// which is absent when the shell that launched it did not set it. `notify` picks its
    /// notification form from this answer, so the detection has to work for a terminal that never
    /// announced itself any other way.
    #[test]
    fn kitty_is_recognised_from_its_own_variables() {
        for env in [
            vec![("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")],
            vec![("KITTY_PID", "42")],
            vec![("TERM_PROGRAM", "ghostty")],
        ] {
            assert!(is_kitty_terminal(&env_of(&env)), "{env:?}");
        }

        assert!(!is_kitty_terminal(&env_of(&[("TERM", "xterm-256color")])));
    }

    /// tmux is recognised from the markers tmux itself sets and from nothing else: `screen` may
    /// be sitting behind it, so its `TERM` prefix alone proves nothing.
    #[test]
    fn tmux_is_recognised_from_its_own_markers() {
        assert!(tmux_detected(&env_of(&[("TERM", "tmux-256color")])));
        assert!(tmux_detected(&env_of(&[
            ("TERM", "screen-256color"),
            ("TERM_PROGRAM", "tmux"),
        ])));

        assert!(!tmux_detected(&env_of(&[("TERM", "screen-256color")])));
        assert!(!tmux_detected(&env_of(&[("TERM", "xterm-kitty")])));
        assert!(!tmux_detected(&env_of(&[])));
    }

    /// These bytes are the contract with the terminal, and the pop is the one that
    /// matters most: a terminal left in the keyboard protocol feeds the shell after
    /// jmds `CSI u` encodings instead of plain keys.
    #[test]
    fn terminal_modes_are_entered_and_left_with_the_documented_sequences() {
        let mut out = Vec::new();
        enable_terminal_modes(&mut out).expect("enable");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\u{1b}[?2004h\u{1b}[>1u",
            "bracketed paste on, then keyboard protocol with disambiguation"
        );

        let mut out = Vec::new();
        disable_terminal_modes(&mut out).expect("disable");
        // The spec's pop is `CSI < number u`, number defaulting to 1, so the explicit
        // form is the documented one. It has to be written while still on the
        // alternate screen: the main and alternate screens keep separate stacks.
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\u{1b}[?2004l\u{1b}[<1u",
            "paste off, keyboard protocol popped"
        );
    }

    /// One frame lives between these two, and kitty drops a frame whose end never
    /// arrives — so the loop must always write both.
    #[test]
    fn synchronized_updates_bracket_a_frame() {
        let mut out = Vec::new();
        begin_synchronized_update(&mut out).expect("begin");
        end_synchronized_update(&mut out).expect("end");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\u{1b}[?2026h\u{1b}[?2026l"
        );
    }
}
