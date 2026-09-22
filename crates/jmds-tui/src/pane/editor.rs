//! The editor pane: the prompt file, in a vim.
//!
//! The other half of [`jmds_core::prompt`]. That module decides what a prompt file *is*; this one is
//! where it gets written, which is why the pane is a vim and not a form: the thing being edited is
//! prose with two header lines, and the hand already knows how to edit prose.
//!
//! Four decisions worth naming:
//!
//! - **edtui's own keybindings, not a keymap of ours.** Vim's editing keys are the reason to use an
//!   editor widget at all; reimplementing a subset would give a vim with holes in it. The app's own
//!   keys are handled before the pane ever sees a key (see `app.rs`), which is what makes that safe:
//!   every key this pane gets is one worth giving to an editor.
//! - **A missing file opens empty, and says so in the title.** Writing a prompt is the thing this
//!   app is for, so the first run has to be able to open a file that does not exist yet rather than
//!   refusing until someone makes one by hand.
//! - **Saving is synchronous, and that is on purpose.** The session file is written with `tokio::fs`
//!   because it is written on every turn; this is written when a human presses a key, and it is a
//!   few kilobytes. Blocking the loop for a millisecond is cheaper than a task and a channel here.
//! - **Sending is a request like the chat pane's.** `Ctrl+G` assembles the prompt and puts it in the
//!   same outbox the chat pane uses, so there is one path from a pane to the engine rather than two.

use std::{io, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::{EditorEventHandler, EditorState, EditorTheme, EditorView, Lines};
use jmds_core::{pane::PaneKind, prompt::Prompt};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    widgets::Widget,
};

use super::{KeyOutcome, Pane};
use crate::theme::Theme;

/// The editing surface.
///
/// Holds the file, the editor's own state, and the text as it was last written so "dirty" is a
/// comparison rather than a flag somebody has to remember to set.
pub struct Editor {
    path: PathBuf,
    /// What relative paths in the prompt resolve against: the session's working directory.
    base: PathBuf,
    state: EditorState,
    handler: EditorEventHandler,
    saved: String,
    /// One line for the title: what just happened, or nothing.
    notice: Option<String>,
    /// The title as the host will ask for it. Kept here because `Pane::title` borrows and the
    /// title is composed from three things that change.
    title_cache: String,
    /// What `Ctrl+G` put in the outbox, drained by the app.
    outbox: Vec<String>,
    /// Spaces a tab shows as. From the config, because the prompt files are read by a model as much
    /// as by a person.
    tab_width: usize,
}

impl Editor {
    /// Open `path`, or start an empty buffer if it is not there.
    ///
    /// Reading is `std::fs` for the same reason saving is: it happens once, when a pane opens.
    pub fn open(path: impl Into<PathBuf>, base: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut editor = Self {
            base: base.into(),
            state: EditorState::new(Lines::from(text.as_str())),
            handler: EditorEventHandler::default(),
            saved: text,
            notice: None,
            title_cache: String::new(),
            outbox: Vec::new(),
            tab_width: 4,
            path,
        };
        editor.title_cache = editor.compose_title();
        Ok(editor)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// How wide a tab is, from the config.
    pub fn with_tab_width(mut self, tab_width: usize) -> Self {
        self.tab_width = tab_width.max(1);
        self
    }

    /// The buffer, as text.
    pub fn text(&self) -> String {
        self.state.lines.to_string()
    }

    /// Whether the buffer differs from what was last read or written.
    pub fn is_dirty(&self) -> bool {
        self.text() != self.saved
    }

    /// What just happened, for the title.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Write the buffer.
    pub fn save(&mut self) -> io::Result<()> {
        let text = self.text();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, &text)?;
        self.saved = text;
        self.notice = Some("saved".to_string());
        self.title_cache = self.compose_title();
        Ok(())
    }

    /// The prompt this buffer stands for, assembled for sending.
    ///
    /// A parse error is reported rather than thrown away: an unclosed header is the one mistake this
    /// file can make, and the message says what to do about it.
    fn assembled(&mut self) -> Option<String> {
        match Prompt::parse(&self.text()) {
            Ok(prompt) => {
                let assembled = prompt.assemble(&self.base);
                if assembled.trim().is_empty() {
                    self.notice = Some("empty: nothing to send".to_string());
                    return None;
                }
                Some(assembled)
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                None
            }
        }
    }

    /// The title: the file, then either what just happened or the mark that it changed.
    ///
    /// A notice outranks the dirty mark because it is news — "saved" is more useful for the second
    /// it is on screen than "this file differs from what was written", which stays true either way.
    fn compose_title(&self) -> String {
        let file = self.file_name();
        match &self.notice {
            Some(notice) => {
                let short: String = notice.chars().take(48).collect();
                let ellipsis = if notice.chars().count() > 48 {
                    "…"
                } else {
                    ""
                };
                format!("{file} {short}{ellipsis}")
            }
            None if self.is_dirty() => format!("{file} ●"),
            None => file,
        }
    }

    /// The file name, which is all a title needs.
    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

impl Pane for Editor {
    fn kind(&self) -> PaneKind {
        PaneKind::Editor
    }

    fn title(&self) -> &str {
        // The title carries the file's name and its state, so it cannot be a `'static` string; the
        // cache is refreshed whenever either changes, and this is a borrow of it.
        &self.title_cache
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let styles = theme.styles();
        // The cursor is drawn by the widget through its theme rather than by the terminal, so there
        // is one cursor on screen and it is the one the editor thinks it has. The status line is
        // hidden because this pane's title already says which file it is and whether it changed,
        // and a second line of the same news costs a row of the buffer.
        let editor_theme = EditorTheme::default()
            .base(styles.text)
            .cursor_style(Style {
                fg: Some(theme.palette.bg),
                bg: Some(theme.palette.accent),
                ..Style::default()
            })
            .selection_style(Style::default().bg(theme.palette.border_focused))
            .hide_status_line();
        EditorView::new(&mut self.state)
            .theme(editor_theme)
            .wrap(true)
            .tab_width(self.tab_width)
            .render(area, buf);
    }

    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // Anything that happened before this key is stale: a notice is about the last thing done,
        // and the next thing done replaces it.
        match key.code {
            KeyCode::Char('s') if control => {
                self.notice = match self.save() {
                    Ok(()) => Some("saved".to_string()),
                    Err(error) => Some(format!("save failed: {error}")),
                };
                self.title_cache = self.compose_title();
                return KeyOutcome::Handled;
            }
            KeyCode::Char('g') if control => {
                if let Some(prompt) = self.assembled() {
                    self.outbox.push(prompt);
                    self.notice = Some("sent".to_string());
                }
                self.title_cache = self.compose_title();
                return KeyOutcome::Handled;
            }
            KeyCode::Char('r') if control => {
                // Reload from disk: the escape hatch for a file changed outside the app.
                self.notice = match Self::open(self.path.clone(), self.base.clone()) {
                    Ok(fresh) => {
                        self.state = fresh.state;
                        self.saved = fresh.saved;
                        Some("reloaded".to_string())
                    }
                    Err(error) => Some(format!("reload failed: {error}")),
                };
                self.title_cache = self.compose_title();
                return KeyOutcome::Handled;
            }
            _ => {}
        }
        if self.notice.is_some() {
            self.notice = None;
        }
        self.handler.on_key_event(key, &mut self.state);
        self.title_cache = self.compose_title();
        KeyOutcome::Handled
    }

    fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.outbox)
    }

    fn cursor(&self, area: Rect) -> Option<Position> {
        // The widget wants the whole area and draws the cursor itself through its theme, so the
        // terminal's own cursor stays out of the way.
        let _ = area;
        None
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-editor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        // Vim: `i` enters insert, `Esc` leaves it. The pane is a vim, so the tests type like one.
        editor.on_key(key(KeyCode::Char('i')));
        for character in text.chars() {
            editor.on_key(key(KeyCode::Char(character)));
        }
        editor.on_key(key(KeyCode::Esc));
    }

    fn area() -> Rect {
        Rect::new(0, 0, 40, 8)
    }

    fn screen(editor: &mut Editor) -> String {
        let mut buf = Buffer::empty(area());
        editor.draw(area(), &mut buf, &Theme::default());
        (0..buf.area.height)
            .map(|row| {
                (0..buf.area.width)
                    .map(|column| buf[(column, row)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_missing_file_opens_as_an_empty_buffer_rather_than_an_error() {
        let dir = scratch("missing");
        let editor = Editor::open(dir.join("prompt.md"), &dir).unwrap();
        assert_eq!(editor.text(), "");
        assert!(!editor.is_dirty(), "an empty new file is not a change");
    }

    #[test]
    fn typing_marks_it_dirty_and_saving_writes_it_out() {
        let dir = scratch("save");
        let path = dir.join("prompt.md");
        let mut editor = Editor::open(&path, &dir).unwrap();

        type_text(&mut editor, "hello");
        assert!(editor.is_dirty());
        assert!(!path.exists(), "nothing is written until it is asked for");

        assert_eq!(
            editor.on_key(control(KeyCode::Char('s'))),
            KeyOutcome::Handled
        );
        assert!(!editor.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert_eq!(editor.notice(), Some("saved"));
    }

    #[test]
    fn saving_creates_the_directory_it_needs() {
        let dir = scratch("mkdir");
        let path = dir.join("prompts").join("deep").join("prompt.md");
        let mut editor = Editor::open(&path, &dir).unwrap();
        type_text(&mut editor, "x");
        editor.save().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn sending_assembles_the_prompt_with_the_session_directory_resolved() {
        let dir = scratch("send");
        let path = dir.join("prompt.md");
        std::fs::write(
            &path,
            "---\npath: src/main.rs\ninstruction: 加一个 --version\n---\n\n顺便看看帮助。\n",
        )
        .unwrap();
        let mut editor = Editor::open(&path, &dir).unwrap();

        assert_eq!(
            editor.on_key(control(KeyCode::Char('g'))),
            KeyOutcome::Handled
        );
        let requests = editor.take_requests();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert!(sent.contains("File: "), "{sent}");
        assert!(sent.contains("src/main.rs"), "{sent}");
        assert!(sent.contains("Task: 加一个 --version"), "{sent}");
        assert!(sent.contains("顺便看看帮助。"), "{sent}");
        assert!(editor.take_requests().is_empty(), "drained once");
    }

    #[test]
    fn an_empty_prompt_is_not_sent_and_says_so() {
        let dir = scratch("empty-send");
        let mut editor = Editor::open(dir.join("prompt.md"), &dir).unwrap();
        editor.on_key(control(KeyCode::Char('g')));
        assert!(editor.take_requests().is_empty());
        assert_eq!(editor.notice(), Some("empty: nothing to send"));
    }

    #[test]
    fn an_unclosed_header_is_refused_with_the_reason() {
        let dir = scratch("bad-header");
        let path = dir.join("prompt.md");
        std::fs::write(&path, "---\npath: src/main.rs\n").unwrap();
        let mut editor = Editor::open(&path, &dir).unwrap();

        editor.on_key(control(KeyCode::Char('g')));
        assert!(editor.take_requests().is_empty());
        assert!(
            editor
                .notice()
                .is_some_and(|notice| notice.contains("收尾")),
            "{:?}",
            editor.notice()
        );
    }

    #[test]
    fn ctrl_r_reloads_what_is_on_disk() {
        let dir = scratch("reload");
        let path = dir.join("prompt.md");
        std::fs::write(&path, "first").unwrap();
        let mut editor = Editor::open(&path, &dir).unwrap();
        type_text(&mut editor, " local");
        assert!(editor.is_dirty());

        std::fs::write(&path, "changed elsewhere").unwrap();
        editor.on_key(control(KeyCode::Char('r')));
        assert_eq!(editor.text(), "changed elsewhere");
        assert!(!editor.is_dirty());
        assert_eq!(editor.notice(), Some("reloaded"));
    }

    #[test]
    fn the_notice_clears_on_the_next_keystroke() {
        let dir = scratch("notice");
        let mut editor = Editor::open(dir.join("prompt.md"), &dir).unwrap();
        editor.on_key(control(KeyCode::Char('g')));
        assert!(editor.notice().is_some());
        editor.on_key(key(KeyCode::Char('x')));
        assert_eq!(editor.notice(), None);
    }

    #[test]
    fn the_title_carries_the_file_and_whether_it_changed() {
        let dir = scratch("title");
        let path = dir.join("prompt.md");
        std::fs::write(&path, "x").unwrap();
        let mut editor = Editor::open(&path, &dir).unwrap();
        let _ = screen(&mut editor);
        assert_eq!(editor.title(), "prompt.md");

        type_text(&mut editor, "y");
        let _ = screen(&mut editor);
        assert_eq!(editor.title(), "prompt.md ●", "a change is marked");

        editor.save().unwrap();
        let _ = screen(&mut editor);
        assert_eq!(editor.title(), "prompt.md saved");
    }

    #[test]
    fn the_buffer_is_drawn_where_the_pane_was_given() {
        let dir = scratch("draw");
        let path = dir.join("prompt.md");
        std::fs::write(&path, "instruction: 改超时\n").unwrap();
        let mut editor = Editor::open(&path, &dir).unwrap();
        let screen = screen(&mut editor);
        // Word by word rather than the whole line: the widget puts a space after a wide character,
        // so `改超时` is drawn with gaps between the characters. That is the editor's rendering, not
        // the pane's, and a test that insisted on the exact line would be testing upstream.
        assert!(screen.contains("instruction:"), "{screen}");
        for character in "改超时".chars() {
            assert!(
                screen.contains(character),
                "{character:?} is missing: {screen}"
            );
        }
    }
}
