//! The terminal's background: `COLORFGBG` when it is exported, the OSC 11 query otherwise.
//!
//! The query itself is platform-shaped — a tty on Unix, the console API on Windows — so each
//! half lives under its own `cfg`; what the two share, the reply parser and the luminance
//! threshold, is here.

use std::{
    env,
    io::{BufRead, Write},
    sync::LazyLock,
};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::probe_tty_background;
#[cfg(windows)]
use windows::probe_console_background;

use super::color::{background_from_luminance, rgb_luminance};

/// Terminal background, used to pick the light or dark theme slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Background {
    #[default]
    Dark,
    Light,
}

/// Config-facing setting: `auto` follows the terminal, the others force a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    #[default]
    Auto,
    Dark,
    Light,
}

impl BackgroundMode {
    /// Apply the setting to what the terminal reported.
    pub fn resolve(self, detected: Background) -> Background {
        match self {
            BackgroundMode::Auto => detected,
            BackgroundMode::Dark => Background::Dark,
            BackgroundMode::Light => Background::Light,
        }
    }
}

/// Whether the app paints its own background over the terminal's.
///
/// The app paints the whole frame with the theme's background so that a theme reads as a theme:
/// without it, every cell the theme does not explicitly paint keeps the terminal's colours, and
/// a light theme in a dark terminal becomes thin light-grey text on a dark screen. What that
/// fill also covers is the terminal's own background — a translucent one, Windows Terminal's
/// acrylic — which is a thing the user *can* see, unlike a fill that matches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundFill {
    /// Paint it only when the theme's background and the terminal's disagree — the case the fill
    /// exists for. When they agree the fill is invisible while the transparency it hides is not.
    #[default]
    Auto,
    /// Always paint it.
    Always,
    /// Never paint it: the terminal's background, and its blur, show through everywhere the
    /// theme does not deliberately paint a surface. On a terminal whose background does not
    /// match the theme, text is then as readable as the user's choice makes it.
    Never,
}

impl BackgroundFill {
    /// Whether the frame should be filled, given the theme's background and the terminal's.
    pub fn paints(self, theme: Background, terminal: Background) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => theme != terminal,
        }
    }
}

/// Parse `COLORFGBG` (`"<fg>;<bg>"`) into the background lightness.
///
/// The background entry is a 0-15 palette index; indices 8 and above are the bright
/// half, which is the light background.
pub fn parse_colorfgbg(value: &str) -> Option<Background> {
    let background = value.rsplit(';').next()?.trim();
    let index = background.parse::<u8>().ok()?;
    Some(if index >= 8 {
        Background::Light
    } else {
        Background::Dark
    })
}

/// Parse an OSC 11 answer (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL`) into relative luminance.
pub fn parse_osc11_luminance(reply: &str) -> Option<f64> {
    let start = reply.find("rgb:")? + "rgb:".len();
    let mut channels = reply[start..].split('/');
    let mut values = [0.0f64; 3];
    for value in &mut values {
        let channel = channels.next()?;
        let digits: String = channel
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if digits.is_empty() {
            return None;
        }
        let max = u32::from_str_radix(&"f".repeat(digits.len()), 16).ok()?;
        let parsed = u32::from_str_radix(&digits, 16).ok()?;
        *value = f64::from(parsed) / f64::from(max);
    }
    Some(rgb_luminance(values[0], values[1], values[2]))
}

/// Send the OSC 11 query and read the terminal's answer.
///
/// Generic over the streams so the parsing path is testable; in the running app the
/// caller passes the controlling terminal.
pub fn query_background_luminance<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> Option<f64> {
    write_osc11_query(writer)?;
    read_osc11_reply(reader).and_then(|reply| parse_osc11_luminance(&reply))
}

fn write_osc11_query<W: Write>(writer: &mut W) -> Option<()> {
    writer.write_all(b"\x1b]11;?\x1b\\").ok()?;
    writer.flush().ok()?;
    Some(())
}

/// Read up to the terminator of a reply (`BEL` or `ESC \`), bounded so a terminal that
/// never answers cannot make the caller spin.
fn read_osc11_reply<R: BufRead>(reader: &mut R) -> Option<String> {
    const MAX_REPLY: usize = 64;

    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while reply.len() < MAX_REPLY {
        if reader.read_exact(&mut byte).is_err() {
            return None;
        }
        match byte[0] {
            0x07 => return String::from_utf8(reply).ok(),
            0x1b => {
                let mut next = [0u8; 1];
                if reader.read_exact(&mut next).is_err() {
                    return None;
                }
                if next[0] == b'\\' {
                    return String::from_utf8(reply).ok();
                }
                reply.push(0x1b);
                reply.push(next[0]);
            }
            other => reply.push(other),
        }
    }
    None
}

/// Overall budget for reading the terminal's answer to the OSC 11 query.
#[cfg(unix)]
const REPLY_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Background of the current terminal, detected once.
///
/// `COLORFGBG` covers terminals that export it; otherwise the terminal is asked
/// directly (OSC 11). Callers that run before the UI takes over the terminal should
/// touch this eagerly so the query happens before the event loop starts reading input.
pub static BACKGROUND: LazyLock<Background> = LazyLock::new(detect_background);

fn detect_background() -> Background {
    if let Ok(value) = env::var("COLORFGBG")
        && let Some(background) = parse_colorfgbg(&value)
    {
        return background;
    }

    #[cfg(unix)]
    if let Some(luminance) = probe_tty_background() {
        return background_from_luminance(luminance);
    }

    #[cfg(windows)]
    if let Some(luminance) = probe_console_background() {
        return background_from_luminance(luminance);
    }

    Background::Dark
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::{
        super::{
            ANSI_16, ColorMode, background_from_luminance, color_luminance, color_mode_from,
            palette_rgb, rgb_to_16, rgb_to_256,
        },
        *,
    };

    #[cfg(target_os = "linux")]
    use super::unix::query_background_on_tty;
    #[cfg(windows)]
    use super::windows::console_color_luminance;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn color_mode_follows_the_environment() {
        assert_eq!(
            color_mode_from(env_from(&[("COLORTERM", "truecolor"), ("TERM", "xterm")])),
            ColorMode::TrueColor
        );
        assert_eq!(
            color_mode_from(env_from(&[("COLORTERM", "24bit"), ("TERM", "screen")])),
            ColorMode::TrueColor
        );
        // Windows Terminal only advertises itself
        assert_eq!(
            color_mode_from(env_from(&[("WT_SESSION", "1"), ("TERM", "xterm")])),
            ColorMode::TrueColor
        );
        assert_eq!(
            color_mode_from(env_from(&[("TERM", "xterm-256color")])),
            ColorMode::Ansi256
        );
        assert_eq!(
            color_mode_from(env_from(&[("TERM", "linux")])),
            ColorMode::Basic
        );
        assert_eq!(
            color_mode_from(env_from(&[("TERM", "dumb")])),
            ColorMode::Basic
        );
        // an ssh session that lost COLORTERM
        assert_eq!(
            color_mode_from(env_from(&[("TERM", "xterm"), ("TERM_PROGRAM", "kitty")])),
            ColorMode::TrueColor
        );
        // unknown legacy terminal: assume the modern default, never the 16-color one
        assert_eq!(
            color_mode_from(env_from(&[("TERM", "vt100")])),
            ColorMode::Ansi256
        );
    }

    #[test]
    fn rgb_to_256_matches_the_xterm_palette() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 255, 0), 46);
        assert_eq!(rgb_to_256(0, 0, 255), 21);
        // grays take the ramp, not the cube
        assert!(rgb_to_256(128, 128, 128) >= 232);
    }

    /// Down-sampling a colour for a palette terminal must not change what it *means*: this is
    /// what keeps a light theme reading as light in a 256-colour terminal (CI caught the
    /// absence of it, where the terminal reports no true-colour support).
    #[test]
    fn a_palette_colour_still_says_what_the_screen_shows() {
        assert_eq!(palette_rgb(16), (0, 0, 0), "the cube's corner");
        assert_eq!(palette_rgb(231), (255, 255, 255), "the cube's other corner");
        assert_eq!(palette_rgb(255), (238, 238, 238), "the grey ramp's end");
        assert_eq!(palette_rgb(15), ANSI_16[15], "the ANSI colours come first");

        let classify =
            |color| background_from_luminance(color_luminance(color).expect("a measurable colour"));
        assert_eq!(classify(Color::Indexed(231)), Background::Light);
        assert_eq!(classify(Color::Indexed(232)), Background::Dark);
        assert_eq!(classify(Color::White), Background::Light);
        assert_eq!(color_luminance(Color::Reset), None);

        // The contract that matters: whatever `rgb_to_256` picks for a colour classifies the
        // way the colour itself does.
        for (r, g, b) in [(255, 255, 255), (250, 250, 250), (0, 0, 0), (20, 20, 20)] {
            assert_eq!(
                classify(Color::Indexed(rgb_to_256(r, g, b))),
                classify(Color::Rgb(r, g, b)),
                "({r},{g},{b}) must survive the trip through the palette"
            );
        }
    }

    #[test]
    fn rgb_to_16_picks_the_nearest_ansi_color() {
        assert_eq!(rgb_to_16(0, 0, 0), 0);
        assert_eq!(rgb_to_16(255, 255, 255), 15);
        assert_eq!(rgb_to_16(255, 0, 0), 9);
        assert_eq!(rgb_to_16(8, 8, 8), 0);
        assert_eq!(rgb_to_16(250, 250, 250), 15);
    }

    #[test]
    fn colorfgbg_selects_the_background() {
        assert_eq!(parse_colorfgbg("15;0"), Some(Background::Dark));
        assert_eq!(parse_colorfgbg("0;15"), Some(Background::Light));
        assert_eq!(parse_colorfgbg("15;8"), Some(Background::Light));
        assert_eq!(parse_colorfgbg("15;default"), None);
        assert_eq!(parse_colorfgbg(""), None);
        assert_eq!(parse_colorfgbg("garbage"), None);
    }

    #[test]
    fn background_thresholds_at_half_luminance() {
        assert_eq!(background_from_luminance(0.0), Background::Dark);
        assert_eq!(background_from_luminance(0.5), Background::Dark);
        assert_eq!(background_from_luminance(0.51), Background::Light);
        assert_eq!(background_from_luminance(1.0), Background::Light);
    }

    /// `COLORREF` packs `0x00BBGGRR`, so the byte order has to be read backwards: blue is the
    /// high byte and the darkest of the three primaries by weight, which is what makes the
    /// usual `#282c34`-style background come out dark instead of bright red.
    #[cfg(windows)]
    #[test]
    fn a_console_colour_is_read_as_bgr() {
        assert!(console_color_luminance(0x0000_0000) < 0.01, "black");
        assert!(console_color_luminance(0x00FF_FFFF) > 0.99, "white");
        let blue = console_color_luminance(0x00FF_0000);
        let green = console_color_luminance(0x0000_FF00);
        let red = console_color_luminance(0x0000_00FF);
        assert!(blue < red && red < green, "{blue} {red} {green}");
        assert_eq!(background_from_luminance(blue), Background::Dark);
    }

    #[test]
    fn osc11_replies_map_to_luminance() {
        let dark = parse_osc11_luminance("\x1b]11;rgb:1e1e/1e1e/1e1e\x07").unwrap();
        assert!(dark < 0.2, "dark background luminance was {dark}");
        let light = parse_osc11_luminance("\x1b]11;rgb:ffff/ffff/ffff\x07").unwrap();
        assert!(light > 0.9, "light background luminance was {light}");
        // short form, and the ESC \ terminator
        assert_eq!(
            parse_osc11_luminance("\x1b]11;rgb:00/00/00\x1b\\"),
            Some(0.0)
        );
        assert_eq!(parse_osc11_luminance("\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_osc11_luminance("no reply"), None);
    }

    #[test]
    fn query_round_trip_reads_the_bel_terminated_reply() {
        let mut reply = std::io::Cursor::new(b"\x1b]11;rgb:ffff/ffff/ffff\x07".to_vec());
        let mut sent = Vec::new();
        let luminance = query_background_luminance(&mut reply, &mut sent).unwrap();
        assert_eq!(sent, b"\x1b]11;?\x1b\\");
        assert!(luminance > 0.9);
    }

    #[test]
    fn query_gives_up_on_a_terminal_that_never_answers() {
        let mut silent = std::io::Cursor::new(Vec::new());
        let mut sent = Vec::new();
        assert_eq!(query_background_luminance(&mut silent, &mut sent), None);
    }

    /// The reply to OSC 11 has no newline, so a tty in canonical mode withholds it: the
    /// poll always timed out and `BACKGROUND` fell back to "dark" on every terminal that
    /// does answer (kitty does not export `COLORFGBG`). The probe now reads the tty in the
    /// mode the event loop later uses, and this test is what pins that: a pty plays the
    /// terminal, the probe runs against the pty's other end.
    ///
    /// Linux only, deliberately. What the test asserts is a *kernel* behaviour — a
    /// canonical-mode read does not see a reply that carries no newline — and that is what
    /// was measured and reproduced here. The macOS pty layer is a different implementation
    /// of the same idea and does not exist on the machine this was written on; the CI macOS
    /// job is what first said so. The *code* under test is still compiled on every Unix: if
    /// this is ever run on a Mac, expect to adjust the pty setup, not the probe.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_background_probe_reads_a_reply_without_a_newline() {
        use std::os::fd::FromRawFd;

        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        // SAFETY: both out-parameters point to valid, writable `c_int`s; the optional
        // name/termios/winsize arguments are genuinely optional and passed as null.
        let created = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(
            created,
            0,
            "openpty failed: {}",
            std::io::Error::last_os_error()
        );

        let terminal = std::thread::spawn(move || {
            let mut query = [0u8; 64];
            // SAFETY: `query` is a valid, writable buffer of `query.len()` bytes.
            let read = unsafe { libc::read(master, query.as_mut_ptr().cast(), query.len()) };
            assert!(read > 0, "the probe sent no query");
            assert!(
                query[..read as usize].starts_with(b"\x1b]11;?"),
                "unexpected query: {:?}",
                &query[..read as usize]
            );

            let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
            // SAFETY: `reply` is a valid, readable buffer of `reply.len()` bytes.
            let written = unsafe { libc::write(master, reply.as_ptr().cast(), reply.len()) };
            assert_eq!(written, reply.len() as isize, "the reply was not delivered");
        });

        // SAFETY: `slave` came from `openpty` above and is not owned by anything else.
        let tty = unsafe { std::fs::File::from_raw_fd(slave) };
        let luminance = query_background_on_tty(&tty).expect("the white reply must be parsed");
        terminal.join().expect("the terminal thread");

        assert!(
            luminance > 0.9,
            "a white background reads as light: {luminance}"
        );
    }
}
