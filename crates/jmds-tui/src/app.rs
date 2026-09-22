//! The shell: which keys are the app's, what an event turns into, and when to stop.
//!
//! Deliberately **not** the event loop. Owning the terminal — raw mode, the alternate screen, the
//! panic hook that puts it back — is a process-level act that belongs to the binary, and a struct
//! that can be constructed and driven in a test is worth more than one that cannot. So this is the
//! part of the loop that has decisions in it:
//!
//! - [`App::on_key`] — the key policy, below.
//! - [`App::on_agent_event`] — an engine event, handed to the panes.
//! - [`App::draw`] — one frame, delegated to the host.
//!
//! # The key policy
//!
//! Keys arrive in a fixed order, and the order is the whole design:
//!
//! 1. **The app's own keys, first**: `Ctrl+Q` quits, `Ctrl+W` closes the focused pane, `Ctrl+T`
//!    cycles focus, `Alt+1`…`Alt+9` jump to a pane by position.
//! 2. **The focused pane's turn**, which answers [`KeyOutcome::Handled`] or [`KeyOutcome::Ignored`].
//!
//! The order used to be the other way round — the pane first — on the theory that `Tab` means
//! "indent" in an editor, so a pane must be able to keep a key. The editor pane settled it: it is a
//! vim, it consumes *every* key it is given, and with the old order, focus in the editor made pane
//! navigation impossible. Every key in the first set is one no pane can want for editing — quit,
//! close, cycle, jump — so the app takes them and the pane gets everything else.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jmds_core::{
    event::AgentEvent,
    pane::{Axis, SplitUnder},
};
use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    commands,
    pane::{Pane, PaneHost},
    theme::GlyphSet,
};

/// What the loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Quit,
}

/// The panes, and the keys that belong to the app rather than to them.
pub struct App {
    host: PaneHost,
    /// Whether anything has happened since the last frame.
    ///
    /// The app is drawn on a clock, and most ticks have nothing to show: drawing anyway means
    /// re-laying-out the transcript twelve times a second to put the same pixels back. A flag set by
    /// everything that changes something, plus what the panes ask for themselves, is what turns the
    /// clock back into a clock.
    dirty: bool,
    /// The rectangle the last frame was drawn into. A frame whose area changed is worth another one:
    /// the panes only learn their size by being drawn, and a terminal that never saw its new size never
    /// tells the pty either.
    last_area: Rect,
    /// The split line a drag is moving, if a drag is happening.
    ///
    /// Kept here rather than asked for on every motion event: what a drag means is "the line it
    /// started on", and asking again would let a pointer that wandered onto another line start
    /// moving *that* one mid-drag.
    dragging: Option<SplitUnder>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// How many sessions `/resume` lists before it stops. A list longer than the pane is a list nobody
/// reads, and `/resume <id>` is for when the person already knows which one.
const SESSION_LIST_LIMIT: usize = 10;

/// How long ago something started, in words.
///
/// Coarse on purpose: the question a list of conversations answers is "which one was that", and the
/// difference between nineteen minutes and twenty does not change the answer.
fn ago(started_at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0);
    let minutes = now.saturating_sub(started_at_ms) / 60_000;
    match minutes {
        0 => "刚刚".to_string(),
        1..=59 => format!("{minutes} 分钟前"),
        60..=1439 => format!("{} 小时前", minutes / 60),
        _ => format!("{} 天前", minutes / 1440),
    }
}

/// Where a pointer is, as the ratio that puts a split's line under it.
///
/// Taken as-is rather than with the slop `split_at` allows: the slop is there so a press near the
/// line starts a drag, and using it here would move the line the moment the drag began.
fn ratio_at(split: &SplitUnder, column: u16, row: u16) -> f32 {
    let (pointer, start, span) = match split.axis {
        Axis::Horizontal => (column, split.area.x, split.area.width),
        Axis::Vertical => (row, split.area.y, split.area.height),
    };
    if span == 0 {
        return 0.5;
    }
    (pointer.saturating_sub(start) as f32 / span as f32).clamp(0.0, 1.0)
}

/// How far one `Alt+arrow` moves a line. Small enough to be a nudge, big enough to see.
const RESIZE_STEP: f32 = 0.03;

impl App {
    pub fn new() -> Self {
        Self {
            host: PaneHost::new(),
            dragging: None,
            dirty: true,
            last_area: Rect::new(0, 0, 0, 0),
        }
    }

    /// Open a pane beside the focused one.
    pub fn open(&mut self, axis: Axis, pane: impl Pane + 'static) -> jmds_core::pane::PaneId {
        self.host.open(axis, pane)
    }

    pub fn host(&self) -> &PaneHost {
        &self.host
    }

    pub fn host_mut(&mut self) -> &mut PaneHost {
        &mut self.host
    }

    /// Whether there is nothing left to show. Closing the last pane is how the app ends.
    pub fn is_empty(&self) -> bool {
        self.host.is_empty()
    }

    /// The key policy, in the order it is written above.
    /// Whether a frame is worth drawing.
    pub fn wants_frame(&self) -> bool {
        self.dirty || self.host.wants_frame()
    }

    /// Something outside changed that no pane has heard about — the terminal was resized, a file the
    /// app read is gone. The next frame is owed because the last one is out of date.
    pub fn touch(&mut self) {
        self.dirty = true;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        self.dirty = true;
        if let Some(action) = self.app_key(key) {
            return action;
        }
        if self.navigate(key) {
            return Action::Continue;
        }
        // Everything else is the pane's, whether it uses it or not.
        self.host.on_key(key);
        Action::Continue
    }

    /// One mouse event.
    ///
    /// A second way to say what the keys say: a click focuses, and the wheel scrolls the pane it is
    /// over. Neither is the only way to do anything — this is a terminal app, and the keyboard is
    /// the first mouth.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Action {
        self.dirty = true;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A press on a split line starts moving it; anywhere else it is a click, which is how
                // the keyboard is handed over.
                match self.host.split_at(mouse.column, mouse.row) {
                    Some(split) => self.dragging = Some(split),
                    None => {
                        if let Some(id) = self.host.pane_at(mouse.column, mouse.row) {
                            self.host.focus(id);
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(split) = self.dragging {
                    let ratio = ratio_at(&split, mouse.column, mouse.row);
                    self.host.set_ratio(split.pane, ratio);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = None,
            MouseEventKind::ScrollUp => self.scroll_at(&mouse, 1),
            MouseEventKind::ScrollDown => self.scroll_at(&mouse, -1),
            _ => {}
        }
        Action::Continue
    }

    /// Move the focused pane's own split line by `step`, if the key's direction is the way it runs.
    ///
    /// The line moves the way the arrow points, exactly as it does under a drag: the ratio *is* the
    /// line's position, so both hands do the same thing to it. A key whose direction does not match
    /// the line does nothing rather than moving a line the person is not looking at — `Alt+Left` on a
    /// pane split top-and-bottom has nothing to say about it, and inventing something would be a key
    /// that moves an invisible thing.
    fn nudge(&mut self, step: f32, axis: Axis) -> Action {
        let Some(id) = self.host.focused_id() else {
            return Action::Continue;
        };
        let Some(split) = self.host.split_of(id) else {
            return Action::Continue;
        };
        if split.axis == axis {
            self.host.set_ratio(id, split.ratio + step);
        }
        Action::Continue
    }

    /// Send the wheel to the pane under the pointer.
    ///
    /// Aimed, unlike a click: what a click means is "give this pane the keyboard", which is a change
    /// of where the person is, and what a wheel means is "move that", which is about what the
    /// pointer is over.
    fn scroll_at(&mut self, mouse: &MouseEvent, steps: isize) {
        if let Some(id) = self.host.pane_at(mouse.column, mouse.row) {
            self.host.scroll(id, steps);
        }
    }

    /// The keys the app answers itself, and the only place a pane can be closed from.
    fn app_key(&mut self, key: KeyEvent) -> Option<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // `Alt+arrow` moves the focused pane's split line, the way `Alt+digit` jumps between
            // panes: the app's own keys, and ones a pane has no use for.
            KeyCode::Left if alt => Some(self.nudge(-RESIZE_STEP, Axis::Horizontal)),
            KeyCode::Right if alt => Some(self.nudge(RESIZE_STEP, Axis::Horizontal)),
            KeyCode::Up if alt => Some(self.nudge(-RESIZE_STEP, Axis::Vertical)),
            KeyCode::Down if alt => Some(self.nudge(RESIZE_STEP, Axis::Vertical)),
            KeyCode::Char('q') if control => Some(Action::Quit),
            KeyCode::Char('w') if control => {
                match self.host.focused_id() {
                    Some(id) => {
                        self.host.close(id);
                    }
                    None => return Some(Action::Quit),
                }
                // Closing the last pane leaves nothing to draw, so it ends the app. Saying so here
                // rather than refusing the close keeps the two keys' meaning simple.
                Some(if self.host.is_empty() {
                    Action::Quit
                } else {
                    Action::Continue
                })
            }
            _ => None,
        }
    }

    /// The navigation keys, which have no `Action` of their own. Answers whether it used the key.
    fn navigate(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('t') if control => {
                self.host.cycle_focus();
                true
            }
            KeyCode::Char(digit @ '1'..='9') if alt => {
                // Layout order from the tree, not from the last frame's geometry: a key that only
                // works after a redraw is a key that does nothing on the first keystroke, and the
                // geometry is the mouse's business.
                let wanted = digit as usize - '1' as usize;
                if let Some(id) = self.host.tree().leaves().get(wanted) {
                    self.host.focus(*id);
                }
                true
            }
            _ => false,
        }
    }

    /// An event from the engine, for whoever is listening. Nothing here quits: a model that fails,
    /// a tool that errors, a file that changed — none of those are reasons to close the app.
    pub fn on_agent_event(&mut self, event: &AgentEvent) -> Action {
        self.dirty = true;
        self.host.on_agent_event(event);
        Action::Continue
    }

    /// Tell the panes that a file under the project root changed.
    pub fn on_file_event(&mut self, event: &jmds_core::event::FileEvent) {
        self.dirty = true;
        self.host.on_file_event(event);
    }

    /// Hand a pty event to the pane it is about.
    pub fn on_pty_event(&mut self, event: &jmds_core::event::PtyEvent) {
        self.dirty = true;
        self.host.on_pty_event(event);
    }

    /// The engine opened or closed a pane.
    ///
    /// Opening one is the app's act even though the engine asks for it: where a pane goes is a
    /// layout decision, and the engine has no idea what the screen looks like. A terminal asked for
    /// by a running command is placed beside the shell rather than beside whatever happens to have
    /// focus, because a command's output belongs with the other commands.
    pub fn on_pane_event(&mut self, event: &jmds_core::event::PaneEvent) {
        self.dirty = true;
        use jmds_core::{event::PaneEvent, pane::PaneKind};

        match event {
            PaneEvent::Opened { spec } => {
                if self.host.pane(spec.id).is_some() {
                    // Already there: the engine may have published this twice around a restart, and
                    // two panes for one command would be two screens for one screen's worth of output.
                    return;
                }
                if spec.kind != PaneKind::Terminal {
                    log::warn!("不知道该怎么开一个 {:?} 面板", spec.kind);
                    return;
                }
                let pane =
                    crate::pane::terminal::TerminalPane::new(spec.id, spec.title.clone(), (10, 40));
                let beside = self.shell_pane();
                let was = self.host.focused_id();
                if let Some(near) = beside {
                    self.host.focus(near);
                }
                self.host
                    .open_as(spec.id, jmds_core::pane::Axis::Horizontal, pane);
                if let Some(was) = was {
                    self.host.focus(was);
                }
            }
            PaneEvent::Closed { id } => {
                self.host.close(*id);
            }
            // Focus and geometry are the tree's own business, and a title it already has.
            _ => {}
        }
    }

    /// The pane a command's output belongs beside: the one the shell is showing in.
    fn shell_pane(&self) -> Option<jmds_core::pane::PaneId> {
        self.pane_of(jmds_core::pane::PaneKind::Terminal)
    }

    /// The first pane of a kind, in the order they are laid out.
    fn pane_of(&self, kind: jmds_core::pane::PaneKind) -> Option<jmds_core::pane::PaneId> {
        self.host
            .tree()
            .leaves()
            .into_iter()
            .find(|id| self.host.pane(*id).map(|pane| pane.kind()) == Some(kind))
    }

    /// What `/resume` with no argument says: the conversations held here, newest first.
    ///
    /// Ids rather than titles, because a session has no title: what it has is a start time and a model,
    /// and the id is what `/resume` takes back. Only the newest few — a list longer than the pane is a
    /// list nobody reads, and `/resume <id>` is for when the person already knows which one.
    fn list_sessions(&mut self) {
        let here = crate::commands::sessions_here();
        if here.is_empty() {
            self.host.note("这个目录里还没有别的会话");
            return;
        }
        let lines: Vec<String> = here
            .into_iter()
            .take(SESSION_LIST_LIMIT)
            .map(|summary| {
                format!(
                    "{}  {}  {}",
                    summary.id(),
                    summary.header.model,
                    ago(summary.header.started_at_ms)
                )
            })
            .collect();
        self.host
            .note(&format!("/resume <id>\n{}", lines.join("\n")));
    }

    /// Start the prompt file from a saved template.
    ///
    /// The template is written *into* the file the editor is holding rather than opened as a buffer of
    /// its own: what goes out with a turn is that file, and a template that lived somewhere else would
    /// be a second place the prompt can be. A pane with unsaved work refuses, and says so.
    fn load_prompt(&mut self, name: &str) {
        if name.is_empty() {
            self.host.note("usage: /prompt <name>");
            return;
        }
        let path = jmds_core::prompt::prompts_dir().join(format!("{name}.md"));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                self.host.note(&format!("没有这个模板：{name}"));
                return;
            }
        };
        let Some(id) = self.pane_of(jmds_core::pane::PaneKind::Editor) else {
            self.host.note("没有打开的编辑器面板");
            return;
        };
        match self.host.pane_mut(id).map(|pane| pane.load_prompt(&text)) {
            Some(Ok(())) => self.host.note(&format!("已从模板 {name} 起头")),
            Some(Err(why)) => self.host.note(&format!("没有替换：{why}")),
            None => {}
        }
    }

    /// What the panes want said to the engine's processes.
    pub fn take_pty(&mut self) -> Vec<jmds_core::event::PtyEvent> {
        self.host.take_pty()
    }

    /// A frame passed. The loop that owns the clock calls this, which is what animates a pane with
    /// something to show while a turn runs.
    pub fn tick(&mut self) {
        self.host.tick();
    }

    /// What the panes are asking for: what the human typed and pressed Enter on.
    pub fn take_requests(&mut self) -> Vec<String> {
        self.host.take_requests()
    }

    /// Run a submitted line as a command, if that is what it is.
    ///
    /// `None` means the line is not the app's: it is prose, and prose is the model's. A line that
    /// names a command the app does not have is *answered* rather than forwarded, because a typo
    /// turned into a question is a typo the model will answer confidently and wrongly.
    pub fn handle_command(&mut self, line: &str) -> Option<CommandOutcome> {
        self.dirty = true;
        let Some((command, argument)) = commands::parse_command(line) else {
            if let Some(name) = unknown_command_name(line) {
                self.host
                    .note(&format!("no such command: /{name} — /help lists them"));
                return Some(CommandOutcome::Handled);
            }
            return None;
        };
        match command.name {
            "quit" => return Some(CommandOutcome::Quit),
            "help" => self.host.note(HELP),
            "clear" => self.host.clear(),
            "theme" => self.set_theme(argument),
            "prompt" => self.load_prompt(argument),
            "resume" => {
                if argument.is_empty() {
                    self.list_sessions();
                } else {
                    return Some(CommandOutcome::Resume(argument.to_string()));
                }
            }
            "glyphs" => self.set_glyphs(argument),
            _ => {}
        }
        Some(CommandOutcome::Handled)
    }

    fn set_theme(&mut self, argument: &str) {
        if argument.is_empty() {
            self.host.note("usage: /theme <name>");
            return;
        }
        match crate::theme::Theme::named(argument) {
            // The glyph choice is kept: `/theme` is about colours, and having it silently undo
            // `/glyphs` would make the two commands fight over one setting.
            Some(theme) => {
                let set = self.host.theme().glyphs.set;
                self.host.set_theme(theme.with_glyphs(set));
            }
            None => self.host.note(&format!("no such theme: {argument}")),
        }
    }

    fn set_glyphs(&mut self, argument: &str) {
        let set = match argument {
            "unicode" => GlyphSet::Unicode,
            "ascii" => GlyphSet::Ascii,
            "" => {
                self.host.note("usage: /glyphs unicode|ascii");
                return;
            }
            other => {
                self.host
                    .note(&format!("no such glyph set: {other} — unicode or ascii"));
                return;
            }
        };
        let theme = self.host.theme().clone().with_glyphs(set);
        self.host.set_theme(theme);
    }

    /// One frame. Drawing is what clears the flag: a frame is the answer to "something happened".
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        // A frame in a new area owes another one straight away: the panes learn their size by being
        // drawn, so the frame that finds out is the frame that has to be redrawn properly.
        self.dirty = area != self.last_area;
        self.last_area = area;
        self.host.draw(area, buf);
    }
}

/// The name in a line that looks like an attempted command but is not one.
///
/// Narrow on purpose: `/nope` is a typo and deserves an answer, while `/etc/passwd is a file` is a
/// sentence about a path, and answering *that* with "no such command" would be the app mistaking
/// prose for a request.
fn unknown_command_name(line: &str) -> Option<&str> {
    let name = line
        .trim_start()
        .strip_prefix('/')?
        .split_whitespace()
        .next()
        .unwrap_or("");
    let command_shaped = name.chars().next().is_some()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    command_shaped.then_some(name)
}

/// What a command line meant, once the app has run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The app did it. Nothing goes to the model.
    Handled,
    /// The line asked the app to stop.
    Quit,
    /// The line asked for another conversation. The app cannot switch one itself — the history has one
    /// owner, and it is not this — so it hands the id back for whoever does own it.
    Resume(String),
}

/// What `/help` says. Short, because a help screen nobody reads is a help screen that does not
/// exist; the thing worth putting here is the input line, which is where the commands are found.
const HELP: &str = "\
/help                    this
/clear                   empty the transcript; the session file stays
/theme <name>            switch colours
/resume [<id>]           continue another conversation held here; alone, list them
/prompt <name>           start the prompt file from a saved template
/glyphs unicode|ascii    which glyph set to draw with
/quit                    leave

In the input line: `/` completes a command, `@` completes a file, `Tab` takes the completion,
`\u{2191}`/`\u{2193}` choose between them, `Esc` closes them, `Ctrl+Q` quits.
A command's own values complete too: `/theme dr` offers `dracula`.
With the mouse: a click focuses a pane, the wheel scrolls it, dragging a split line moves it.
`Alt+\u{2190}`/`\u{2192}`/`\u{2191}`/`\u{2193}` move that line from the keyboard.";

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use crossterm::event::KeyModifiers;
    use jmds_core::{
        event::FinishReason,
        pane::{PaneId, PaneKind},
    };

    use crate::pane::KeyOutcome;

    use super::*;

    /// The keys one pane was given, shared with the pane so the test can read them afterwards.
    type Recorded = Rc<RefCell<Vec<KeyCode>>>;

    /// What the app said to a pane: notes it was given, how often it was cleared, and how many file
    /// events reached it.
    #[derive(Debug, Default)]
    struct Log {
        notes: Vec<String>,
        clears: usize,
        files: usize,
        scrolled: isize,
    }

    /// A pane whose answers the test chooses, and which records what it was given.
    struct Recorder {
        handles: bool,
        keys: Recorded,
        events: Rc<RefCell<usize>>,
        log: Rc<RefCell<Log>>,
    }

    impl Recorder {
        fn new(handles: bool) -> (Self, Recorded) {
            Self::with_log(handles, Rc::new(RefCell::new(Log::default())))
        }

        /// A recorder whose notes and clears the test can read afterwards.
        fn with_log(handles: bool, log: Rc<RefCell<Log>>) -> (Self, Recorded) {
            let keys = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    handles,
                    keys: keys.clone(),
                    events: Rc::new(RefCell::new(0)),
                    log,
                },
                keys,
            )
        }
    }

    impl Pane for Recorder {
        fn kind(&self) -> PaneKind {
            PaneKind::Chat
        }

        fn title(&self) -> &str {
            "recorder"
        }

        fn draw(&mut self, _area: Rect, _buf: &mut Buffer, _theme: &crate::theme::Theme) {}

        fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
            self.keys.borrow_mut().push(key.code);
            if self.handles {
                KeyOutcome::Handled
            } else {
                KeyOutcome::Ignored
            }
        }

        fn on_agent_event(&mut self, _event: &AgentEvent) {
            *self.events.borrow_mut() += 1;
        }

        fn note(&mut self, text: &str) {
            self.log.borrow_mut().notes.push(text.to_string());
        }

        fn on_file_event(&mut self, _event: &jmds_core::event::FileEvent) {
            self.log.borrow_mut().files += 1;
        }

        fn on_scroll(&mut self, steps: isize, _height: u16) {
            self.log.borrow_mut().scrolled += steps;
        }

        fn clear(&mut self) {
            self.log.borrow_mut().clears += 1;
        }
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn dragging_a_split_line_moves_it_and_a_release_lets_it_go() {
        let (mut app, _) = app_with_three_logs();
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let (_, second) = app.host().geometry()[1];
        let (line, row) = (second.x, second.y + 1);

        // A press on the line grabs it; anywhere else it would be a click, which is how the keyboard
        // is handed over.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), line, row));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 45, row));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 45, row));
        app.draw(area, &mut buffer);
        assert_eq!(app.host().geometry()[0].1.width, 45, "线跟着指针走到了 45");

        // After the release the line is nobody's: dragging on does nothing.
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 10, row));
        app.draw(area, &mut buffer);
        assert_eq!(app.host().geometry()[0].1.width, 45);
    }

    #[test]
    fn alt_arrows_move_the_focused_panes_own_line() {
        let (mut app, _) = app_with_three_logs();
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        let focused = app.host().focused_id().expect("有个焦点");
        let size = |app: &App| {
            app.host()
                .geometry()
                .iter()
                .find(|(id, _)| *id == focused)
                .map(|(_, rect)| (rect.width, rect.height))
                .expect("每个面板都有自己的矩形")
        };

        let (width, height) = size(&app);
        // The line moves the way the arrow points, so a pane on the left of it grows and a pane on
        // the right shrinks — the same thing a drag does.
        assert!(!app.host().geometry().is_empty());
        app.on_key(alt(KeyCode::Right));
        app.draw(area, &mut buffer);
        let (moved, taller) = size(&app);
        assert_ne!(moved, width, "Alt+→ 该动那条线：{width} -> {moved}");
        assert_eq!(taller, height, "竖着的方向不碰横着的那条线");

        // An arrow across the line does nothing, rather than moving a line the person is not looking
        // at: this pane's line runs left-to-right, so up-and-down has nothing to say about it.
        app.on_key(alt(KeyCode::Down));
        app.draw(area, &mut buffer);
        assert_eq!(size(&app), (moved, taller), "跨方向的箭头什么都不做");
        app.on_key(alt(KeyCode::Left));
        app.draw(area, &mut buffer);
        assert_eq!(size(&app).0, width, "Alt+← 把它放回去");
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    /// The app with three panes that ignore everything, and the recorders watching them.
    fn three_panes() -> (App, Vec<PaneId>, Vec<Recorded>) {
        let mut app = App::new();
        let mut ids = Vec::new();
        let mut keys = Vec::new();
        for _ in 0..3 {
            let (pane, recorded) = Recorder::new(false);
            ids.push(app.open(Axis::Horizontal, pane));
            keys.push(recorded);
        }
        (app, ids, keys)
    }

    #[test]
    fn ctrl_q_quits_even_though_the_pane_would_have_taken_it() {
        // A pane that handles everything must not be able to trap the user.
        let mut app = App::new();
        let (pane, keys) = Recorder::new(true);
        app.open(Axis::Horizontal, pane);

        assert_eq!(app.on_key(control(KeyCode::Char('q'))), Action::Quit);
        assert!(keys.borrow().is_empty(), "the pane never saw it");
    }

    /// An app with one recorder pane, and the log it writes to.
    fn app_with_recorder() -> (App, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut app = App::new();
        app.open(Axis::Horizontal, Recorder::with_log(false, log.clone()).0);
        (app, log)
    }

    #[test]
    fn a_file_event_reaches_every_pane_not_only_the_one_in_focus() {
        let mut app = App::new();
        let mut logs = Vec::new();
        for _ in 0..3 {
            let log = Rc::new(RefCell::new(Log::default()));
            app.open(Axis::Horizontal, Recorder::with_log(false, log.clone()).0);
            logs.push(log);
        }
        assert_eq!(app.host().focused_id(), Some(app.host().tree().leaves()[2]));

        app.on_file_event(&jmds_core::event::FileEvent::Changed {
            path: "/work/a.txt".into(),
        });
        for (index, log) in logs.iter().enumerate() {
            assert_eq!(log.borrow().files, 1, "第 {} 个 pane 也该被告知", index + 1);
        }
    }

    #[test]
    fn a_terminal_asked_for_by_the_engine_appears_beside_the_shell() {
        use jmds_core::{
            event::PaneEvent,
            pane::{PaneId, PaneSpec},
        };

        let mut app = App::new();
        let shell = PaneId::fresh();
        app.open(
            Axis::Horizontal,
            crate::pane::terminal::TerminalPane::new(shell, "sh", (10, 40)),
        );
        let chat = app.open(Axis::Horizontal, Recorder::new(false).0);

        let tool = PaneId::fresh();
        app.on_pane_event(&PaneEvent::Opened {
            spec: PaneSpec::new(tool, jmds_core::pane::PaneKind::Terminal).with_title("cargo test"),
        });

        assert_eq!(
            app.host().pane(tool).map(|pane| pane.title().to_string()),
            Some("cargo test".to_string())
        );
        assert_eq!(
            app.host().focused_id(),
            Some(chat),
            "焦点不该被一个工具面板抢走"
        );
        // The engine may say it twice around a restart, and two panes for one command would be two
        // screens for one screen's worth of output.
        app.on_pane_event(&PaneEvent::Opened {
            spec: PaneSpec::new(tool, jmds_core::pane::PaneKind::Terminal).with_title("cargo test"),
        });
        assert_eq!(app.host().len(), 3, "只该有一个工具面板");

        app.on_pane_event(&PaneEvent::Closed { id: tool });
        assert!(app.host().pane(tool).is_none());
    }

    #[test]
    fn closing_a_terminal_pane_tells_the_engine_to_stop_its_command() {
        use jmds_core::{event::PtyEvent, pane::PaneId};

        let mut app = App::new();
        let shell = PaneId::fresh();
        // Opened under the id the engine will name it by, which is how a pane a command runs in
        // gets its name: `open` would mint one of its own and no event would ever match it.
        app.host_mut().open_as(
            shell,
            Axis::Horizontal,
            crate::pane::terminal::TerminalPane::new(shell, "sh", (10, 40)),
        );
        assert!(app.host_mut().close(shell));
        assert_eq!(app.take_pty(), vec![PtyEvent::Kill { id: shell }]);
        assert!(app.take_pty().is_empty(), "交出去的就是交出去了");

        // A pane with no command behind it closes quietly: there is no process to tell.
        let chat = app.open(Axis::Horizontal, Recorder::new(false).0);
        assert!(app.host_mut().close(chat));
        assert!(app.take_pty().is_empty());
    }

    /// An app with three recorder panes, and the log each of them writes to.
    fn app_with_three_logs() -> (App, Vec<Rc<RefCell<Log>>>) {
        let mut app = App::new();
        let mut logs = Vec::new();
        for _ in 0..3 {
            let log = Rc::new(RefCell::new(Log::default()));
            app.open(Axis::Horizontal, Recorder::with_log(false, log.clone()).0);
            logs.push(log);
        }
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        // A frame first: the mouse is hit-tested against what was last drawn, so there is nothing to
        // click on until something has been.
        app.draw(area, &mut buffer);
        (app, logs)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn inside(app: &App, index: usize) -> (PaneId, u16, u16) {
        let (id, rect) = app.host().geometry()[index];
        (id, rect.x + 1, rect.y + 1)
    }

    #[test]
    fn a_frame_is_drawn_only_when_something_happened() {
        let (mut app, _) = app_with_three_logs();
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        // The first frame in a new area owes a second one: the panes learn their size by being drawn.
        app.draw(area, &mut buffer);
        app.draw(area, &mut buffer);
        assert!(!app.wants_frame(), "没事的时候不该画");

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.wants_frame(), "按键之后要画");
        app.draw(area, &mut buffer);
        assert!(!app.wants_frame());

        app.on_agent_event(&AgentEvent::TurnFinished {
            reason: jmds_core::event::FinishReason::Stop,
        });
        assert!(app.wants_frame(), "引擎说了话之后要画");

        // A resize is something no pane has heard about, so the frame is owed.
        app.draw(area, &mut buffer);
        app.touch();
        assert!(app.wants_frame());
    }

    #[test]
    fn a_click_gives_the_pane_under_the_pointer_the_keyboard() {
        let (mut app, _) = app_with_three_logs();
        let (target, column, row) = inside(&app, 1);
        assert_ne!(app.host().focused_id(), Some(target), "先是别的面板");

        assert_eq!(
            app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row)),
            Action::Continue
        );
        assert_eq!(app.host().focused_id(), Some(target));
    }

    #[test]
    fn the_wheel_moves_the_pane_under_the_pointer_not_the_focused_one() {
        let (mut app, logs) = app_with_three_logs();
        let (_, column, row) = inside(&app, 0);
        let focused = app.host().focused_id();

        app.on_mouse(mouse(MouseEventKind::ScrollUp, column, row));
        assert_eq!(logs[0].borrow().scrolled, 1, "指针下面那个该动");
        for (index, log) in logs.iter().enumerate().skip(1) {
            assert_eq!(log.borrow().scrolled, 0, "第 {} 个面板不该动", index + 1);
        }
        assert_eq!(app.host().focused_id(), focused, "滚轮不改焦点");
    }

    #[test]
    fn the_wheel_over_nothing_moves_nothing() {
        let (mut app, logs) = app_with_three_logs();
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 500, 500));
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 500, 500));
        for (index, log) in logs.iter().enumerate() {
            assert_eq!(log.borrow().scrolled, 0, "第 {} 个面板不该动", index + 1);
        }
    }

    #[test]
    fn resume_hands_the_id_back_to_whoever_owns_the_history() {
        let (mut app, _) = app_with_recorder();
        // The app cannot switch a conversation: the history has one owner and it is not this one, so
        // the id goes back out through the outcome the loop reads.
        assert_eq!(
            app.handle_command("/resume 700-0"),
            Some(CommandOutcome::Resume("700-0".to_string()))
        );
        // With no id it answers instead of handing anything over: a list, or that there is nothing to
        // list here.
        assert_eq!(app.handle_command("/resume"), Some(CommandOutcome::Handled));
    }

    #[test]
    fn a_command_is_run_by_the_app_and_never_forwarded() {
        let (mut app, log) = app_with_recorder();
        assert_eq!(app.handle_command("/clear"), Some(CommandOutcome::Handled));
        assert_eq!(log.borrow().clears, 1);
        assert!(
            app.take_requests().is_empty(),
            "the pane asked for nothing: the app answered it"
        );
    }

    #[test]
    fn quit_is_an_outcome_not_a_prompt() {
        let (mut app, _) = app_with_recorder();
        assert_eq!(app.handle_command("/quit"), Some(CommandOutcome::Quit));
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn an_unrecognised_command_is_answered_rather_than_asked() {
        let (mut app, log) = app_with_recorder();
        assert_eq!(app.handle_command("/nope"), Some(CommandOutcome::Handled));
        assert!(
            log.borrow()
                .notes
                .iter()
                .any(|note| note.contains("no such command")),
            "{:?}",
            log.borrow().notes
        );
    }

    #[test]
    fn prose_that_starts_with_a_slash_is_still_prose() {
        let (mut app, _) = app_with_recorder();
        // The model's business: answering this with "no such command" would be the app mistaking a
        // sentence about a path for a request.
        assert_eq!(app.handle_command("/etc/passwd is just a file"), None);
        assert_eq!(app.handle_command("what is 2+2?"), None);
    }

    #[test]
    fn every_command_is_in_the_help_text() {
        // The table and the help text are two places the same vocabulary is written down, and a
        // command nobody can find in `/help` is a command that may as well not exist.
        for command in crate::commands::COMMANDS {
            assert!(HELP.contains(command.name), "/help 里没有 {}", command.name);
        }
    }

    #[test]
    fn help_explains_the_line_the_person_is_typing_in() {
        let (mut app, log) = app_with_recorder();
        assert_eq!(app.handle_command("/help"), Some(CommandOutcome::Handled));
        assert!(
            log.borrow()
                .notes
                .iter()
                .any(|note| note.contains("completes a command")),
            "{:?}",
            log.borrow().notes
        );
    }

    #[test]
    fn the_theme_and_the_glyph_set_do_not_fight_over_one_setting() {
        let (mut app, log) = app_with_recorder();
        app.handle_command("/glyphs ascii");
        assert_eq!(app.host().theme().glyphs.set, crate::theme::GlyphSet::Ascii);
        // A theme brings colours; the glyph choice is the person's and stays.
        app.handle_command("/theme terminal");
        assert_eq!(app.host().theme().glyphs.set, crate::theme::GlyphSet::Ascii);

        app.handle_command("/glyphs katakana");
        assert_eq!(app.host().theme().glyphs.set, crate::theme::GlyphSet::Ascii);
        assert!(
            log.borrow()
                .notes
                .iter()
                .any(|note| note.contains("no such glyph set")),
            "{:?}",
            log.borrow().notes
        );
    }

    #[test]
    fn ctrl_w_closes_the_focused_pane_and_focus_returns() {
        let (mut app, ids, _) = three_panes();
        assert_eq!(app.host().focused_id(), Some(ids[2]));

        assert_eq!(app.on_key(control(KeyCode::Char('w'))), Action::Continue);
        assert_eq!(app.host().len(), 2);
        assert!(app.host().pane(ids[2]).is_none());
        assert_eq!(app.host().focused_id(), Some(ids[1]));
    }

    #[test]
    fn closing_the_last_pane_ends_the_app() {
        let mut app = App::new();
        app.open(Axis::Horizontal, Recorder::new(false).0);
        let second = app.open(Axis::Horizontal, Recorder::new(false).0);
        assert!(!app.is_empty());

        assert_eq!(
            app.on_key(control(KeyCode::Char('w'))),
            Action::Continue,
            "one pane left: still something to look at"
        );
        assert_eq!(app.host().focused_id(), Some(app.host().tree().leaves()[0]));
        let _ = second;

        assert_eq!(
            app.on_key(control(KeyCode::Char('w'))),
            Action::Quit,
            "nothing left to show"
        );
        assert!(app.is_empty());
    }

    #[test]
    fn the_apps_keys_never_reach_a_pane() {
        // The editor pane is a vim: it consumes every key it is given. That is why the app's keys
        // go first — with the old order, pane navigation died the moment focus landed in the editor.
        let mut app = App::new();
        let (greedy, keys) = Recorder::new(true);
        let first = app.open(Axis::Horizontal, greedy);
        let second = app.open(Axis::Horizontal, Recorder::new(true).0);

        for key in [
            control(KeyCode::Char('q')),
            control(KeyCode::Char('w')),
            control(KeyCode::Char('t')),
            alt(KeyCode::Char('1')),
        ] {
            // `Ctrl+Q` would quit and `Ctrl+W` would close, so they are not sent here; the point of
            // this test is that the pane sees none of it.
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('w')) {
                continue;
            }
            app.on_key(key);
        }
        assert!(keys.borrow().is_empty(), "the pane saw {:?}", keys.borrow());

        // And they did what they say: `Ctrl+T` moved focus, `Alt+1` jumped back.
        assert_eq!(app.host().focused_id(), Some(first));
        assert!(app.host().pane(second).is_some());
        assert_eq!(app.on_key(control(KeyCode::Char('q'))), Action::Quit);
        assert!(keys.borrow().is_empty(), "not even the quit key");
    }

    #[test]
    fn alt_digit_jumps_to_a_pane_by_position() {
        let (mut app, ids, _) = three_panes();
        assert_eq!(app.host().focused_id(), Some(ids[2]));

        // Position is layout order, which is what the geometry was drawn in.
        app.host_mut().focus(ids[0]);
        app.on_key(alt(KeyCode::Char('2')));
        assert_eq!(app.host().focused_id(), Some(ids[1]));

        app.on_key(alt(KeyCode::Char('3')));
        assert_eq!(app.host().focused_id(), Some(ids[2]));

        // A digit with no pane behind it changes nothing.
        app.on_key(alt(KeyCode::Char('9')));
        assert_eq!(app.host().focused_id(), Some(ids[2]));
    }

    #[test]
    fn an_engine_event_reaches_the_panes_and_is_not_a_reason_to_quit() {
        let (mut app, _, _) = three_panes();
        // A delta, a turn ending, and a failure: none of them closes the app.
        for event in [
            AgentEvent::Content("hi".into()),
            AgentEvent::TurnFinished {
                reason: FinishReason::Stop,
            },
            AgentEvent::Error("upstream said no".into()),
        ] {
            assert_eq!(app.on_agent_event(&event), Action::Continue);
        }
    }

    #[test]
    fn plain_keys_are_the_panes_business() {
        // No modifier: the app has no opinion, and the focused pane is told.
        let (mut app, ids, keys) = three_panes();
        assert!(app.host_mut().focus(ids[0]));

        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            keys[0].borrow().as_slice(),
            &[KeyCode::Char('j'), KeyCode::Enter]
        );
        assert!(keys[1].borrow().is_empty() && keys[2].borrow().is_empty());
        assert_eq!(app.host().focused_id(), Some(ids[0]), "nothing moved focus");
    }
}
