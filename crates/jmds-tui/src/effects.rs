//! The moving parts of the look: a colour ramp, a highlight that travels along text, and a spinner.
//!
//! These are written here rather than taken from anywhere, and they are deliberately small: each is
//! a pure function of text and a number, so they can be drawn every frame, tested without a
//! terminal, and animated by whatever owns a clock. Nothing here holds state or owns a timer — a
//! frame is a function of `(text, phase)`.
//!
//! Two rules the whole module obeys:
//!
//! - **A character is never split.** Every effect works per character, so a colour ramp cannot land
//!   in the middle of a multi-byte character or between the two cells of a wide one.
//! - **A colour that is not truecolor is not interpolated.** A ramp between two palette indices has
//!   no meaningful midpoint — index 3 is not "between" 1 and 5 — so such a ramp snaps at the
//!   halfway point instead of inventing a colour the terminal was never asked about. The same is
//!   true of `Reset`, which means "whatever the terminal does", and blending that is nonsense.

use ratatui::{
    style::{Color, Style},
    text::Span,
};

/// A colour ramp, sampled anywhere along its length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ramp {
    pub from: Color,
    pub to: Color,
}

impl Ramp {
    pub const fn new(from: Color, to: Color) -> Self {
        Self { from, to }
    }

    /// The colour at `t`, where `0` is [`Self::from`] and `1` is [`Self::to`].
    pub fn at(&self, t: f32) -> Color {
        blend(self.from, self.to, t)
    }

    /// The text, one span per character, coloured along the ramp.
    ///
    /// Per character rather than per byte or per cell: this is what keeps a two-cell character from
    /// being coloured on half of itself.
    pub fn spans(&self, text: &str) -> Vec<Span<'static>> {
        let characters: Vec<char> = text.chars().collect();
        let last = characters.len().saturating_sub(1);
        characters
            .into_iter()
            .enumerate()
            .map(|(index, character)| {
                let t = if last == 0 {
                    0.0
                } else {
                    index as f32 / last as f32
                };
                Span::styled(character.to_string(), Style::default().fg(self.at(t)))
            })
            .collect()
    }
}

/// Mix two colours. `t` is clamped, so a caller may pass anything.
pub fn blend(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        // Nothing sensible interpolates towards or away from `Reset`: it is the terminal's own
        // colour, and there is no number between it and an RGB triple.
        (Color::Reset, _) | (_, Color::Reset) => {
            if t < 0.5 {
                from
            } else {
                to
            }
        }
        // Palette indices are positions in a table, not points on a line: index 4 is not half of
        // anything. Snap, and let the caller pick colours that are actually near each other.
        _ => {
            if t < 0.5 {
                from
            } else {
                to
            }
        }
    }
}

/// A highlight that travels along the text and wraps.
///
/// `phase` is where the band is, in `0.0..1.0`, so the caller animates by handing in a fraction of
/// elapsed time and this stays a pure function. The band covers a quarter of the text, at least one
/// character: a highlight narrower than a character is invisible, and one that moves a whole
/// character at a time only looks animated when the text is long enough.
pub fn shimmer(text: &str, phase: f32, base: Style, highlight: Style) -> Vec<Span<'static>> {
    let characters: Vec<char> = text.chars().collect();
    let count = characters.len();
    if count == 0 {
        return Vec::new();
    }

    let width = (count as f32 / 4.0).ceil().max(1.0);
    let centre = phase.rem_euclid(1.0) * count as f32;
    // Distance from the band's centre, wrapping around the ends so the highlight does not appear to
    // stop at either edge.
    let distance = |index: usize| {
        let raw = (index as f32 - centre).abs();
        raw.min(count as f32 - raw)
    };

    characters
        .into_iter()
        .enumerate()
        .map(|(index, character)| {
            let falloff = (1.0 - distance(index) / width).clamp(0.0, 1.0);
            let style = if falloff <= 0.0 {
                base
            } else {
                // The band's own base is the text's style; the highlight is what it brightens to,
                // so a dim label shimmers without ever looking like a different kind of label.
                let fg = match (base.fg, highlight.fg) {
                    (Some(from), Some(to)) => Some(blend(from, to, falloff)),
                    (None, Some(to)) => Some(blend(to, to, falloff)),
                    (from, None) => from,
                };
                Style {
                    fg,
                    ..base.add_modifier(highlight.add_modifier)
                }
            };
            Span::styled(character.to_string(), style)
        })
        .collect()
}

/// The frame a spinner should show at `tick`.
///
/// Takes the frames rather than owning them, because the glyph set is the theme's business: an
/// ASCII terminal gets `|/-\` and a Unicode one gets braille.
pub fn spinner_frame(frames: &[&'static str], tick: u64) -> &'static str {
    if frames.is_empty() {
        return "";
    }
    frames[(tick % frames.len() as u64) as usize]
}

/// A fraction that rises then falls, for a pulse. `tick` is a frame counter, `period` its length.
pub fn pulse(tick: u64, period: u64) -> f32 {
    if period == 0 {
        return 1.0;
    }
    let step = (tick % period) as f32 / period as f32;
    // A triangle rather than a sine: no trigonometry for something that is only ever a few dozen
    // frames long, and the eye cannot tell them apart at this scale.
    if step < 0.5 {
        step * 2.0
    } else {
        (1.0 - step) * 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(r, g, b)
    }

    #[test]
    fn a_ramp_starts_and_ends_where_it_says_and_bends_in_between() {
        let ramp = Ramp::new(rgb(0, 0, 0), rgb(255, 255, 255));
        assert_eq!(ramp.at(0.0), rgb(0, 0, 0));
        assert_eq!(ramp.at(1.0), rgb(255, 255, 255));
        assert_eq!(ramp.at(0.5), rgb(128, 128, 128));
        // Clamped, not wrapping: a caller that overshoots gets the end, not the start.
        assert_eq!(ramp.at(-1.0), rgb(0, 0, 0));
        assert_eq!(ramp.at(2.0), rgb(255, 255, 255));
    }

    #[test]
    fn a_ramp_over_text_colours_one_character_at_a_time() {
        let ramp = Ramp::new(rgb(0, 0, 0), rgb(255, 255, 255));
        let spans = ramp.spans("abcd");
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].style.fg, Some(rgb(0, 0, 0)));
        assert_eq!(spans[3].style.fg, Some(rgb(255, 255, 255)));
        assert_eq!(spans[1].style.fg, Some(rgb(85, 85, 85)));

        // One character is a whole ramp: no division by zero, and it takes the near end.
        assert_eq!(ramp.spans("x")[0].style.fg, Some(rgb(0, 0, 0)));
        assert!(ramp.spans("").is_empty());

        // Wide characters stay one character: a ramp never colours half of one.
        let spans = ramp.spans("你好");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "你");
    }

    #[test]
    fn a_ramp_between_palette_colours_snaps_instead_of_inventing_one() {
        // Index 4 is a position in the terminal's table, not a point on a line between 1 and 7:
        // blending them would ask the terminal for a colour it was never told about.
        assert_eq!(
            blend(Color::Indexed(1), Color::Indexed(7), 0.2),
            Color::Indexed(1)
        );
        assert_eq!(
            blend(Color::Indexed(1), Color::Indexed(7), 0.8),
            Color::Indexed(7)
        );
        assert_eq!(blend(Color::Reset, rgb(255, 0, 0), 0.9), rgb(255, 0, 0));
        assert_eq!(blend(rgb(255, 0, 0), Color::Reset, 0.1), rgb(255, 0, 0));
    }

    #[test]
    fn the_highlight_moves_across_the_text_and_wraps_round() {
        let base = Style::default().fg(rgb(0, 0, 0));
        let highlight = Style::default().fg(rgb(255, 255, 255));
        let text = "0123456789abcdef";

        let bright = |phase: f32| {
            shimmer(text, phase, base, highlight)
                .iter()
                .position(|span| span.style.fg == Some(rgb(255, 255, 255)))
        };

        assert_eq!(bright(0.0), Some(0), "the band starts at the beginning");
        assert_eq!(bright(0.5), Some(8), "and is halfway along at a half");
        // Past the end it comes back round rather than sticking to the last character.
        assert_eq!(bright(1.0), Some(0));
        assert_eq!(bright(1.5), Some(8));

        // Every character is present, in order, whatever the phase.
        let spans = shimmer(text, 0.37, base, highlight);
        assert_eq!(spans.len(), text.chars().count());
        assert_eq!(
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn a_highlight_on_short_text_is_still_visible() {
        for text in ["a", "ab", "abc"] {
            let spans = shimmer(
                text,
                0.5,
                Style::default().fg(rgb(0, 0, 0)),
                Style::default().fg(rgb(255, 255, 255)),
            );
            assert_eq!(spans.len(), text.chars().count());
            // Short text is where the band is widest relative to the text, so the brightest span
            // is a blend rather than the pure highlight: that it is brighter than the base at all
            // is the promise.
            let brightest = spans
                .iter()
                .filter_map(|span| span.style.fg)
                .max_by_key(|colour| match colour {
                    Color::Rgb(r, _, _) => *r,
                    _ => 0,
                });
            assert!(
                brightest.is_some_and(|colour| colour != rgb(0, 0, 0)),
                "{text:?}: the band vanished"
            );
        }
        assert!(shimmer("", 0.0, Style::default(), Style::default()).is_empty());
    }

    #[test]
    fn a_spinner_cycles_and_survives_an_empty_set() {
        let frames = ["|", "/", "-", "\\"];
        assert_eq!(spinner_frame(&frames, 0), "|");
        assert_eq!(spinner_frame(&frames, 3), "\\");
        assert_eq!(spinner_frame(&frames, 4), "|", "it wraps");
        assert_eq!(spinner_frame(&frames, 4_000_000_001), "/");
        assert_eq!(spinner_frame(&[], 7), "");
    }

    #[test]
    fn a_pulse_rises_and_falls_within_its_period() {
        assert_eq!(pulse(0, 8), 0.0);
        assert_eq!(pulse(2, 8), 0.5);
        assert_eq!(pulse(4, 8), 1.0);
        assert_eq!(pulse(6, 8), 0.5);
        assert_eq!(pulse(8, 8), 0.0, "and starts again");
        assert_eq!(pulse(3, 0), 1.0, "a zero period is not a division by zero");
    }
}
