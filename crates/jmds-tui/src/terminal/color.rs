//! Colour maths: the palette tables, and what a colour works out to.
//!
//! Two callers share it: the theme layer, which down-samples a colour for the terminal's
//! palette and asks what the result still means, and the background probe, which classifies
//! what the terminal reported.

use ratatui::style::Color;

use super::background::Background;

/// Nearest xterm-256 index for an RGB color (`16..=255`).
///
/// Grays use the dedicated 24-step ramp — the color cube's gray diagonal is coarse
/// enough to be visible in borders and dim text.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        return match r {
            0..=7 => 16,
            248..=255 => 231,
            _ => 232 + ((u16::from(r) - 8) * 24 / 247) as u8,
        };
    }
    let level = |v: u8| (u16::from(v) * 5 + 127) / 255;
    16 + (36 * level(r) + 6 * level(g) + level(b)) as u8
}

/// The sixteen ANSI colors, in palette order. Terminals disagree about the exact values; these
/// are the ones [`rgb_to_16`] maps to, so the two directions agree with each other.
pub(crate) const ANSI_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// The xterm 256-color palette's value for an index: the sixteen ANSI colors, then a 6×6×6
/// cube, then a 24-step grey ramp with a 10-per-step gap from 8.
///
/// The inverse of [`rgb_to_256`] in the direction a classifier needs: given the index a theme
/// was down-sampled to, what colour is the screen actually showing.
pub(crate) fn palette_rgb(index: u8) -> (u8, u8, u8) {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => ANSI_16[usize::from(index)],
        16..=231 => {
            let i = u16::from(index) - 16;
            (
                CUBE[(i / 36) as usize],
                CUBE[((i % 36) / 6) as usize],
                CUBE[(i % 6) as usize],
            )
        }
        // 232..=255: the grey ramp.
        _ => {
            let level = 8 + 10 * (u16::from(index) - 232);
            (level as u8, level as u8, level as u8)
        }
    }
}

/// What a colour says about the screen it is drawn on, when it says anything.
///
/// `Rgb` is exact. A palette index is looked up in the table it comes from — a theme that was
/// down-sampled for a 256-color terminal is still a light theme, and this is what keeps it from
/// being classified as a dark one. The sixteen names are the ANSI colors they stand for.
/// `Reset` is whatever the terminal already had on screen, so it has no answer of its own; the
/// caller's fallback is then the same assumption [`Background::default`] makes.
pub(crate) fn color_luminance(color: Color) -> Option<f64> {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(index) => palette_rgb(index),
        Color::Black => ANSI_16[0],
        Color::Red => ANSI_16[1],
        Color::Green => ANSI_16[2],
        Color::Yellow => ANSI_16[3],
        Color::Blue => ANSI_16[4],
        Color::Magenta => ANSI_16[5],
        Color::Cyan => ANSI_16[6],
        Color::Gray => ANSI_16[7],
        Color::DarkGray => ANSI_16[8],
        Color::LightRed => ANSI_16[9],
        Color::LightGreen => ANSI_16[10],
        Color::LightYellow => ANSI_16[11],
        Color::LightBlue => ANSI_16[12],
        Color::LightMagenta => ANSI_16[13],
        Color::LightCyan => ANSI_16[14],
        Color::White => ANSI_16[15],
        Color::Reset => return None,
    };
    Some(rgb_luminance(
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    ))
}

/// Nearest index into the 16 ANSI colors, for terminals without a 256-color palette.
pub fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let mut best = 0u8;
    let mut best_distance = u32::MAX;
    for (index, (pr, pg, pb)) in ANSI_16.iter().enumerate() {
        let distance = i32::from(r).abs_diff(i32::from(*pr)).pow(2)
            + i32::from(g).abs_diff(i32::from(*pg)).pow(2)
            + i32::from(b).abs_diff(i32::from(*pb)).pow(2);
        if distance < best_distance {
            best_distance = distance;
            best = index as u8;
        }
    }
    best
}

/// WCAG relative luminance of an sRGB colour with channels in `0.0..=1.0`.
///
/// One implementation for the two ways a background arrives — the terminal's OSC 11 answer
/// and, on Windows, the console's colour table.
pub(crate) fn rgb_luminance(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// What a luminance says about the background.
pub(crate) fn background_from_luminance(luminance: f64) -> Background {
    if luminance > 0.5 {
        Background::Light
    } else {
        Background::Dark
    }
}
