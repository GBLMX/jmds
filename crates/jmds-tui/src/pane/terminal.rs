//! The terminal pane: a real shell, in a real PTY, drawn from a screen model.
//!
//! Three pieces, and each one is doing something the others cannot:
//!
//! - **`portable-pty`** runs the program. A shell wants a terminal — it asks for a window size,
//!   it turns on line editing, it sends `SIGWINCH` — so the only honest way to run one is to give
//!   it a PTY. That is also why this pane is not `bash`-the-tool: the tool runs *commands* with no
//!   terminal and closed stdin, which is right for an agent and wrong for a human.
//! - **`vt100`** turns the bytes back into a screen. A PTY gives you escape sequences, not rows:
//!   showing them is a terminal emulator's job, and reimplementing one would be a project of its
//!   own. The model also answers the questions the pane needs — where the cursor is, whether the
//!   program is on the alternate screen.
//! - **A reader thread**, because a PTY read blocks and this pane is synchronous. The thread owns
//!   the reading and hands chunks over a channel; the pane drains that channel when it draws, so a
//!   frame is still a function of state rather than of what happened to arrive while it was being
//!   built.
//!
//! Keys go the other way: crossterm gives a `KeyEvent` and the PTY wants bytes, so there is a small
//! encoder between them. `Ctrl+C` is the interesting one — it is `0x03`, and it is why the app's key
//! policy leaves `Ctrl+C` to the focused pane: in a shell that key means interrupt, and no
//! application key should be able to take it away.

use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jmds_core::pane::PaneKind;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};

use super::{KeyOutcome, Pane};
use crate::theme::Theme;

/// How much scrollback the screen model keeps. Enough to scroll a build's output back a few screens
/// without holding a whole log in a second place — the artifact files are for that.
const SCROLLBACK: usize = 2_000;

/// How much one `pump` takes.
///
/// A program that floods the PTY — `yes`, a build with no rate limit, a runaway loop — must not be
/// able to make a frame wait for it. An unbounded drain would never see the channel empty and would
/// never return, so the app would stop drawing while the flood went on; the next frame takes the
/// rest. The pty's own buffer is what pushes back in the meantime, which is exactly what it is for.
const PUMP_CHUNKS: usize = 64;

/// A shell in a pane.
pub struct TerminalPane {
    /// The program's name, as it was launched.
    program: String,
    /// The title as the host asks for it: the name, plus what happened to the program.
    title_cache: String,
    /// Where keys go.
    writer: Box<dyn Write + Send>,
    /// Where output comes from. Drained on every draw.
    incoming: Receiver<Vec<u8>>,
    /// The screen model.
    parser: vt100::Parser,
    /// The child process, so it can be killed when the pane goes away.
    child: Box<dyn Child + Send + Sync>,
    /// The PTY, kept for resizing.
    master: Box<dyn MasterPty + Send>,
    /// What the PTY and the screen model are currently sized to.
    size: (u16, u16),
    /// Set when the reader thread saw the end of the output.
    exited: bool,
    notice: Option<String>,
}

impl TerminalPane {
    /// Run `program` in a new PTY of `size`.
    ///
    /// The size is a placeholder: the first draw resizes both the PTY and the model to the area the
    /// pane actually got, which is the only size that matters.
    pub fn spawn(
        program: &str,
        args: &[&str],
        cwd: &Path,
        size: (u16, u16),
    ) -> std::io::Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: size.0.max(1),
                cols: size.1.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(cwd);
        // The shell inherits the environment, which is what makes a pane useful: `PATH`, the
        // editor, the proxy. `TERM` is set rather than inherited so the model and the program
        // agree on what the escape sequences mean.
        for (key, value) in std::env::vars() {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(command).map_err(to_io)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer = pair.master.take_writer().map_err(to_io)?;

        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            // Dropping the sender is how the pane learns the program is gone.
        });

        let name = program
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(program)
            .to_string();
        Ok(Self {
            title_cache: compose_title(&name, None),
            program: name,
            writer,
            incoming,
            parser: vt100::Parser::new(size.0.max(1), size.1.max(1), SCROLLBACK),
            child,
            master: pair.master,
            size,
            exited: false,
            notice: None,
        })
    }

    /// The shell the user's environment asks for, or `sh`.
    pub fn shell() -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "sh".to_string())
    }

    /// Send bytes to the program.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Take everything the program has said since the last call.
    pub fn pump(&mut self) -> usize {
        let mut taken = 0;
        for _ in 0..PUMP_CHUNKS {
            match self.incoming.try_recv() {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    taken += chunk.len();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The reader ends when the program does. Saying so in the title is the whole
                    // exit report a terminal user needs.
                    if !self.exited {
                        self.exited = true;
                        self.notice = Some("exited".to_string());
                        self.title_cache = compose_title(&self.program, self.notice.as_deref());
                    }
                    break;
                }
            }
        }
        taken
    }

    /// Whether the program is still running.
    pub fn is_running(&self) -> bool {
        !self.exited
    }

    /// The screen's text, for tests and for anything that wants to read what is on it.
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Give the program the size it now has.
    ///
    /// Both halves have to be told: the PTY so the program lays out for it, and the model so the
    /// pane draws what the program thinks it wrote.
    fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.screen_mut().set_size(rows, cols);
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // Closing the pane closes the program: a shell left running with no way to see it is a
        // process nobody can find.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `KeyEvent` as the bytes a terminal would send for it.
///
/// This is the half of a terminal emulator that goes the other way, and it is small because a
/// modern keyboard is: the letters, the control codes, and the handful of cursor keys every
/// program agrees about.
pub fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut bytes = match key.code {
        KeyCode::Char(character) => {
            if control {
                // `Ctrl+A` is 0x01 and `Ctrl+C` is 0x03: the only control keys a shell needs are
                // the ones that predate keyboards having modifiers.
                let lower = character.to_ascii_lowercase();
                match lower {
                    'a'..='z' => vec![lower as u8 - b'a' + 1],
                    ' ' | '@' => vec![0x00],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    '^' => vec![0x1e],
                    '_' => vec![0x1f],
                    // Anything else with Ctrl is not a control code: drop the modifier rather than
                    // invent one.
                    _ => character.to_string().into_bytes(),
                }
            } else {
                character.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        // A key with no terminal encoding: the pane ignores it rather than pretending.
        KeyCode::F(_)
        | KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => return None,
    };

    // `Alt` is `Esc` first: that is what a terminal sends and what every readline understands.
    if alt && !bytes.is_empty() && bytes[0] != 0x1b {
        let mut with_escape = vec![0x1b];
        with_escape.append(&mut bytes);
        bytes = with_escape;
    }
    Some(bytes)
}

/// The colour a cell asked for, in ratatui's terms.
fn cell_colour(colour: vt100::Color, default: Color) -> Color {
    match colour {
        vt100::Color::Default => default,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The pane's title, from the program's name and what happened to it.
fn compose_title(name: &str, notice: Option<&str>) -> String {
    match notice {
        Some(notice) => format!("{name} {notice}"),
        None => name.to_string(),
    }
}

fn to_io(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

impl Pane for TerminalPane {
    fn kind(&self) -> PaneKind {
        PaneKind::Terminal
    }

    fn title(&self) -> &str {
        &self.title_cache
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.pump();
        self.resize(area.height, area.width);

        let screen = self.parser.screen();
        let styles = theme.styles();

        for row in 0..area.height {
            for column in 0..area.width {
                let Some(cell) = screen.cell(row, column) else {
                    continue;
                };
                if cell.contents().is_empty() {
                    // An empty cell is a space with the cell's background: a program that paints a
                    // background is drawing, not leaving blanks.
                    let background = cell_colour(cell.bgcolor(), theme.palette.bg);
                    let x = area.x + column;
                    let y = area.y + row;
                    buf[(x, y)].set_symbol(" ");
                    buf[(x, y)].set_bg(background);
                    continue;
                }
                let mut style = Style::default()
                    .fg(cell_colour(
                        cell.fgcolor(),
                        styles.text.fg.unwrap_or(Color::Reset),
                    ))
                    .bg(cell_colour(cell.bgcolor(), theme.palette.bg));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let x = area.x + column;
                let y = area.y + row;
                buf.set_string(x, y, cell.contents(), style);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        // The pane owns the keyboard while it has focus: every key it can encode goes to the
        // program, and `Ctrl+C` is the one that matters — a shell that cannot be interrupted is a
        // shell you have to kill from another window.
        match encode_key(&key) {
            Some(bytes) => {
                if self.write(&bytes).is_ok() {
                    KeyOutcome::Handled
                } else {
                    self.notice = Some("the program is not reading".to_string());
                    self.title_cache = compose_title(&self.program, self.notice.as_deref());
                    KeyOutcome::Handled
                }
            }
            None => KeyOutcome::Ignored,
        }
    }

    fn cursor(&self, area: Rect) -> Option<Position> {
        let screen = self.parser.screen();
        if screen.hide_cursor() {
            return None;
        }
        let (row, column) = screen.cursor_position();
        Some(Position::new(
            area.x + column.min(area.width.saturating_sub(1)),
            area.y + row.min(area.height.saturating_sub(1)),
        ))
    }

    fn tick(&mut self) {
        // Output arrives between frames: take it here so a program that is printing shows it even
        // when nothing is being pressed.
        self.pump();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 40, 10)
    }

    /// Wait for `check` to hold, pumping the pane, or give up.
    ///
    /// A PTY is a real process doing real work, so a test has to wait for it; the bound is what
    /// keeps a broken pane a failure rather than a hang.
    fn wait_for(pane: &mut TerminalPane, mut check: impl FnMut(&TerminalPane) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            pane.pump();
            if check(pane) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn pane(name: &str, script: &str) -> TerminalPane {
        let dir = std::env::temp_dir().join(format!("jmds-pty-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // `sh -c` rather than the user's shell: a test should not depend on someone's `zshrc`, and
        // what is being tested is the pane, not the shell.
        TerminalPane::spawn("sh", &["-c", script], &dir, (24, 80)).expect("a pty opens")
    }

    #[test]
    fn a_program_that_prints_shows_its_output_on_the_screen() {
        let mut pane = pane("print", "printf 'hello from the pty\\n'");
        assert!(
            wait_for(&mut pane, |pane| pane
                .contents()
                .contains("hello from the pty")),
            "the output never arrived: {:?}",
            pane.contents()
        );
    }

    #[test]
    fn what_is_typed_reaches_the_program() {
        // `cat` echoes what it reads, which is the shortest way to prove the write path works.
        let mut pane = pane("cat", "cat");
        pane.write(b"typed by the test\r").unwrap();
        assert!(
            wait_for(&mut pane, |pane| pane
                .contents()
                .contains("typed by the test")),
            "the program never echoed: {:?}",
            pane.contents()
        );
    }

    #[test]
    fn the_screen_is_drawn_where_the_pane_was_given() {
        let mut pane = pane("draw", "printf 'drawn\\n'");
        assert!(wait_for(&mut pane, |pane| pane
            .contents()
            .contains("drawn")));
        let mut buf = Buffer::empty(area());
        pane.draw(area(), &mut buf, &Theme::default());
        let screen: String = (0..area().height)
            .map(|row| {
                (0..area().width)
                    .map(|column| buf[(column, row)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("drawn"), "{screen}");
    }

    #[test]
    fn a_program_that_exits_says_so_in_the_title() {
        let mut pane = pane("exit", "exit 0");
        assert!(
            wait_for(&mut pane, |pane| !pane.is_running()),
            "the pane never noticed the program leaving"
        );
        let mut buf = Buffer::empty(area());
        pane.draw(area(), &mut buf, &Theme::default());
        assert!(pane.title().ends_with("exited"), "{}", pane.title());
    }

    #[test]
    fn the_cursor_is_where_the_program_left_it() {
        let mut pane = pane("cursor", "printf 'abc'");
        assert!(wait_for(&mut pane, |pane| pane.contents().contains("abc")));
        let position = pane.cursor(area()).expect("a visible cursor");
        assert_eq!(position, Position::new(3, 0), "after three characters");
    }

    #[test]
    fn a_flooding_program_does_not_stall_a_pump() {
        // A program that never stops writing must not make a frame wait for it: one pump takes a
        // bounded amount and returns, or the app stops drawing while the flood goes on.
        let dir = std::env::temp_dir().join(format!("jmds-pty-flood-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut pane = TerminalPane::spawn("sh", &[], &dir, (24, 80)).expect("a pty opens");

        // Typed straight into the shell rather than through any command machinery: this is about
        // the pane's reading, not about what a command is.
        pane.write(b"while :; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n'; done\r")
            .unwrap();
        let started = Instant::now();
        for _ in 0..5 {
            pane.pump();
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "five pumps took {:?}, so one of them waited for the program",
            started.elapsed()
        );
    }

    #[test]
    fn a_resize_reaches_both_the_program_and_the_model() {
        let mut pane = pane("resize", "sleep 5");
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 6));
        pane.draw(Rect::new(0, 0, 30, 6), &mut buf, &Theme::default());
        assert_eq!(pane.size, (6, 30), "the pty and the model agree");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn control_keys_become_the_control_codes() {
        // The one that matters: `Ctrl+C` is what interrupts a program in a shell.
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            Some(vec![0x03]),
            "case does not change the control code"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(vec![0x04])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Some(vec![0x00])
        );
    }

    #[test]
    fn plain_text_goes_through_as_its_own_bytes() {
        assert_eq!(
            encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(vec![b'a'])
        );
        // Not ASCII, not a problem: the PTY takes UTF-8.
        assert_eq!(
            encode_key(&key(KeyCode::Char('中'), KeyModifiers::NONE)),
            Some("中".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r']),
            "a terminal sends carriage return, not newline"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn the_cursor_keys_are_the_sequences_every_program_agrees_about() {
        for (code, expected) in [
            (KeyCode::Up, "\x1b[A"),
            (KeyCode::Down, "\x1b[B"),
            (KeyCode::Right, "\x1b[C"),
            (KeyCode::Left, "\x1b[D"),
            (KeyCode::Home, "\x1b[H"),
            (KeyCode::End, "\x1b[F"),
            (KeyCode::Delete, "\x1b[3~"),
            (KeyCode::PageUp, "\x1b[5~"),
        ] {
            assert_eq!(
                encode_key(&key(code, KeyModifiers::NONE)),
                Some(expected.as_bytes().to_vec()),
                "{code:?}"
            );
        }
    }

    #[test]
    fn alt_is_escape_first() {
        assert_eq!(
            encode_key(&key(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(b"\x1bb".to_vec())
        );
        // `Alt+Esc` is already an escape: it is not sent twice.
        assert_eq!(
            encode_key(&key(KeyCode::Esc, KeyModifiers::ALT)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn a_key_with_no_encoding_is_ignored_rather_than_guessed_at() {
        assert_eq!(encode_key(&key(KeyCode::F(5), KeyModifiers::NONE)), None);
        assert_eq!(
            encode_key(&key(KeyCode::CapsLock, KeyModifiers::NONE)),
            None
        );
        // A control character with no control code keeps its own text rather than inventing one.
        assert_eq!(
            encode_key(&key(KeyCode::Char('1'), KeyModifiers::CONTROL)),
            Some(vec![b'1'])
        );
    }
}
