//! Escape-sequence emission: the bytes the app writes to change something about the
//! terminal, rather than to draw a cell.

use std::io::{self, Write};

use crossterm::{
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};

use super::capability::is_kitty_terminal;

/// Ask the terminal to raise a desktop notification.
///
/// Two forms, both out-of-band — they change no cells, so they cannot disturb the frame
/// being drawn around them:
///
/// * `OSC 99` — kitty's own, and the richer one. It carries a title and a body as separate
///   payloads, names the application (`f=`, which is what a user filters by), and takes the
///   payloads Base64-encoded (`e=1`). Base64 is why a title containing `;`, `BEL` or
///   `ESC` — a title is remote data — arrives intact instead of being stripped.
/// * `OSC 9` — the iTerm2 form, which every terminal with notifications implements (kitty,
///   WezTerm, foot, Windows Terminal among them). It carries one plain-text string, so
///   control characters are stripped and the length is capped; a terminal without
///   notifications drops the sequence, so nothing needs probing.
///
/// kitty is recognised from the environment, not from a query: this sequence has no reply,
/// so probing would mean waiting for a timeout on every start.
pub fn notify<W: Write>(out: &mut W, title: &str, body: &str) -> io::Result<()> {
    let kitty = is_kitty_terminal(&|key: &str| std::env::var(key).ok());
    notify_as(out, title, body, kitty)
}

/// The payload limit kitty documents is 2048 bytes before encoding. A title is far
/// shorter, but the text can come from the network, so it is clipped rather than trusted.
const OSC99_MAX_PAYLOAD: usize = 2048;

/// [`notify`] with the terminal's identity passed in instead of probed.
fn notify_as<W: Write>(out: &mut W, title: &str, body: &str, kitty: bool) -> io::Result<()> {
    if kitty {
        return notify_osc99(out, title, body);
    }

    let text = if title.is_empty() {
        body.to_string()
    } else {
        format!("{title} — {body}")
    };
    let clean: String = text.chars().filter(|c| !c.is_control()).take(240).collect();
    write!(out, "\x1b]9;{clean}\x07")?;
    out.flush()
}

fn notify_osc99<W: Write>(out: &mut W, title: &str, body: &str) -> io::Result<()> {
    use base64::Engine as _;

    let encode = |text: &str| {
        let mut used = 0usize;
        let clipped: String = text
            .chars()
            .take_while(|c| {
                used += c.len_utf8();
                used <= OSC99_MAX_PAYLOAD
            })
            .collect();
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(clipped.as_bytes())
    };

    // `i=` is the notification id, so a later notification replaces the previous one instead
    // of stacking up. `d=0` holds the notification back until the body arrives with `d=1`,
    // which is what makes title and body one notification rather than two.
    let app = base64::engine::general_purpose::STANDARD_NO_PAD.encode(b"jmds");
    const ID: &str = "jmds";

    if !title.is_empty() {
        write!(
            out,
            "\x1b]99;i={ID}:f={app}:e=1:d=0;{}\x1b\\",
            encode(title)
        )?;
    }
    write!(
        out,
        "\x1b]99;i={ID}:f={app}:p=body:e=1:d=1;{}\x1b\\",
        encode(body)
    )?;
    out.flush()
}

/// `CSI ? 2004 h` / `CSI ? 2004 l` — bracketed paste on and off.
const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";
/// `CSI > 1 u` pushes kitty keyboard flag 1; `CSI < 1 u` pops it again.
const PUSH_DISAMBIGUATE_ESCAPE_CODES: &[u8] = b"\x1b[>1u";
const POP_KEYBOARD_ENHANCEMENT_FLAGS: &[u8] = b"\x1b[<1u";

/// Put the terminal into the modes the UI relies on, and take them back out again.
///
/// The sequences are written directly rather than through `crossterm`'s command types,
/// for one reason that matters on Windows: there those commands go through the legacy
/// console API instead of writing bytes, and with no console attached they do not
/// degrade — `PushKeyboardEnhancementFlags` returns
/// "Keyboard progressive enhancement not implemented for the legacy Windows API". The
/// rest of the frame already talks to the terminal this way (`OSC 11`, `OSC 99`,
/// `DECSET 2026`), so this is the same mouth speaking, and the bytes are exactly what
/// the Unix implementation emitted before.
///
/// Neither sequence is probed for: a terminal that does not implement one ignores it,
/// so there is nothing to break.
///
/// * `CSI ? 2004 h` — bracketed paste, so a paste arrives as one delimited block
///   instead of a burst of keystrokes.
/// * `CSI > 1 u` — the kitty keyboard protocol, flag 1 (disambiguate escape codes):
///   "pressing the Esc key generates the byte 0x1b which also is used to indicate the
///   start of an escape code", which is how an `Esc` press gets read as `Alt+<key>`
///   when another key follows it. Kitty, ghostty, foot, wezterm, alacritty and iTerm2
///   implement it; a terminal that does not simply ignores the sequence and `Esc` keeps
///   the ambiguity it always had. Only that one flag is pushed: the rest are for
///   features jmds does not use.
pub fn enable_terminal_modes<W: Write>(out: &mut W) -> io::Result<()> {
    out.write_all(BRACKETED_PASTE_ON)?;
    out.write_all(PUSH_DISAMBIGUATE_ESCAPE_CODES)?;
    out.flush()
}

/// Undo [`enable_terminal_modes`]. Runs on the way out, including after a panic.
///
/// `CSI < 1 u` and not the defaulted `CSI < u`: the explicit form is the documented one,
/// and it has to be written while still on the alternate screen, since the main and
/// alternate screens keep separate stacks.
pub fn disable_terminal_modes<W: Write>(out: &mut W) -> io::Result<()> {
    out.write_all(BRACKETED_PASTE_OFF)?;
    out.write_all(POP_KEYBOARD_ENHANCEMENT_FLAGS)?;
    out.flush()
}

/// Open a synchronized update (`DECSET 2026`): the terminal buffers everything
/// drawn until [`end_synchronized_update`] and puts it on screen in one go.
///
/// `ratatui` does not do this itself, and without it a frame is visible while it is
/// being written — most obviously while the spectrum redraws many times a second.
/// Terminals without the mode ignore the sequence; kitty discards a frame whose end
/// never arrives, so every open must be closed.
pub fn begin_synchronized_update<W: Write>(out: &mut W) -> io::Result<()> {
    execute!(out, BeginSynchronizedUpdate)
}

/// Close the update opened by [`begin_synchronized_update`].
pub fn end_synchronized_update<W: Write>(out: &mut W) -> io::Result<()> {
    execute!(out, EndSynchronizedUpdate)
}

#[cfg(test)]
mod notification_tests {
    use super::{super::CursorStyle, *};

    /// `OSC 9` is the form the terminals implement, and the payload is remote data: a `BEL`
    /// or `ESC` in a title would otherwise end the sequence early or start another one.
    /// Title and body are joined with a dash because this form carries a single string.
    #[test]
    fn a_notification_is_one_clean_osc_9() {
        let mut out = Vec::new();
        notify_as(&mut out, "标题", "正文", false).expect("notify");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\u{1b}]9;标题 — 正文\u{7}"
        );
    }

    #[test]
    fn control_characters_cannot_escape_the_notification() {
        let mut out = Vec::new();
        notify_as(&mut out, "", "a\u{7}b\u{1b}]9;evil", false).expect("notify");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text, "\u{1b}]9;ab]9;evil\u{7}");
        assert_eq!(text.matches('\u{7}').count(), 1, "exactly one terminator");
    }

    /// kitty's own form keeps the halves apart, and Base64 is what lets a title containing
    /// `;` or `ESC` through instead of being filtered away.
    #[test]
    fn the_kitty_form_keeps_title_and_body_apart() {
        let mut out = Vec::new();
        notify_as(&mut out, "a;b\u{1b}c", "body text", true).expect("notify");
        let text = String::from_utf8(out).expect("utf8");

        let payloads = osc99_payloads(&text);
        assert_eq!(payloads.len(), 2, "a title payload and a body payload");
        assert_eq!(decode(&payloads[0]), "a;b\u{1b}c");
        assert_eq!(decode(&payloads[1]), "body text");

        assert!(text.contains("p=body"), "the body says what it is");
        assert!(text.contains("f="), "the application names itself");
        // `d=0` holds the notification back until `d=1` arrives: the other order is two
        // notifications rather than one.
        assert!(text.find("d=0").expect("d=0") < text.find("d=1").expect("d=1"));
    }

    /// A notification with nothing to put in the title is one sequence, not an empty first.
    #[test]
    fn a_body_only_notification_has_no_title_sequence() {
        let mut out = Vec::new();
        notify_as(&mut out, "", "失败了", true).expect("notify");
        let text = String::from_utf8(out).expect("utf8");

        assert_eq!(osc99_payloads(&text).len(), 1);
        assert_eq!(text.matches("\u{1b}]99;").count(), 1);
        assert!(!text.contains("d=0"));
    }

    /// The clip is counted in bytes — a CJK title is three bytes a character, so a character
    /// count would overshoot kitty's documented 2048 — and it must not split a character.
    #[test]
    fn the_payload_stops_at_kitty_s_byte_limit() {
        let long = "歌".repeat(2000);
        let mut out = Vec::new();
        notify_as(&mut out, &long, "body", true).expect("notify");
        let text = String::from_utf8(out).expect("utf8");

        let title = decode(&osc99_payloads(&text)[0]);
        assert!(title.len() <= OSC99_MAX_PAYLOAD, "{}", title.len());
        assert_eq!(title.chars().count(), 682, "2048 / 3, rounded down");
        assert!(
            title.chars().all(|c| c == '歌'),
            "no broken character at the cut"
        );
    }

    fn osc99_payloads(text: &str) -> Vec<String> {
        text.split("\u{1b}\\")
            .filter(|sequence| sequence.starts_with("\u{1b}]99;"))
            .map(|sequence| {
                sequence
                    .rsplit(';')
                    .next()
                    .expect("every sequence has a payload")
                    .to_string()
            })
            .collect()
    }

    fn decode(payload: &str) -> String {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(payload)
            .expect("base64 payload");
        String::from_utf8(bytes).expect("utf8 payload")
    }

    /// The config's choices have to land on distinct terminal shapes.
    #[test]
    fn cursor_styles_are_distinct() {
        let shapes: Vec<String> = [
            CursorStyle::Default,
            CursorStyle::Block,
            CursorStyle::Underline,
            CursorStyle::Bar,
        ]
        .iter()
        .map(|style| format!("{:?}", style.command()))
        .collect();

        let mut unique = shapes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), shapes.len(), "{shapes:?}");
    }
}
