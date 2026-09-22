//! The look: the colours a theme names, and the glyphs it draws with.
//!
//! Everything here is `ratatui`'s own vocabulary — [`Color`], [`Style`], and plain `&'static str`
//! for the glyphs — because a theme that is not made of the types the renderer draws with is a
//! second rendering model waiting to disagree with the first.
//!
//! # Where this comes from
//!
//! Three projects were mined for their art, and only the parts that survive the trip into Rust and
//! `ratatui` were taken:
//!
//! - **pigma** ([`Look`]/`PaintSpec` and the palette table): a theme is a flat set of named colours
//!   and the surfaces refer to them, `downsampled` maps truecolor onto what the terminal can
//!   actually display, and the background is worked out from the palette's own luminance so the
//!   OSC 11 probe and the theme can be compared. Its hex values are used verbatim below.
//! - **oh-my-pi** (the role vocabulary and the symbol presets): `accent`/`border`/`muted`/`dim`/
//!   `thinkingText`/`toolOutput`-style naming rather than one hue per widget, and `unicode | nerd |
//!   ascii` as the glyph axis. Its `nerd` preset needs a patched font or its 71 KB glyph bundle
//!   shipped over a private-use-area protocol — both are out of scope for a Rust/`ratatui` app with
//!   no font story, so this port has **unicode** and **ascii** only, and no invented PUA codepoints
//!   (a made-up codepoint renders as tofu, which is worse than an ASCII fallback).
//! - **dsh-TUI** (the colour-value grammar, and honouring the terminal's palette): `#rgb`,
//!   `#rrggbb`, `rgb(r,g,b)`, `ansi256(n)` and the sixteen `ansi:` names are accepted, and a theme
//!   may be authored against a `dark | light | ansi` base. Only the grammar is taken — its theme
//!   pipeline is React/Ink, and none of that shape is useful here.
//!
//! Not taken, and why, so nobody goes looking: the glyph protocol and its bundled outlines (needs a
//! TypeScript generator and a terminal that implements the protocol), the shimmer/animation effects
//! (they want a frame clock this app does not have yet), screenshots and image assets (this is a
//! cell-based renderer), and the per-session colour derivation (jmds has one session per process).

use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::terminal::{
    Background, ColorMode, background_from_luminance, color_luminance, rgb_to_16, rgb_to_256,
};

/// A colour as a theme file writes it.
///
/// The forms are the ones dsh-TUI accepts, minus `#rrggbbaa` (ratatui has no alpha channel) and
/// plus the plain ANSI names ratatui spells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColourSpec {
    /// A literal colour, already terminal-relative or truecolor.
    Literal(Color),
    /// The name of a colour field of the theme this appears in — `accent`, `muted`, …
    Named(String),
}

impl ColourSpec {
    /// Read one from text. `None` for anything unrecognised, which the caller decides about.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some(hex) = text.strip_prefix('#') {
            return parse_hex(hex).map(ColourSpec::Literal);
        }
        if let Some(rest) = text.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
            let mut parts = rest.split(',').map(|part| part.trim().parse::<u8>());
            return match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(Ok(r)), Some(Ok(g)), Some(Ok(b)), None) => {
                    Some(ColourSpec::Literal(Color::Rgb(r, g, b)))
                }
                _ => None,
            };
        }
        if let Some(rest) = text
            .strip_prefix("ansi256(")
            .and_then(|r| r.strip_suffix(')'))
        {
            return rest
                .trim()
                .parse::<u8>()
                .ok()
                .map(|index| ColourSpec::Literal(Color::Indexed(index)));
        }
        if let Some(name) = text.strip_prefix("ansi:") {
            return ansi_colour(name).map(ColourSpec::Literal);
        }
        // No sigil: a field name of this theme, or one of ratatui's own colour words.
        match named_colour(text) {
            Some(colour) => Some(ColourSpec::Literal(colour)),
            None => Some(ColourSpec::Named(text.to_string())),
        }
    }
}

/// `rrggbb` or `rgb`, from a `#` form.
fn parse_hex(hex: &str) -> Option<Color> {
    let expand = |c: char| c.to_digit(16).map(|d| d as u8);
    match hex.len() {
        3 => {
            let mut digits = hex.chars().map(expand);
            match (digits.next(), digits.next(), digits.next(), digits.next()) {
                (Some(Some(r)), Some(Some(g)), Some(Some(b)), None) => {
                    Some(Color::Rgb(r * 17, g * 17, b * 17))
                }
                _ => None,
            }
        }
        6 => {
            let value = u32::from_str_radix(hex, 16).ok()?;
            Some(Color::Rgb(
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ))
        }
        _ => None,
    }
}

/// The sixteen ANSI names, as ratatui spells them.
fn ansi_colour(name: &str) -> Option<Color> {
    Some(match name {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "blackBright" => Color::DarkGray,
        "redBright" => Color::LightRed,
        "greenBright" => Color::LightGreen,
        "yellowBright" => Color::LightYellow,
        "blueBright" => Color::LightBlue,
        "magentaBright" => Color::LightMagenta,
        "cyanBright" => Color::LightCyan,
        "whiteBright" => Color::White,
        _ => return None,
    })
}

/// ratatui's own colour words, which a theme file may use directly.
fn named_colour(name: &str) -> Option<Color> {
    Some(match name {
        "reset" | "default" => Color::Reset,
        "black" => Color::Black,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        "red" => Color::Red,
        "lightred" => Color::LightRed,
        _ => return ansi_colour(name),
    })
}

/// The names a theme's colours can be referred to by.
///
/// A surface names a *role* rather than a hue, which is what lets one palette retint the whole app
/// without anyone having to guess which of seven colours a widget meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Text,
    Dim,
    Muted,
    Accent,
    Border,
    BorderFocused,
    Success,
    Warn,
    Error,
    /// What the human typed.
    User,
    /// What the model answered.
    Assistant,
    /// `reasoning_content`, which is shown apart from the answer.
    Thinking,
    /// A tool call and its result.
    Tool,
    /// A tool that failed, and anything else the transcript marks as bad news.
    ToolFailed,
    /// The prompt on the input line.
    Prompt,
}

impl Role {
    /// Every role, for tests and for a theme editor that has to cover them all.
    pub const ALL: [Self; 15] = [
        Self::Text,
        Self::Dim,
        Self::Muted,
        Self::Accent,
        Self::Border,
        Self::BorderFocused,
        Self::Success,
        Self::Warn,
        Self::Error,
        Self::User,
        Self::Assistant,
        Self::Thinking,
        Self::Tool,
        Self::ToolFailed,
        Self::Prompt,
    ];

    /// Lowercase, as it reads in a theme file.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Dim => "dim",
            Self::Muted => "muted",
            Self::Accent => "accent",
            Self::Border => "border",
            Self::BorderFocused => "borderFocused",
            Self::Success => "success",
            Self::Warn => "warning",
            Self::Error => "error",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::Tool => "tool",
            Self::ToolFailed => "toolFailed",
            Self::Prompt => "prompt",
        }
    }
}

/// One theme's colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub text: Color,
    pub dim: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focused: Color,
    pub success: Color,
    pub warn: Color,
    pub error: Color,
    pub user: Color,
    pub assistant: Color,
    pub thinking: Color,
    pub tool: Color,
    pub tool_failed: Color,
    pub prompt: Color,
    /// The theme's own background, or [`Color::Reset`] to mean "whatever the terminal has".
    pub bg: Color,
}

impl Palette {
    pub fn colour(&self, role: Role) -> Color {
        match role {
            Role::Text => self.text,
            Role::Dim => self.dim,
            Role::Muted => self.muted,
            Role::Accent => self.accent,
            Role::Border => self.border,
            Role::BorderFocused => self.border_focused,
            Role::Success => self.success,
            Role::Warn => self.warn,
            Role::Error => self.error,
            Role::User => self.user,
            Role::Assistant => self.assistant,
            Role::Thinking => self.thinking,
            Role::Tool => self.tool,
            Role::ToolFailed => self.tool_failed,
            Role::Prompt => self.prompt,
        }
    }

    /// Which of [`Role::ALL`] this palette's colours are, so a theme file's overrides can be
    /// resolved by name.
    pub fn lookup(&self, name: &str) -> Option<Color> {
        Role::ALL
            .into_iter()
            .find(|role| role.name() == name)
            .map(|role| self.colour(role))
    }

    /// The same palette in what the terminal can display.
    ///
    /// Applied once when a theme is loaded, so nothing on the render path has to know about the
    /// terminal's capabilities. Named and indexed colours are already terminal-relative and pass
    /// through; only truecolor is mapped.
    pub fn downsampled(&self, mode: ColorMode) -> Palette {
        if mode == ColorMode::TrueColor {
            return self.clone();
        }
        let map = |colour: Color| downsample(colour, mode);
        Palette {
            text: map(self.text),
            dim: map(self.dim),
            muted: map(self.muted),
            accent: map(self.accent),
            border: map(self.border),
            border_focused: map(self.border_focused),
            success: map(self.success),
            warn: map(self.warn),
            error: map(self.error),
            user: map(self.user),
            assistant: map(self.assistant),
            thinking: map(self.thinking),
            tool: map(self.tool),
            tool_failed: map(self.tool_failed),
            prompt: map(self.prompt),
            bg: map(self.bg),
        }
    }

    /// Whether this palette's own background is light, which is what decides whether the app paints
    /// over the terminal's background. The threshold is the probe's, so the two answers compare.
    pub fn background(&self) -> Background {
        // `Reset` — the terminal's own background — says nothing about itself, so the probe's
        // assumption is the fallback. A colour that cannot be measured at all is dark by the same
        // rule.
        color_luminance(self.bg).map_or(Background::Dark, background_from_luminance)
    }
}

/// Map a truecolor value onto what the terminal can display.
///
/// Named and indexed colours are already terminal-relative and pass through; only `Rgb` is mapped,
/// so a 256-colour terminal stops receiving truecolor escapes it cannot render.
fn downsample(colour: Color, mode: ColorMode) -> Color {
    match colour {
        Color::Rgb(r, g, b) => match mode {
            ColorMode::TrueColor => colour,
            ColorMode::Ansi256 => Color::Indexed(rgb_to_256(r, g, b)),
            ColorMode::Basic => Color::Indexed(rgb_to_16(r, g, b)),
        },
        other => other,
    }
}

/// Which glyph set a theme draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSet {
    /// Box drawing and the punctuation that terminals have had for decades.
    Unicode,
    /// The same shapes in the printable ASCII range, for a terminal or a font where the above
    /// comes out wrong.
    Ascii,
}

/// The characters a pane draws with.
///
/// Borrowed `&'static str`, because a glyph table is data that lives as long as the process and
/// copying fragments of it per frame is work for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Before what the human typed.
    pub prompt: &'static str,
    /// Before reasoning.
    pub thinking: &'static str,
    /// Before a tool call, and before its result.
    pub tool: &'static str,
    /// Before something that failed.
    pub failure: &'static str,
    /// Marks where long output was folded away.
    pub folded: &'static str,
    /// Written before the input line while the caret is somewhere else in it.
    pub caret: &'static str,
    /// Before a line the app itself is saying, rather than the model or the human.
    pub note: &'static str,
    /// Which set these glyphs came from, so switching theme can keep the choice.
    pub set: GlyphSet,
    /// The box a pane is drawn in.
    pub box_round: BoxGlyphs,
    /// What a working indicator cycles through. Braille, because its cells are dot-aligned and the
    /// animation reads as motion rather than as changing text.
    pub spinner: &'static [&'static str],
}

/// The six characters a box needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxGlyphs {
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
}

impl Glyphs {
    pub fn for_set(set: GlyphSet) -> Self {
        match set {
            GlyphSet::Unicode => Self {
                prompt: "› ",
                thinking: "· ",
                tool: "::",
                failure: "!! ",
                folded: "…",
                caret: "▏",
                note: "· ",
                set,
                box_round: BoxGlyphs {
                    top_left: "╭",
                    top_right: "╮",
                    bottom_left: "╰",
                    bottom_right: "╯",
                    horizontal: "─",
                    vertical: "│",
                },
                spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            },
            GlyphSet::Ascii => Self {
                prompt: "> ",
                thinking: "- ",
                tool: "::",
                failure: "!! ",
                folded: "...",
                caret: "|",
                note: "- ",
                set,
                box_round: BoxGlyphs {
                    top_left: "+",
                    top_right: "+",
                    bottom_left: "+",
                    bottom_right: "+",
                    horizontal: "-",
                    vertical: "|",
                },
                spinner: &["|", "/", "-", "\\"],
            },
        }
    }

    /// The ratatui border for the pane frames. The glyph table says which set to use; ratatui draws
    /// the six characters itself, because its box renderer knows how the corners meet a title.
    pub fn border(self) -> ratatui::widgets::BorderType {
        match self.box_round.vertical {
            "|" => ratatui::widgets::BorderType::Plain,
            _ => ratatui::widgets::BorderType::Rounded,
        }
    }
}

/// A theme: a name, a palette, and the glyphs it draws with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
    pub glyphs: Glyphs,
}

impl Default for Theme {
    fn default() -> Self {
        Self::named("terminal").expect("`terminal` is a built-in theme")
    }
}

impl Theme {
    /// Every built-in theme's name, for anything that offers a choice.
    ///
    /// Kept next to [`Theme::named`] because the two have to agree: a name offered here that does not
    /// resolve is a menu entry that fails, and the test below is what keeps them honest.
    pub const NAMES: &'static [&'static str] = &[
        "terminal",
        "default",
        "dracula",
        "nord",
        "gruvbox",
        "tokyo-night",
        "catppuccin",
        "one-dark",
    ];

    /// A built-in theme by name.
    pub fn named(name: &str) -> Option<Self> {
        let palette = match name {
            "terminal" => terminal_palette(),
            "default" => dark_palette(Color::Rgb(0x0e, 0x0e, 0x0e), Color::Rgb(0xc2, 0x0c, 0x0c)),
            "dracula" => palette_from(
                "#282a36", "#f8f8f2", "#6272a4", "#bd93f9", "#6272a4", "#50fa7b", "#f1fa8c",
                "#ff5555", "#8be9fd",
            ),
            "nord" => palette_from(
                "#2e3440", "#eceff4", "#616e88", "#88c0d0", "#616e88", "#a3be8c", "#ebcb8b",
                "#bf616a", "#81a1c1",
            ),
            "gruvbox" => palette_from(
                "#282828", "#ebdbb2", "#928374", "#d65d0e", "#928374", "#b8bb26", "#fabd2f",
                "#fb4934", "#83a598",
            ),
            "tokyo-night" => palette_from(
                "#1a1b26", "#c0caf5", "#565f89", "#7aa2f7", "#565f89", "#9ece6a", "#e0af68",
                "#f7768e", "#bb9af7",
            ),
            "catppuccin" => palette_from(
                "#1e1e2e", "#cdd6f4", "#6c7086", "#cba6f7", "#6c7086", "#a6e3a1", "#f9e2af",
                "#f38ba8", "#89b4fa",
            ),
            "one-dark" => palette_from(
                "#282c34", "#abb2bf", "#5c6370", "#61afef", "#5c6370", "#98c379", "#e5c07b",
                "#e06c75", "#c678dd",
            ),
            _ => return None,
        };
        Some(Self {
            name: name.to_string(),
            palette,
            glyphs: Glyphs::for_set(GlyphSet::Unicode),
        })
    }

    /// Every built-in name, in the order a picker should show them: the terminal's own colours
    /// first, because that is the one that is always legible.
    pub fn builtin_names() -> &'static [&'static str] {
        &[
            "terminal",
            "default",
            "dracula",
            "nord",
            "gruvbox",
            "tokyo-night",
            "catppuccin",
            "one-dark",
        ]
    }

    /// The same theme in what the terminal can display.
    pub fn downsampled(&self, mode: ColorMode) -> Self {
        Self {
            name: self.name.clone(),
            palette: self.palette.downsampled(mode),
            glyphs: self.glyphs,
        }
    }

    pub fn with_glyphs(mut self, set: GlyphSet) -> Self {
        self.glyphs = Glyphs::for_set(set);
        self
    }

    /// The styles the panes draw with, worked out from the palette in one place.
    ///
    /// A surface that wants its own colour goes through a role; a surface that reaches past this for
    /// a raw [`Color`] is how a retheme ends up half-applied.
    pub fn styles(&self) -> Styles {
        let p = &self.palette;
        Styles {
            text: Style::default().fg(p.text),
            dim: Style::default().fg(p.dim).add_modifier(Modifier::DIM),
            muted: Style::default().fg(p.muted),
            accent: Style::default().fg(p.accent),
            border: Style::default().fg(p.border),
            border_focused: Style::default()
                .fg(p.border_focused)
                .add_modifier(Modifier::BOLD),
            success: Style::default().fg(p.success),
            warn: Style::default().fg(p.warn),
            error: Style::default().fg(p.error).add_modifier(Modifier::BOLD),
            user: Style::default().fg(p.user).add_modifier(Modifier::BOLD),
            assistant: Style::default().fg(p.assistant),
            thinking: Style::default()
                .fg(p.thinking)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
            tool: Style::default().fg(p.tool),
            tool_failed: Style::default().fg(p.tool_failed),
            prompt: Style::default().fg(p.prompt).add_modifier(Modifier::BOLD),
        }
    }

    /// How wide a glyph is, in cells. Used by the tests, and by anything that has to lay glyphs out.
    pub fn width_of(glyph: &str) -> usize {
        glyph.width()
    }
}

/// The styles a pane draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Styles {
    pub text: Style,
    pub dim: Style,
    pub muted: Style,
    pub accent: Style,
    pub border: Style,
    pub border_focused: Style,
    pub success: Style,
    pub warn: Style,
    pub error: Style,
    pub user: Style,
    pub assistant: Style,
    pub thinking: Style,
    pub tool: Style,
    pub tool_failed: Style,
    pub prompt: Style,
}

/// The terminal's own sixteen colours: the theme that cannot be illegible, because the terminal's
/// palette is the one the user already chose.
fn terminal_palette() -> Palette {
    Palette {
        text: Color::Reset,
        dim: Color::Indexed(8),
        muted: Color::Indexed(8),
        accent: Color::Indexed(6),
        border: Color::Indexed(8),
        border_focused: Color::Indexed(6),
        success: Color::Indexed(2),
        warn: Color::Indexed(3),
        error: Color::Indexed(1),
        user: Color::Indexed(6),
        assistant: Color::Reset,
        thinking: Color::Indexed(8),
        tool: Color::Indexed(4),
        tool_failed: Color::Indexed(1),
        prompt: Color::Indexed(2),
        bg: Color::Reset,
    }
}

/// The dark palette the truecolor themes are written on, before their own colours.
fn dark_palette(bg: Color, accent: Color) -> Palette {
    Palette {
        text: Color::Rgb(0xff, 0xff, 0xff),
        dim: Color::Rgb(0x55, 0x55, 0x55),
        muted: Color::Rgb(0x88, 0x88, 0x88),
        accent,
        border: Color::Rgb(0x55, 0x55, 0x55),
        border_focused: accent,
        success: Color::Rgb(0x98, 0xc3, 0x79),
        warn: Color::Rgb(0xe5, 0xc0, 0x7b),
        error: Color::Rgb(0xf4, 0x53, 0x5a),
        user: accent,
        assistant: Color::Rgb(0xff, 0xff, 0xff),
        thinking: Color::Rgb(0x77, 0x77, 0x77),
        tool: Color::Rgb(0x61, 0xaf, 0xef),
        tool_failed: Color::Rgb(0xf4, 0x53, 0x5a),
        prompt: accent,
        bg,
    }
}

/// One of the truecolor themes, from the nine colours that actually differ between them.
#[allow(clippy::too_many_arguments)]
fn palette_from(
    bg: &str,
    text: &str,
    muted: &str,
    accent: &str,
    border: &str,
    success: &str,
    warn: &str,
    error: &str,
    tool: &str,
) -> Palette {
    let c = |spec: &str| match ColourSpec::parse(spec) {
        Some(ColourSpec::Literal(colour)) => colour,
        _ => Color::Reset,
    };
    Palette {
        text: c(text),
        dim: c(muted),
        muted: c(muted),
        accent: c(accent),
        border: c(border),
        border_focused: c(accent),
        success: c(success),
        warn: c(warn),
        error: c(error),
        user: c(accent),
        assistant: c(text),
        thinking: c(muted),
        tool: c(tool),
        tool_failed: c(error),
        prompt: c(success),
        bg: c(bg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_theme_uses_only_the_terminals_own_colours() {
        // This one is the default because it cannot be illegible: every colour in it is either the
        // terminal's default foreground or one of its palette indices.
        let palette = terminal_palette();
        for role in Role::ALL {
            let colour = palette.colour(role);
            assert!(
                matches!(colour, Color::Reset | Color::Indexed(_)),
                "{:?} is {colour:?}, which is a colour the terminal was not asked about",
                role
            );
        }
    }

    #[test]
    fn every_built_in_theme_names_every_role_and_a_way_back() {
        for name in Theme::builtin_names() {
            let theme = Theme::named(name).unwrap_or_else(|| panic!("{name} should exist"));
            assert_eq!(&theme.name, name);
            for role in Role::ALL {
                // The value itself does not matter; that it is reachable by name does, because a
                // theme file overrides colours by name.
                assert_eq!(
                    theme.palette.lookup(role.name()),
                    Some(theme.palette.colour(role))
                );
            }
        }
        assert!(Theme::named("no-such-theme").is_none());
    }

    #[test]
    fn downsampling_leaves_no_truecolor_for_a_terminal_that_cannot_show_it() {
        let theme = Theme::named("dracula").unwrap();
        for mode in [ColorMode::Ansi256, ColorMode::Basic] {
            let reduced = theme.downsampled(mode);
            for role in Role::ALL {
                let colour = reduced.palette.colour(role);
                assert!(
                    !matches!(colour, Color::Rgb(..)),
                    "{:?} survived as {colour:?} in {mode:?}",
                    role
                );
            }
        }
        // And a truecolor terminal gets the theme it asked for, untouched.
        let same = theme.downsampled(ColorMode::TrueColor);
        assert_eq!(same, theme);
    }

    #[test]
    fn a_light_theme_says_so_from_its_own_colours() {
        let dark = Theme::named("dracula").unwrap();
        assert_eq!(dark.palette.background(), Background::Dark);

        let mut light = dark.clone();
        light.palette.bg = Color::Rgb(0xf8, 0xf8, 0xf2);
        assert_eq!(light.palette.background(), Background::Light);

        // `Reset` says nothing about itself, so it falls back to the probe's assumption.
        let terminal = Theme::named("terminal").unwrap();
        assert_eq!(terminal.palette.background(), Background::Dark);
    }

    #[test]
    fn a_colour_can_be_written_the_ways_the_grammar_allows() {
        assert_eq!(
            ColourSpec::parse("#bd93f9"),
            Some(ColourSpec::Literal(Color::Rgb(0xbd, 0x93, 0xf9)))
        );
        assert_eq!(
            ColourSpec::parse("#abc"),
            Some(ColourSpec::Literal(Color::Rgb(0xaa, 0xbb, 0xcc)))
        );
        assert_eq!(
            ColourSpec::parse("rgb( 1 , 2 , 3 )"),
            Some(ColourSpec::Literal(Color::Rgb(1, 2, 3)))
        );
        assert_eq!(
            ColourSpec::parse("ansi256(17)"),
            Some(ColourSpec::Literal(Color::Indexed(17)))
        );
        assert_eq!(
            ColourSpec::parse("ansi:redBright"),
            Some(ColourSpec::Literal(Color::LightRed))
        );
        assert_eq!(
            ColourSpec::parse("blue"),
            Some(ColourSpec::Literal(Color::Blue))
        );

        // A bare word that is not a colour is a field of the theme being read.
        assert_eq!(
            ColourSpec::parse("myAccent"),
            Some(ColourSpec::Named("myAccent".into()))
        );

        // Nothing here is allowed to guess: an out-of-range channel or a truncated hex is `None`.
        assert_eq!(ColourSpec::parse("rgb(1,2,300)"), None);
        assert_eq!(ColourSpec::parse("#12345"), None);
        assert_eq!(ColourSpec::parse("ansi256(256)"), None);
        assert_eq!(ColourSpec::parse("ansi:burnt"), None);
    }

    #[test]
    fn a_glyph_set_is_printable_and_its_box_glyphs_are_exactly_one_cell() {
        for set in [GlyphSet::Unicode, GlyphSet::Ascii] {
            let glyphs = Glyphs::for_set(set);

            // The markers are prefixes and may carry a trailing space.
            let markers = [
                glyphs.prompt,
                glyphs.thinking,
                glyphs.tool,
                glyphs.failure,
                glyphs.folded,
                glyphs.caret,
            ];
            for marker in markers {
                assert!(!marker.is_empty(), "{set:?} has an empty marker");
                assert!(
                    !marker.chars().any(char::is_control),
                    "{set:?} has a control character in {marker:?}"
                );
                assert!(
                    (1..=3).contains(&marker.width()),
                    "{set:?}: the marker {marker:?} is {} cells, which is not a prefix",
                    marker.width()
                );
            }

            // The box is a frame, and a frame character that is not one cell wide does not join up.
            let frame = [
                glyphs.box_round.top_left,
                glyphs.box_round.top_right,
                glyphs.box_round.bottom_left,
                glyphs.box_round.bottom_right,
                glyphs.box_round.horizontal,
                glyphs.box_round.vertical,
            ];
            for glyph in frame {
                assert_eq!(
                    glyph.width(),
                    1,
                    "{set:?}: the box glyph {glyph:?} is not one cell wide"
                );
            }

            for frame in glyphs.spinner {
                assert!(!frame.is_empty(), "{set:?} has an empty spinner frame");
            }
            assert!(glyphs.spinner.len() > 1, "{set:?} cannot animate");
        }
    }

    #[test]
    fn the_ascii_set_stays_inside_ascii() {
        // The whole point of this set: a terminal or font where the box-drawing characters come out
        // wrong still gets a usable frame.
        let glyphs = Glyphs::for_set(GlyphSet::Ascii);
        let all = format!(
            "{}{}{}{}{}{}{}{}{}{}{}{}",
            glyphs.prompt,
            glyphs.thinking,
            glyphs.tool,
            glyphs.failure,
            glyphs.folded,
            glyphs.caret,
            glyphs.box_round.top_left,
            glyphs.box_round.top_right,
            glyphs.box_round.bottom_left,
            glyphs.box_round.bottom_right,
            glyphs.box_round.horizontal,
            glyphs.box_round.vertical
        );
        assert!(all.is_ascii(), "{all:?} is not ASCII");
        for frame in glyphs.spinner {
            assert!(frame.is_ascii(), "{frame:?} is not ASCII");
        }
        assert_eq!(glyphs.border(), ratatui::widgets::BorderType::Plain);
        assert_eq!(
            Glyphs::for_set(GlyphSet::Unicode).border(),
            ratatui::widgets::BorderType::Rounded
        );
    }

    #[test]
    fn a_theme_and_its_glyph_set_are_chosen_separately() {
        let theme = Theme::named("nord").unwrap().with_glyphs(GlyphSet::Ascii);
        assert_eq!(theme.glyphs, Glyphs::for_set(GlyphSet::Ascii));
        assert_eq!(theme.name, "nord");
        assert_eq!(
            Theme::named("nord").unwrap().glyphs,
            Glyphs::for_set(GlyphSet::Unicode),
            "the default set is the pretty one"
        );
    }

    #[test]
    fn the_styles_a_pane_draws_with_come_from_the_palette() {
        let theme = Theme::named("dracula").unwrap();
        let styles = theme.styles();
        assert_eq!(styles.text.fg, Some(theme.palette.text));
        assert_eq!(styles.border.fg, Some(theme.palette.border));
        assert_eq!(styles.border_focused.fg, Some(theme.palette.border_focused));
        assert_eq!(styles.user.fg, Some(theme.palette.user));
        assert_eq!(styles.assistant.fg, Some(theme.palette.assistant));
        // Reasoning is dim and italic so it reads as not the answer even in one colour.
        assert!(styles.thinking.add_modifier.contains(Modifier::ITALIC));
        assert!(styles.thinking.add_modifier.contains(Modifier::DIM));
    }
}
