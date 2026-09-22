//! The conversation: what was asked, what came back, and where the next question is typed.
//!
//! Three decisions worth naming, because each one is a way chat panes go wrong:
//!
//! - **Deltas append to the entry they belong to.** `reasoning_content` and visible content are
//!   separate entries from the first delta, not one buffer switched on a flag: a reasoning model
//!   interleaves them, and the answer must not end up inside the thinking. A turn ending *closes*
//!   the open entries, so the next delta starts a new one instead of appending to the last turn's.
//! - **The view follows the bottom until the user says otherwise.** New output scrolls into view
//!   only while the view is already at the bottom. Scrolling up is a request to read, and yanking
//!   the viewport away from what someone is reading is the rudest thing a chat can do. `Ctrl+End`
//!   asks to follow again.
//! - **Long reasoning is folded, not hidden.** A thinking entry over three lines shows its first
//!   two and says how many it is holding back, so the model's reasoning is one keystroke away
//!   rather than invisible.
//!
//! What leaves the pane is an *outbox*, not a channel: the app drains it and hands it to the
//! engine. A channel would make the pane's behaviour depend on when its reader happens to run,
//! which is exactly what makes this untestable; a queue makes it a function of the keys pressed.

use std::collections::{HashMap, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jmds_core::{event::AgentEvent, pane::PaneKind};
use ratatui::layout::{Position, Rect};
use ratatui::{
    buffer::Buffer,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{KeyOutcome, Pane};
use crate::{effects, theme::Theme};

/// How many lines of a long entry are shown before it is folded.
const FOLD_AFTER: usize = 3;
/// How many of those are kept when folding.
const FOLD_KEEP: usize = 2;

/// One thing in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// What the human asked.
    User(String),
    /// The model's visible answer, grown by deltas.
    Assistant(String),
    /// `reasoning_content`, kept apart from the answer.
    Thinking(String),
    /// A tool the model asked for.
    ToolCall { name: String, arguments: String },
    /// What that tool said.
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
    /// The turn failed.
    Error(String),
}

/// The conversation pane.
pub struct Chat {
    entries: Vec<Entry>,
    /// Where an arriving visible delta appends, and where a thinking one does. Kept apart because
    /// a reasoning model sends both in one turn.
    open_content: Option<usize>,
    open_thinking: Option<usize>,
    /// Tool names by call id, so a result can say which tool it came from.
    tool_names: HashMap<String, String>,
    input: String,
    /// Caret position, in characters — not bytes, because the cursor is between characters.
    caret: usize,
    history: Vec<String>,
    /// Where in the history a recalled line came from, if the input is one.
    recall: Option<usize>,
    /// Rows scrolled past the top of the transcript. A request can run past the end, and the draw
    /// pass is what knows the real bound, so it is clamped there rather than here.
    scroll: usize,
    /// Whether the view is pinned to the newest output.
    follow: bool,
    /// Whether a turn is in flight, which is what puts the working line on screen.
    running: bool,
    /// A frame counter for the working line's animation. Nothing here owns a clock: whoever does
    /// calls [`Chat::tick`], so a frame is still a function of the state and the tick.
    tick: u64,
    outbox: VecDeque<String>,
}

impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}

impl Chat {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            open_content: None,
            open_thinking: None,
            tool_names: HashMap::new(),
            input: String::new(),
            caret: 0,
            history: Vec::new(),
            recall: None,
            scroll: 0,
            follow: true,
            running: false,
            tick: 0,
            outbox: VecDeque::new(),
        }
    }

    /// The transcript, for whoever draws or stores it.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    /// Take what the human has asked to send, oldest first.
    fn take_outbox(&mut self) -> Vec<String> {
        self.outbox.drain(..).collect()
    }

    /// A frame passed. Whoever owns the clock says so; nothing here polls one.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Whether a turn is in flight.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// A turn is over: the next delta starts a new entry rather than growing the last one.
    fn close_entries(&mut self) {
        self.open_content = None;
        self.open_thinking = None;
    }

    fn push_delta(&mut self, thinking: bool, delta: &str) {
        let open = if thinking {
            &mut self.open_thinking
        } else {
            &mut self.open_content
        };
        let index = match *open {
            Some(index) => index,
            None => {
                let entry = if thinking {
                    Entry::Thinking(String::new())
                } else {
                    Entry::Assistant(String::new())
                };
                self.entries.push(entry);
                let index = self.entries.len() - 1;
                *open = Some(index);
                index
            }
        };
        match &mut self.entries[index] {
            Entry::Thinking(text) | Entry::Assistant(text) => text.push_str(delta),
            _ => {}
        }
    }

    fn push_input_char(&mut self, character: char) {
        let at = byte_index(&self.input, self.caret);
        self.input.insert(at, character);
        self.caret += 1;
        self.recall = None;
    }

    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let at = byte_index(&self.input, self.caret - 1);
        self.input.remove(at);
        self.caret -= 1;
        self.recall = None;
    }

    fn delete(&mut self) {
        if self.caret >= self.input.chars().count() {
            return;
        }
        let at = byte_index(&self.input, self.caret);
        self.input.remove(at);
        self.recall = None;
    }

    fn move_caret(&mut self, to: usize) {
        self.caret = to.min(self.input.chars().count());
    }

    /// Send what is in the input, if anything.
    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.recall = None;
        self.entries.push(Entry::User(text.clone()));
        self.outbox.push_back(text);
        self.input.clear();
        self.caret = 0;
        self.follow = true;
    }

    /// Walk the history. `back` is what `Up` does.
    fn walk_history(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.recall, back) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(at), true) => Some(at - 1),
            (Some(at), false) if at + 1 >= self.history.len() => None,
            (Some(at), false) => Some(at + 1),
        };
        self.recall = next;
        self.input = match next {
            Some(at) => self.history[at].clone(),
            // Walking past the newest line gives the empty line back, which is what a shell does
            // and what a half-typed question that was never sent would want.
            None => String::new(),
        };
        self.caret = self.input.chars().count();
    }

    /// Move the view. The bounds are applied when drawing, where the wrapped row count is known;
    /// reaching the bottom there is what resumes following.
    fn scroll_by(&mut self, delta: usize, up: bool) {
        self.scroll = if up {
            self.scroll.saturating_sub(delta)
        } else {
            self.scroll.saturating_add(delta)
        };
        self.follow = false;
    }

    /// The transcript as rows that fit `width` cells.
    ///
    /// Wrapping is done here rather than by the widget because the count of rows *is* the scroll
    /// arithmetic: measuring with one implementation and drawing with another is how a chat ends
    /// up unable to reach its own last line.
    fn wrapped(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let width = width as usize;
        self.lines(theme)
            .into_iter()
            .flat_map(|line| wrap(line, width))
            .collect()
    }

    /// The transcript as lines, folded where it is long.
    ///
    /// Every marker and every colour comes from the theme: a pane that hard-codes `›` or a bold
    /// attribute is a pane that does not change when the theme does.
    fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let styles = theme.styles();
        let glyphs = theme.glyphs;
        let mut lines = Vec::new();

        for entry in &self.entries {
            let (prefix, style, body): (&str, Style, String) = match entry {
                Entry::User(text) => (glyphs.prompt, styles.user, text.clone()),
                Entry::Assistant(text) => ("", styles.assistant, text.clone()),
                Entry::Thinking(text) => (glyphs.thinking, styles.thinking, text.clone()),
                Entry::ToolCall { name, arguments } => (
                    glyphs.tool,
                    styles.tool,
                    format!("{name} {}", one_line(arguments)),
                ),
                Entry::ToolResult { name, ok, summary } => {
                    let style = if *ok { styles.tool } else { styles.tool_failed };
                    let body = if *ok {
                        format!("{name}: {summary}")
                    } else {
                        format!("{name}: failed: {summary}")
                    };
                    (glyphs.tool, style, body)
                }
                Entry::Error(text) => (glyphs.failure, styles.error, text.clone()),
            };

            let body_lines: Vec<&str> = body.split('\n').collect();
            // Only reasoning is folded. Folding the answer would hide the thing the human asked
            // for; folding reasoning is what keeps a thinking model's transcript readable.
            let folded = matches!(entry, Entry::Thinking(_)) && body_lines.len() > FOLD_AFTER;
            let shown = if folded { FOLD_KEEP } else { body_lines.len() };
            for (index, line) in body_lines.iter().take(shown).enumerate() {
                let prefix = if index == 0 { prefix } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), style),
                    Span::styled((*line).to_string(), style),
                ]));
            }
            if folded {
                lines.push(Line::from(Span::styled(
                    format!("{} {} more lines", glyphs.folded, body_lines.len() - shown),
                    styles.dim,
                )));
            }
        }

        if self.running {
            lines.push(self.working_line(theme));
        }
        lines
    }

    /// What is happening right now: a spinner, and a highlight travelling along the label.
    ///
    /// A tool that is still running is named, because "running read" and "thinking" are different
    /// waits and a user watching needs to know which one they are in.
    fn working_line(&self, theme: &Theme) -> Line<'static> {
        let styles = theme.styles();
        let label = match self.pending_tool() {
            Some(name) => format!("running {name}"),
            None => "thinking".to_string(),
        };
        let frame = effects::spinner_frame(theme.glyphs.spinner, self.tick);
        // A full sweep every twenty-four frames, whatever the frame rate is: the phase is a
        // fraction of the way through, which is exactly what a frame counter can supply.
        let phase = (self.tick % 24) as f32 / 24.0;

        let mut spans = vec![Span::styled(format!("{frame} "), styles.accent)];
        spans.extend(effects::shimmer(&label, phase, styles.dim, styles.accent));
        Line::from(spans)
    }

    /// A tool call the model asked for that has not come back yet.
    fn pending_tool(&self) -> Option<&str> {
        let mut open: Option<&str> = None;
        for entry in &self.entries {
            match entry {
                Entry::ToolCall { name, .. } => open = Some(name),
                Entry::ToolResult { .. } => open = None,
                _ => {}
            }
        }
        open
    }

    /// The input line, and where in it the window starts.
    ///
    /// Measured in cells rather than characters: a Chinese prompt is two cells per character, and a
    /// window that counted characters would push the caret off the right edge of the row.
    fn input_window(&self, width: u16, prompt: &str) -> InputWindow {
        let chars: Vec<char> = self.input.chars().collect();
        let caret = self.caret.min(chars.len());
        let available = width.saturating_sub(prompt.width() as u16) as usize;
        let caret_cells: usize = chars[..caret].iter().map(|c| c.width().unwrap_or(0)).sum();

        let mut skipped = 0;
        let mut dropped_cells = 0;
        while caret_cells - dropped_cells > available && skipped < caret {
            dropped_cells += chars[skipped].width().unwrap_or(0);
            skipped += 1;
        }

        InputWindow {
            text: chars[skipped..].iter().collect(),
            caret_cells: (caret_cells - dropped_cells) as u16,
        }
    }
}

/// The visible part of the input line: what is shown, and where the caret sits inside it.
struct InputWindow {
    text: String,
    caret_cells: u16,
}

/// The character index as a byte index.
fn byte_index(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

/// Break one styled line into rows of at most `width` cells.
///
/// Cells, not characters: a row holding Chinese text fits half as many characters as one holding
/// ASCII, and splitting by character count would overflow the pane on every second line.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut used = 0usize;

    for span in line.spans {
        let style = span.style;
        for character in span.content.chars() {
            let cells = character.width().unwrap_or(0);
            // A row breaks before the character that would not fit, never after it.
            if used + cells > width && used > 0 {
                if !text.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut text), style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            text.push(character);
            used += cells;
        }
        if !text.is_empty() {
            current.push(Span::styled(std::mem::take(&mut text), style));
        }
    }

    rows.push(Line::from(current));
    rows
}

/// Tool arguments, on one line: a call card is a line in a transcript, not a JSON dump.
fn one_line(arguments: &str) -> String {
    let joined = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 100 {
        let cut: String = joined.chars().take(100).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

impl Pane for Chat {
    fn kind(&self) -> PaneKind {
        PaneKind::Chat
    }

    fn title(&self) -> &str {
        "chat"
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // The last row is the input; the rest is the transcript. The two must not overlap: an
        // input row left at the top of the area would draw its prompt over the first transcript row.
        let input_row = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        let transcript = Rect::new(area.x, area.y, area.width, area.height - 1);

        // Wrapped once, by this pane, so the row count used for scrolling is the row count drawn.
        let rows = self.wrapped(transcript.width, theme);
        let bottom = rows.len().saturating_sub(transcript.height as usize);

        // The clamp lives here because only a draw knows the width the wrapping used, and reaching
        // the bottom is what resumes following.
        self.scroll = if self.follow {
            bottom
        } else {
            self.scroll.min(bottom)
        };
        self.follow = self.scroll >= bottom;

        let styles = theme.styles();
        let visible: Vec<Line<'static>> = rows.into_iter().skip(self.scroll).collect();
        Paragraph::new(Text::from(visible))
            .style(styles.text)
            .render(transcript, buf);

        let prompt = theme.glyphs.prompt;
        buf.set_string(area.x, input_row.y, prompt, styles.prompt);
        let window = self.input_window(area.width, prompt);
        buf.set_string(
            area.x + prompt.width() as u16,
            input_row.y,
            &window.text,
            styles.text,
        );
    }

    /// The lines the human has sent, for the app to hand to the engine.
    fn take_requests(&mut self) -> Vec<String> {
        self.take_outbox()
    }

    fn cursor(&self, area: Rect) -> Option<Position> {
        if area.height == 0 {
            return None;
        }
        // The caret is placed from the theme's prompt too: a glyph that is two cells wide would
        // otherwise put the cursor a cell off from the text.
        let prompt = crate::theme::Theme::default().glyphs.prompt;
        let window = self.input_window(area.width, prompt);
        let column = area.x + prompt.width() as u16 + window.caret_cells;
        Some(Position::new(
            column.min(area.right().saturating_sub(1)),
            area.y + area.height - 1,
        ))
    }

    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let caret = self.caret;
        let end = self.input.chars().count();
        let _ = alt;

        match key.code {
            KeyCode::Enter => self.send(),
            KeyCode::Char('u') if control => {
                let at = byte_index(&self.input, caret);
                self.input.drain(..at);
                self.caret = 0;
            }
            KeyCode::Char('a') if control => self.move_caret(0),
            KeyCode::Char('e') if control => self.move_caret(end),
            KeyCode::Char('k') if control => {
                let at = byte_index(&self.input, caret);
                self.input.truncate(at);
            }
            // Scrolling asks to read. `Ctrl+Home` and `Ctrl+End` are the ends of the transcript,
            // and the bottom is where following resumes; the plain keys belong to the caret.
            KeyCode::PageUp => self.scroll_by(10, true),
            KeyCode::PageDown => self.scroll_by(10, false),
            KeyCode::Home if control => {
                self.scroll = 0;
                self.follow = false;
            }
            KeyCode::End if control => self.follow = true,
            KeyCode::Up => self.walk_history(true),
            KeyCode::Down => self.walk_history(false),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if control => self.move_caret(0),
            KeyCode::Left => self.move_caret(caret.saturating_sub(1)),
            KeyCode::Right if control => self.move_caret(end),
            KeyCode::Right => self.move_caret(caret + 1),
            KeyCode::Home => self.move_caret(0),
            KeyCode::End => self.move_caret(end),
            KeyCode::Char(character) if !control && !alt => self.push_input_char(character),
            _ => return KeyOutcome::Ignored,
        }
        KeyOutcome::Handled
    }

    fn on_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted { .. } => {
                self.close_entries();
                self.running = true;
            }
            AgentEvent::Content(delta) => self.push_delta(false, delta),
            AgentEvent::Thinking(delta) => self.push_delta(true, delta),
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.close_entries();
                self.tool_names.insert(id.clone(), name.clone());
                self.entries.push(Entry::ToolCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            AgentEvent::ToolResult { id, ok, summary } => {
                self.close_entries();
                let name = self
                    .tool_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                self.entries.push(Entry::ToolResult {
                    name,
                    ok: *ok,
                    summary: summary.clone(),
                });
            }
            AgentEvent::TurnFinished { .. } => {
                self.close_entries();
                self.running = false;
            }
            AgentEvent::Error(text) => {
                self.close_entries();
                self.running = false;
                self.entries.push(Entry::Error(text.clone()));
            }
            AgentEvent::Usage(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn typed(chat: &mut Chat, text: &str) {
        for character in text.chars() {
            chat.on_key(key(KeyCode::Char(character)));
        }
    }

    fn area() -> Rect {
        Rect::new(0, 0, 40, 10)
    }

    fn screen(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|row| {
                (0..buf.area.width)
                    .map(|column| buf[(column, row)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn drawn_lines(chat: &mut Chat) -> Vec<String> {
        let mut buf = Buffer::empty(area());
        chat.draw(area(), &mut buf, &Theme::default());
        screen(&buf).lines().map(str::to_string).collect()
    }

    #[test]
    fn a_question_is_sent_once_and_appears_in_the_transcript() {
        let mut chat = Chat::new();
        typed(&mut chat, "what is 2+2?");
        assert_eq!(chat.on_key(key(KeyCode::Enter)), KeyOutcome::Handled);

        assert_eq!(chat.take_outbox(), vec!["what is 2+2?".to_string()]);
        assert_eq!(chat.input(), "");
        assert_eq!(chat.entries(), &[Entry::User("what is 2+2?".into())]);
        assert!(chat.take_outbox().is_empty(), "drained once");
    }

    #[test]
    fn an_empty_input_is_not_sent() {
        let mut chat = Chat::new();
        chat.on_key(key(KeyCode::Enter));
        typed(&mut chat, "   ");
        chat.on_key(key(KeyCode::Enter));
        assert!(chat.take_outbox().is_empty());
        assert!(chat.entries().is_empty(), "whitespace is not a question");
    }

    #[test]
    fn the_caret_edits_where_it_is_not_where_the_text_ends() {
        let mut chat = Chat::new();
        typed(&mut chat, "helo");
        chat.on_key(key(KeyCode::Left));
        typed(&mut chat, "l");
        assert_eq!(chat.input(), "hello");

        chat.on_key(key(KeyCode::Home));
        typed(&mut chat, ">");
        assert_eq!(chat.input(), ">hello");

        chat.on_key(key(KeyCode::End));
        chat.on_key(key(KeyCode::Backspace));
        assert_eq!(chat.input(), ">hell");

        chat.on_key(key(KeyCode::Home));
        chat.on_key(key(KeyCode::Delete));
        assert_eq!(chat.input(), "hell");
    }

    #[test]
    fn walking_the_history_gives_the_empty_line_back() {
        let mut chat = Chat::new();
        for question in ["one", "two"] {
            typed(&mut chat, question);
            chat.on_key(key(KeyCode::Enter));
        }

        chat.on_key(key(KeyCode::Up));
        assert_eq!(chat.input(), "two");
        chat.on_key(key(KeyCode::Up));
        assert_eq!(chat.input(), "one");
        chat.on_key(key(KeyCode::Up));
        assert_eq!(chat.input(), "one", "the oldest line is the end of it");
        chat.on_key(key(KeyCode::Down));
        assert_eq!(chat.input(), "two");
        chat.on_key(key(KeyCode::Down));
        assert_eq!(
            chat.input(),
            "",
            "past the newest is the line being written"
        );
    }

    #[test]
    fn a_recalled_line_can_be_edited_and_sent() {
        let mut chat = Chat::new();
        typed(&mut chat, "cargo test");
        chat.on_key(key(KeyCode::Enter));
        chat.on_key(key(KeyCode::Up));
        typed(&mut chat, " --lib");
        chat.on_key(key(KeyCode::Enter));
        assert_eq!(chat.take_outbox(), vec!["cargo test", "cargo test --lib"]);
    }

    #[test]
    fn content_deltas_grow_one_entry_and_thinking_never_lands_inside_it() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::TurnStarted {
            model: "deepseek-chat".into(),
        });
        chat.on_agent_event(&AgentEvent::Thinking("let me ".into()));
        chat.on_agent_event(&AgentEvent::Thinking("think".into()));
        chat.on_agent_event(&AgentEvent::Content("The answer ".into()));
        chat.on_agent_event(&AgentEvent::Content("is 4.".into()));

        assert_eq!(
            chat.entries(),
            &[
                Entry::Thinking("let me think".into()),
                Entry::Assistant("The answer is 4.".into()),
            ]
        );
    }

    #[test]
    fn a_finished_turn_closes_its_entries_so_the_next_delta_starts_a_new_one() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::Content("first".into()));
        chat.on_agent_event(&AgentEvent::TurnFinished {
            reason: jmds_core::event::FinishReason::Stop,
        });
        chat.on_agent_event(&AgentEvent::Content("second".into()));

        assert_eq!(
            chat.entries(),
            &[
                Entry::Assistant("first".into()),
                Entry::Assistant("second".into())
            ],
            "the second turn must not be appended to the first"
        );
    }

    #[test]
    fn a_tool_result_says_which_tool_it_came_from() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{\n  \"path\": \"src/main.rs\"\n}".into(),
        });
        chat.on_agent_event(&AgentEvent::ToolResult {
            id: "call_1".into(),
            ok: true,
            summary: "42 lines".into(),
        });

        assert_eq!(
            chat.entries(),
            &[
                Entry::ToolCall {
                    name: "read".into(),
                    arguments: "{\n  \"path\": \"src/main.rs\"\n}".into()
                },
                Entry::ToolResult {
                    name: "read".into(),
                    ok: true,
                    summary: "42 lines".into()
                },
            ]
        );
        let screen = drawn_lines(&mut chat).join("\n");
        assert!(
            screen.contains("::read { \"path\": \"src/main.rs\" }"),
            "{screen}"
        );
        assert!(screen.contains("::read: 42 lines"), "{screen}");
    }

    #[test]
    fn a_turn_that_failed_is_in_the_transcript() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::Error("upstream said no".into()));
        assert_eq!(chat.entries(), &[Entry::Error("upstream said no".into())]);
        let screen = drawn_lines(&mut chat).join("\n");
        assert!(screen.contains("!! upstream said no"), "{screen}");
    }

    #[test]
    fn long_reasoning_is_folded_and_says_how_much_it_holds_back() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::Thinking("one\ntwo\nthree\nfour\nfive".into()));
        let screen = drawn_lines(&mut chat).join("\n");
        assert!(screen.contains("one") && screen.contains("two"), "{screen}");
        assert!(!screen.contains("three"), "folded away: {screen}");
        assert!(screen.contains("… 3 more lines"), "{screen}");
    }

    #[test]
    fn the_view_follows_new_output_but_not_once_the_user_scrolls_up() {
        let mut chat = Chat::new();
        for index in 0..40 {
            chat.on_agent_event(&AgentEvent::Content(format!("line {index}\n")));
        }
        let lines = drawn_lines(&mut chat);
        assert!(
            lines.iter().any(|line| line.contains("line 39")),
            "the newest output is on screen: {lines:?}"
        );

        // Scrolling up is a request to read; new output must not drag the viewport away.
        chat.on_key(key(KeyCode::PageUp));
        let scrolled = drawn_lines(&mut chat);
        assert!(
            !scrolled.iter().any(|line| line.contains("line 39")),
            "scrolled up: {scrolled:?}"
        );
        chat.on_agent_event(&AgentEvent::Content("line 40\n".into()));
        let after = drawn_lines(&mut chat);
        assert_eq!(scrolled, after, "the reader was not yanked to the bottom");

        // `Ctrl+End` asks to follow again.
        chat.on_key(control(KeyCode::End));
        assert!(
            drawn_lines(&mut chat)
                .iter()
                .any(|line| line.contains("line 40")),
            "following again"
        );
    }

    #[test]
    fn the_caret_is_where_the_text_ends_including_wide_characters() {
        let mut chat = Chat::new();
        typed(&mut chat, "你好");
        // The prompt is two cells, each character two cells: the caret is at column six.
        assert_eq!(chat.cursor(area()), Some(Position::new(6, 9)));

        chat.on_key(key(KeyCode::Left));
        assert_eq!(chat.cursor(area()), Some(Position::new(4, 9)));
    }

    #[test]
    fn a_long_input_scrolls_so_the_caret_stays_on_screen() {
        let mut chat = Chat::new();
        typed(&mut chat, &"x".repeat(60));
        let mut buf = Buffer::empty(area());
        chat.draw(area(), &mut buf, &Theme::default());
        let last = screen(&buf).lines().last().unwrap().to_string();
        assert!(last.starts_with("› "), "{last}");
        assert!(last.trim_end().ends_with('x'), "{last}");

        let cursor = chat.cursor(area()).unwrap();
        assert!(
            cursor.x < area().width,
            "the caret is on screen: {cursor:?}"
        );
        assert_eq!(cursor.y, area().height - 1, "on the input row");
    }

    #[test]
    fn wrapping_counts_cells_so_wide_characters_do_not_overflow_the_pane() {
        let line = Line::from("你好世界一二三");
        let rows = wrap(line, 4);
        assert_eq!(rows.len(), 4, "two characters fit in four cells");
        assert_eq!(rows[0].spans[0].content, "你好");
        assert_eq!(rows[3].spans[0].content, "三");

        // A long ASCII answer wraps at the same budget.
        let rows = wrap(Line::from("abcdefghij"), 4);
        assert_eq!(
            rows.iter()
                .map(|row| row.spans[0].content.to_string())
                .collect::<Vec<_>>(),
            vec!["abcd", "efgh", "ij"]
        );

        // Zero width is a pane with nothing to draw into, not an infinite loop.
        assert!(wrap(Line::from("x"), 0).is_empty());
    }

    #[test]
    fn the_input_line_is_the_last_row_and_the_transcript_is_above_it() {
        let mut chat = Chat::new();
        chat.on_agent_event(&AgentEvent::Content("the answer".into()));
        let lines = drawn_lines(&mut chat);
        assert!(lines[0].contains("the answer"), "{lines:?}");
        assert!(
            lines[9].starts_with("› "),
            "the input sits at the bottom: {lines:?}"
        );
    }
}
