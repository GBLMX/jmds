//! A pane showing a terminal — the screen half of one.
//!
//! **The engine owns the process; this owns the screen.** The pane does not open a pty, does not
//! spawn anything, and cannot kill anything: it is handed bytes ([`PtyEvent::Output`]) and it hands
//! bytes back ([`PtyEvent::Input`]), and everything about the process's life — when one starts,
//! what it runs, who stops it, whether it is reused — belongs to whoever started it. Two earlier
//! designs put the allocation and the reaping on this side and did not converge, because a pane's
//! lifetime is a layout decision (it is closed when a split is rearranged) and a process's lifetime
//! is not.
//!
//! What is left here is the part that is genuinely a view: a `vt100` screen model, the drawing of
//! it, the cursor, and the encoding of a key into the bytes a terminal would send.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jmds_core::event::PtyEvent;
use jmds_core::pane::{PaneId, PaneKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use super::{KeyOutcome, Pane};
use crate::theme::Theme;

/// How much scrollback the screen model keeps. Enough to scroll a build's output back a few screens
/// without holding a whole log in a second place — the artifact files are for that.
const SCROLLBACK: usize = 2_000;

/// How many chunks of output one frame takes.
///
/// A program that floods the pty — `yes`, a build with no rate limit, a runaway loop — must not be
/// able to make a frame wait for it. The queue is drained a bounded amount at a time and the rest
/// waits for the next frame, so the app keeps drawing at the tick it promised while a flood goes on.
const CHUNKS_PER_FRAME: usize = 64;

/// A terminal's screen in a pane.
pub struct TerminalPane {
    /// Which pane this screen belongs to. Pty events are routed by id, so a pane can tell what is
    /// meant for it — and, more to the point, can tell what is not.
    id: PaneId,
    /// The name of what is running, as it was started.
    program: String,
    /// The title as the host asks for it: the name, plus what happened to the program.
    title_cache: String,
    /// The screen model.
    parser: vt100::Parser,
    /// What the screen model is currently sized to.
    size: (u16, u16),
    /// Output that has arrived and has not been shown yet.
    pending: VecDeque<Vec<u8>>,
    /// Keys that have been pressed and have not been handed to the engine yet. The pane has no
    /// process to write to, so this is where a key waits for the app to publish it.
    pending_input: VecDeque<PtyEvent>,
    /// Set when the engine said the command ended.
    exited: bool,
    /// Whether the engine has not been told the new size yet.
    resized: bool,
    notice: Option<String>,
}

impl TerminalPane {
    /// A screen for `id`, showing a terminal of `size`.
    ///
    /// The size is a placeholder: the first draw reports what the pane actually got, and the engine
    /// resizes the pty to it. Until then there is nothing to be right about.
    pub fn new(id: PaneId, title: impl Into<String>, size: (u16, u16)) -> Self {
        let (rows, cols) = (size.0.max(1), size.1.max(1));
        let program = title.into();
        Self {
            title_cache: compose_title(&program, None),
            program,
            id,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            size: (rows, cols),
            pending: VecDeque::new(),
            pending_input: VecDeque::new(),
            exited: false,
            resized: false,
            notice: None,
        }
    }

    /// Which pane this screen belongs to.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// The screen's text, for tests and for anything that wants to read what is on it.
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Whether the command is still running, as far as the engine has said.
    pub fn is_running(&self) -> bool {
        !self.exited
    }

    /// Take everything the engine has said to this screen.
    pub fn on_pty_event(&mut self, event: &PtyEvent) {
        match event {
            PtyEvent::Started { id, title } if *id == self.id => {
                self.program = title.clone();
                self.title_cache = compose_title(&self.program, self.notice.as_deref());
            }
            // Queued rather than processed here: `process` is the expensive part, and doing it in
            // the loop that receives the event would let a flood of output hold up a frame.
            PtyEvent::Output { id, bytes } if *id == self.id => {
                self.pending.push_back(bytes.clone())
            }
            PtyEvent::Exited { id, code } if *id == self.id => {
                self.exited = true;
                self.notice = Some(match code {
                    Some(0) => "exited".to_string(),
                    Some(code) => format!("exited {code}"),
                    // Killed rather than exited: a signal is not an exit status, and inventing one
                    // would be a number nobody saw.
                    None => "killed".to_string(),
                });
                self.title_cache = compose_title(&self.program, self.notice.as_deref());
            }
            // Everything else — another pane's output, and the events that only go the other way —
            // belongs to someone else.
            _ => {}
        }
    }

    /// Show what has arrived, a bounded amount at a time.
    pub fn drain_pending(&mut self) -> usize {
        let mut shown = 0;
        for _ in 0..CHUNKS_PER_FRAME {
            match self.pending.pop_front() {
                Some(chunk) => {
                    shown += chunk.len();
                    self.parser.process(&chunk);
                }
                None => break,
            }
        }
        shown
    }

    /// The size the pane has now, and whether the engine still needs telling about it.
    fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        // Only the screen model is resized here. The pty is the engine's, so the pane says what it
        // now is and lets the engine decide to act on it.
        self.parser.screen_mut().set_size(rows, cols);
        self.resized = true;
    }
}

impl Pane for TerminalPane {
    fn kind(&self) -> PaneKind {
        PaneKind::Terminal
    }

    fn title(&self) -> &str {
        &self.title_cache
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.drain_pending();
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
        // program. It is put in the pane's own outbox rather than written straight to a process,
        // because there is no process here to write to.
        match encode_key(&key) {
            Some(bytes) => {
                self.pending_input
                    .push_back(PtyEvent::Input { id: self.id, bytes });
                KeyOutcome::Handled
            }
            None => KeyOutcome::Ignored,
        }
    }

    fn take_pty(&mut self) -> Vec<PtyEvent> {
        let mut events: Vec<PtyEvent> = self.pending_input.drain(..).collect();
        if self.resized {
            self.resized = false;
            events.push(PtyEvent::Resize {
                id: self.id,
                rows: self.size.0,
                cols: self.size.1,
            });
        }
        events
    }

    fn on_pty_event(&mut self, event: &PtyEvent) {
        TerminalPane::on_pty_event(self, event);
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
        // Output arrives between frames: taking it here is what makes a program that is printing
        // show it even when nothing is being pressed.
        self.drain_pending();
    }

    fn wants_frame(&self) -> bool {
        // Output that has arrived but has not been shown yet is this pane's own backlog: it drains a
        // bounded amount per frame, so a flood needs the next frame to keep up.
        !self.pending.is_empty()
    }

    /// The wheel scrolls the scrollback, the way a terminal does: output keeps arriving and the view
    /// stays where the reader put it.
    fn on_scroll(&mut self, steps: isize, _height: u16) {
        let screen = self.parser.screen_mut();
        let wanted = screen.scrollback() as isize + steps;
        screen.set_scrollback(wanted.clamp(0, SCROLLBACK as isize) as usize);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(id: u64, title: &str) -> TerminalPane {
        TerminalPane::new(PaneId::new(id), title, (10, 40))
    }

    fn area() -> Rect {
        Rect::new(0, 0, 40, 10)
    }

    fn screen(pane: &mut TerminalPane) -> String {
        let mut buffer = Buffer::empty(area());
        pane.draw(area(), &mut buffer, &Theme::default());
        (0..area().height)
            .map(|row| {
                (0..area().width)
                    .map(|column| buffer[(column, row)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn output(pane: &mut TerminalPane, bytes: &str) {
        pane.on_pty_event(&PtyEvent::Output {
            id: pane.id(),
            bytes: bytes.as_bytes().to_vec(),
        });
    }

    #[test]
    fn output_from_the_engine_shows_on_the_screen() {
        let mut pane = terminal(1, "sh");
        output(&mut pane, "hello from the pty\n");
        assert!(
            screen(&mut pane).contains("hello from the pty"),
            "{}",
            pane.contents()
        );
    }

    #[test]
    fn a_flood_is_taken_a_bounded_amount_per_frame() {
        let mut pane = terminal(1, "yes");
        for index in 0..(CHUNKS_PER_FRAME * 2) {
            output(&mut pane, &format!("line {index}\n"));
        }
        // One frame takes its share, not the lot: a program that never stops printing must not be
        // able to make a frame wait for it.
        let taken = pane.drain_pending();
        assert!(taken > 0);
        assert_eq!(
            pane.pending.len(),
            CHUNKS_PER_FRAME,
            "一帧只取自己那份，剩下的等下一帧"
        );
        // And the next frame takes the rest.
        assert!(pane.drain_pending() > 0);
        assert!(pane.pending.is_empty());
    }

    #[test]
    fn the_engine_says_when_the_command_ends() {
        let mut pane = terminal(1, "sh");
        assert!(pane.is_running());
        pane.on_pty_event(&PtyEvent::Exited {
            id: pane.id(),
            code: Some(0),
        });
        assert!(!pane.is_running());
        assert!(pane.title().ends_with("exited"), "{}", pane.title());

        // A killed command is not an exit status: saying "exited 137" would be a number nobody saw.
        let mut pane = terminal(1, "sh");
        pane.on_pty_event(&PtyEvent::Exited {
            id: pane.id(),
            code: None,
        });
        assert!(pane.title().ends_with("killed"), "{}", pane.title());
    }

    #[test]
    fn queued_output_keeps_asking_for_frames() {
        let mut pane = terminal(1, "sh");
        assert!(!pane.wants_frame(), "没东西要显示");
        output(&mut pane, "printed\n");
        assert!(pane.wants_frame(), "还有没画出来的输出");
        pane.drain_pending();
        assert!(!pane.wants_frame());
    }

    #[test]
    fn a_started_event_names_what_is_running() {
        let mut pane = terminal(1, "sh");
        pane.on_pty_event(&PtyEvent::Started {
            id: pane.id(),
            title: "cargo test".to_string(),
        });
        assert_eq!(pane.title(), "cargo test");
    }

    #[test]
    fn events_for_another_pane_are_ignored() {
        let mut pane = terminal(1, "sh");
        pane.on_pty_event(&PtyEvent::Output {
            id: PaneId::new(2),
            bytes: b"not mine".to_vec(),
        });
        pane.on_pty_event(&PtyEvent::Exited {
            id: PaneId::new(2),
            code: Some(1),
        });
        assert!(pane.is_running(), "别人的进程死活与这个面板无关");
        assert!(!screen(&mut pane).contains("not mine"));
    }

    #[test]
    fn what_is_typed_leaves_the_pane_as_an_input_event() {
        let mut pane = terminal(7, "sh");
        assert_eq!(
            pane.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            KeyOutcome::Handled
        );
        assert_eq!(
            pane.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyOutcome::Handled
        );
        let events = pane.take_pty();
        assert_eq!(
            events,
            vec![
                PtyEvent::Input {
                    id: PaneId::new(7),
                    bytes: vec![b'a']
                },
                PtyEvent::Input {
                    id: PaneId::new(7),
                    bytes: vec![0x03]
                },
            ]
        );
        assert!(pane.take_pty().is_empty(), "交出去的就是交出去了");
    }

    #[test]
    fn a_key_with_no_encoding_is_left_alone() {
        let mut pane = terminal(1, "sh");
        assert_eq!(
            pane.on_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            KeyOutcome::Ignored
        );
        assert!(pane.take_pty().is_empty());
    }

    #[test]
    fn a_resize_is_reported_once_per_change() {
        let mut pane = terminal(3, "sh");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 6));
        pane.draw(Rect::new(0, 0, 30, 6), &mut buffer, &Theme::default());
        assert_eq!(
            pane.take_pty(),
            vec![PtyEvent::Resize {
                id: PaneId::new(3),
                rows: 6,
                cols: 30
            }]
        );
        // Drawing again at the same size says nothing: the engine was already told.
        pane.draw(Rect::new(0, 0, 30, 6), &mut buffer, &Theme::default());
        assert!(pane.take_pty().is_empty());
    }

    #[test]
    fn the_cursor_is_where_the_program_left_it() {
        let mut pane = terminal(1, "sh");
        output(&mut pane, "abc");
        pane.drain_pending();
        assert_eq!(
            pane.cursor(area()).expect("一个看得见的游标"),
            Position::new(3, 0),
            "三个字符之后"
        );
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
